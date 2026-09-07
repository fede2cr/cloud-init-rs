//! Dumps Hyper-V KVP pool records the way `HyperVKvpReportingHandler` builds
//! them, for differential testing.
//!
//! Reads one JSON request on stdin:
//!
//! ```json
//! {"op": "item",      "key": "...", "value": "..."}
//! {"op": "write_key", "key": "...", "value": "..."}
//! {"op": "event",     "name": "...", "type": "...", "timestamp": 0.0,
//!                     "description": "...", "result": "SUCCESS",
//!                     "duration": 1.5}
//! ```
//!
//! `result` and `duration` are optional; leaving them out is a start event.
//! Each record is printed as its masked key on one line and the hex of its
//! 2048 value bytes on the next. Hex because a record truncated mid-codepoint
//! is not valid UTF-8 — reproducing that is half the point.

use std::io::Read as _;

use ci_report::events::{Event, EventType, Status};
use ci_report::kvp::{self, HyperVKvpHandler, MAX_KEY_SIZE};
use serde_json::Value;

/// Fixed so the two implementations agree; upstream derives it from the boot
/// time, which moves between two runs of the same case.
const INCARNATION: i64 = 1_700_000_000;
const VM_ID: &str = "11111111-2222-3333-4444-555555555555";

fn main() -> std::process::ExitCode {
    let mut input = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("read: {error}");
        return std::process::ExitCode::FAILURE;
    }
    let request: Value = match serde_json::from_str(&input) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("parse: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    let mut log = ci_log::Logger::silent();
    let pool = std::env::temp_dir().join(format!("ci-dump-kvp.{}", std::process::id()));
    let mut handler = HyperVKvpHandler::new(&pool, None, &mut log);
    handler.set_incarnation_no(INCARNATION);
    handler.set_vm_id(VM_ID);

    let key = request.get("key").and_then(Value::as_str).unwrap_or("");
    let value = request.get("value").and_then(Value::as_str).unwrap_or("");
    let records = match request.get("op").and_then(Value::as_str) {
        Some("item") => vec![kvp::encode_item(key, value)],
        Some("write_key") => {
            handler.write_key(key, value, &mut log);
            read_pool(&pool)
        }
        Some("event") => handler.encode_event(&event_from(&request)),
        other => {
            eprintln!("unknown op: {other:?}");
            let _ = std::fs::remove_file(&pool);
            return std::process::ExitCode::FAILURE;
        }
    };
    let _ = std::fs::remove_file(&pool);

    for record in &records {
        let (key, value) = record.split_at(MAX_KEY_SIZE);
        println!("{}", mask(&String::from_utf8_lossy(trim_nuls(key))));
        println!("{}", hex(value));
    }
    std::process::ExitCode::SUCCESS
}

/// Builds the event `_encode_event` reads. A missing `result` or `duration` is
/// a field upstream's `hasattr` check does not find, so it is left out.
fn event_from(request: &Value) -> Event {
    Event {
        event_type: if request.get("type").and_then(Value::as_str) == Some("start") {
            EventType::Start
        } else {
            EventType::Finish
        },
        name: request
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        description: request
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        result: request
            .get("result")
            .and_then(Value::as_str)
            .map(|text| match text {
                "WARN" => Status::Warn,
                "FAIL" => Status::Fail,
                _ => Status::Success,
            }),
        duration: request.get("duration").and_then(Value::as_f64),
        timestamp: request
            .get("timestamp")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
    }
}

/// The last uuid in an event key is `uuid.uuid4()`, which is different on every
/// call. It sits at the end, optionally followed by `|<slice index>`; the vm id
/// is the same shape but never last, so anchoring keeps it visible.
fn mask(key: &str) -> String {
    let (head, tail) = match key.rsplit_once('|') {
        Some((head, tail))
            if tail.chars().all(|c| c.is_ascii_digit()) && !tail.is_empty() =>
        {
            (head, format!("|{tail}"))
        }
        _ => (key, String::new()),
    };
    match head.rsplit_once('|') {
        Some((prefix, last)) if is_uuid(last) => format!("{prefix}|<uuid>{tail}"),
        _ => key.to_owned(),
    }
}

fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.chars().enumerate().all(|(index, c)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn trim_nuls(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |i| i + 1);
    bytes.get(..end).unwrap_or(bytes)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn read_pool(path: &std::path::Path) -> Vec<Vec<u8>> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    bytes.chunks(kvp::RECORD_SIZE).map(<[u8]>::to_vec).collect()
}
