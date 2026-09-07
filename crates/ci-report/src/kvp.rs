//! The Hyper-V key-value-pair pool: how a guest tells the Azure host anything.
//!
//! Port of `HyperVKvpReportingHandler` from `cloudinit/reporting/handlers.py`.
//! The host enumerates `/var/lib/hyperv/.kvp_pool_1` through `hv_kvp_daemon`,
//! so a provisioning failure written here is the only channel that survives a
//! guest with no working network. Without it a failed boot is silent.
//!
//! The pool file is a flat array of fixed 2560-byte records: a 512-byte
//! NUL-padded key followed by a 2048-byte NUL-padded value.

use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use ci_core::time;
use ci_core::uuid::Uuid;
use ci_log::Logger;
use serde_json::{json, Map, Value};

use crate::events::Event;

/// `HV_KVP_EXCHANGE_MAX_KEY_SIZE`.
pub const MAX_KEY_SIZE: usize = 512;
/// `HV_KVP_EXCHANGE_MAX_VALUE_SIZE`.
pub const MAX_VALUE_SIZE: usize = 2048;
/// `HV_KVP_AZURE_MAX_VALUE_SIZE` — what the host will actually read back.
pub const AZURE_MAX_VALUE_SIZE: usize = 1024;
/// `HV_KVP_RECORD_SIZE`.
pub const RECORD_SIZE: usize = MAX_KEY_SIZE + MAX_VALUE_SIZE;

const EVENT_PREFIX: &str = "CLOUD_INIT";
const MSG_KEY: &str = "msg";
const DESC_IDX_KEY: &str = "msg_i";
const MESSAGE_PLACE_HOLDER: &str = "\"msg\":\"\"";

/// `ZERO_GUID`: what the handler reports until something tells it the vm id.
pub const ZERO_GUID: &str = "00000000-0000-0000-0000-000000000000";

/// `KVP_POOL_FILE_GUEST`.
pub const POOL_FILE_GUEST: &str = "/var/lib/hyperv/.kvp_pool_1";

/// `_already_truncated_pool_file`, which upstream keeps as a class attribute.
///
/// Truncating twice would drop key-value pairs the host has not read yet, so
/// this happens at most once per process however many handlers are built.
static ALREADY_TRUNCATED: AtomicBool = AtomicBool::new(false);

/// `_encode_kvp_item`: `struct.pack("512s2048s", key, value)`.
///
/// `struct.pack` silently truncates an oversized field to the declared width,
/// and it does so in bytes while every caller has budgeted in characters. A
/// multi-byte key or value therefore lands in the pool cut mid-codepoint. That
/// is upstream's behaviour and the port reproduces it (COMPAT.md B46).
#[must_use]
pub fn encode_item(key: &str, value: &str) -> Vec<u8> {
    let mut record = vec![0u8; RECORD_SIZE];
    pack(&mut record, 0, MAX_KEY_SIZE, key);
    pack(&mut record, MAX_KEY_SIZE, MAX_VALUE_SIZE, value);
    record
}

fn pack(record: &mut [u8], offset: usize, width: usize, text: &str) {
    let bytes = text.as_bytes();
    let len = bytes.len().min(width);
    if let (Some(field), Some(source)) =
        (record.get_mut(offset..offset + len), bytes.get(..len))
    {
        field.copy_from_slice(source);
    }
}

/// A registered `hyperv` reporting handler.
///
/// Upstream hands events to a daemon thread that drains a queue and appends one
/// batch per wakeup. The port writes from the publishing thread instead: there
/// is no queue to drain, so the batch is always the single event in hand
/// (COMPAT.md deviation 134).
#[derive(Debug)]
pub struct HyperVKvpHandler {
    path: PathBuf,
    event_types: Option<Vec<String>>,
    incarnation_no: i64,
    vm_id: String,
}

impl HyperVKvpHandler {
    /// Building the handler truncates a stale pool file, exactly as upstream's
    /// constructor does, so the host is not shown last boot's telemetry.
    pub fn new(
        path: &Path,
        event_types: Option<Vec<String>>,
        logger: &mut Logger,
    ) -> Self {
        truncate_guest_pool_file(path, logger);
        Self {
            path: path.to_owned(),
            event_types,
            incarnation_no: incarnation_no(logger),
            vm_id: ZERO_GUID.to_owned(),
        }
    }

    #[must_use]
    pub fn vm_id(&self) -> &str {
        &self.vm_id
    }

    /// The Azure datasource sets this once it has resolved the vm id; every
    /// event key written afterwards carries it.
    pub fn set_vm_id(&mut self, vm_id: &str) {
        vm_id.clone_into(&mut self.vm_id);
    }

    /// Whether the vm id is still the placeholder, so a caller that can reach
    /// DMI knows to supply the `system-uuid` fallback upstream's `vm_id`
    /// property reads for itself.
    #[must_use]
    pub fn vm_id_is_unset(&self) -> bool {
        self.vm_id == ZERO_GUID
    }

    /// Pins the incarnation number, which is otherwise this boot's start time
    /// and so differs between two runs. Only the differential harness has any
    /// business calling this.
    #[doc(hidden)]
    pub fn set_incarnation_no(&mut self, incarnation_no: i64) {
        self.incarnation_no = incarnation_no;
    }

    /// `write_key`. The value is cut to 1023 *characters* first, which is where
    /// the byte-width truncation in [`encode_item`] gets its material.
    pub fn write_key(&mut self, key: &str, value: &str, logger: &mut Logger) {
        let value = if value.chars().count() >= AZURE_MAX_VALUE_SIZE {
            value.chars().take(AZURE_MAX_VALUE_SIZE - 1).collect()
        } else {
            value.to_owned()
        };
        if self.append(&[encode_item(key, &value)]).is_err() {
            logger.warning(
                "handlers.py",
                &format!("failed posting kvp={key} value={value}"),
            );
        }
    }

    /// `_event_key`: `CLOUD_INIT|<incarnation>|<type>|<name>|<vm_id>|<uuid4>`.
    #[must_use]
    fn event_key(&self, event: &Event) -> String {
        format!(
            "{EVENT_PREFIX}|{}|{}|{}|{}|{}",
            self.incarnation_no,
            event.event_type.as_str(),
            event.name,
            self.vm_id,
            Uuid::v4()
        )
    }

    /// `_encode_event`, split into one record or, if the metadata will not fit
    /// in what the host reads, a run of numbered slices.
    #[must_use]
    pub fn encode_event(&self, event: &Event) -> Vec<Vec<u8>> {
        let key = self.event_key(event);
        let mut meta = Map::new();
        meta.insert("name".to_owned(), json!(event.name));
        meta.insert("type".to_owned(), json!(event.event_type.as_str()));
        meta.insert(
            "ts".to_owned(),
            json!(time::format_python_isoformat_utc(event.timestamp)),
        );
        if let Some(result) = event.result {
            meta.insert("result".to_owned(), json!(result.as_str()));
        }
        if let Some(duration) = event.duration {
            meta.insert("duration".to_owned(), json!(duration));
        }
        meta.insert(MSG_KEY.to_owned(), json!(event.description));

        let value = ci_core::jsonfmt::dumps_compact(&Value::Object(meta.clone()));
        if value.chars().count() > AZURE_MAX_VALUE_SIZE {
            break_down(&key, &mut meta, &event.description)
        } else {
            vec![encode_item(&key, &value)]
        }
    }

    /// `_append_kvp_item`.
    ///
    /// Upstream takes an advisory `flock` and then writes one record at a time.
    /// `unsafe_code` is forbidden here so `flock(2)` is out of reach; the whole
    /// batch goes out in a single `write(2)` on an `O_APPEND` descriptor
    /// instead, which is atomic against other appenders rather than merely
    /// serialised against other cloud-init processes (COMPAT.md deviation 135).
    fn append(&self, records: &[Vec<u8>]) -> io::Result<()> {
        let mut buffer = Vec::with_capacity(records.len() * RECORD_SIZE);
        for record in records {
            buffer.extend_from_slice(record);
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(&buffer)?;
        file.flush()
    }
}

impl crate::handlers::Handler for HyperVKvpHandler {
    fn publish(&mut self, event: &Event, logger: &mut Logger) {
        if let Some(types) = &self.event_types {
            if !types.iter().any(|kind| kind == event.event_type.as_str()) {
                return;
            }
        }
        if let Err(why) = self.append(&self.encode_event(event)) {
            logger.warning(
                "handlers.py",
                &format!("failed posting events to kvp, {why}"),
            );
        }
    }

    fn as_kvp(&mut self) -> Option<&mut HyperVKvpHandler> {
        Some(self)
    }
}

/// `_break_down`: chop the description across as many records as it takes.
///
/// Deleting and re-adding `msg` moves it behind `msg_i`, and the slice is taken
/// over the *escaped* JSON text, so a cut can land inside a `\n` or `\uXXXX`
/// and leave a record that will not parse. Both are upstream's (COMPAT.md B79).
fn break_down(
    key: &str,
    meta: &mut Map<String, Value>,
    description: &str,
) -> Vec<Vec<u8>> {
    meta.remove(MSG_KEY);
    let quoted = ci_core::jsonfmt::quote_string(description);
    let mut remaining: Vec<char> = quoted
        .chars()
        .skip(1)
        .take(quoted.chars().count().saturating_sub(2))
        .collect();

    let mut records = Vec::new();
    for index in 0i64.. {
        meta.insert(DESC_IDX_KEY.to_owned(), json!(index));
        meta.insert(MSG_KEY.to_owned(), json!(""));
        let without_desc =
            ci_core::jsonfmt::dumps_compact(&Value::Object(meta.clone()));

        let room = room_for_desc(without_desc.chars().count());
        let (head, tail) = split_for_room(&remaining, room);
        let value = without_desc
            .replace(MESSAGE_PLACE_HOLDER, &format!("\"{MSG_KEY}\":\"{head}\""));
        records.push(encode_item(&format!("{key}|{index}"), &value));

        // Upstream loops until the remainder is empty. When the metadata alone
        // overruns the value budget the slice makes no progress and upstream
        // spins forever; the port stops after the record it did produce
        // (COMPAT.md B79).
        if tail.len() >= remaining.len() || tail.is_empty() {
            break;
        }
        remaining = tail;
    }
    records
}

/// `HV_KVP_AZURE_MAX_VALUE_SIZE - len(data_without_desc) - 8`, which upstream
/// lets go negative and then uses as a Python end-relative slice bound.
fn room_for_desc(without_desc: usize) -> i64 {
    i64::try_from(AZURE_MAX_VALUE_SIZE)
        .unwrap_or(i64::MAX)
        .saturating_sub(i64::try_from(without_desc).unwrap_or(i64::MAX))
        .saturating_sub(8)
}

/// `text[:room]` and `text[room:]`, including Python's end-relative reading of
/// a negative bound.
fn split_for_room(text: &[char], room: i64) -> (String, Vec<char>) {
    let len = i64::try_from(text.len()).unwrap_or(i64::MAX);
    let cut = if room < 0 {
        len.saturating_add(room).max(0)
    } else {
        room.min(len)
    };
    let (head, tail) = text.split_at(usize::try_from(cut).unwrap_or(0));
    (head.iter().collect(), tail.to_vec())
}

/// `_truncate_guest_pool_file`: empty the pool once, and only if nothing has
/// written to it since boot.
fn truncate_guest_pool_file(path: &Path, logger: &mut Logger) {
    if ALREADY_TRUNCATED.swap(true, Ordering::SeqCst) {
        return;
    }
    let boot_time = time::now_epoch() - uptime_seconds(logger);
    match modified_epoch(path) {
        Ok(modified) if modified < boot_time => {
            if let Err(why) = std::fs::File::create(path) {
                logger.warning(
                    "handlers.py",
                    &format!("failed to truncate kvp pool file, {why}"),
                );
            }
        }
        Ok(_) => {}
        Err(why) => logger.warning(
            "handlers.py",
            &format!("failed to truncate kvp pool file, {why}"),
        ),
    }
}

fn modified_epoch(path: &Path) -> io::Result<f64> {
    let modified = std::fs::metadata(path)?.modified()?;
    Ok(modified
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs_f64())
        .unwrap_or_default())
}

/// `_get_incarnation_no`: boot time in whole seconds, which distinguishes this
/// boot's records from the ones the host has already read.
fn incarnation_no(logger: &mut Logger) -> i64 {
    let uptime = time::uptime();
    let Ok(seconds) = uptime.parse::<f64>() else {
        logger.warning(
            "handlers.py",
            &format!("uptime '{uptime}' not in correct format."),
        );
        return 0;
    };
    #[allow(clippy::cast_possible_truncation)]
    let boot_time = (time::now_epoch() - seconds) as i64;
    boot_time
}

fn uptime_seconds(logger: &mut Logger) -> f64 {
    let uptime = time::uptime();
    uptime.parse::<f64>().unwrap_or_else(|_| {
        logger.warning(
            "handlers.py",
            &format!("uptime '{uptime}' not in correct format."),
        );
        0.0
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::events::{EventType, Status};
    use crate::handlers::Handler as _;

    fn handler(path: &Path) -> HyperVKvpHandler {
        let mut logger = Logger::silent();
        let mut handler = HyperVKvpHandler::new(path, None, &mut logger);
        handler.incarnation_no = 1_700_000_000;
        handler.set_vm_id("11111111-2222-3333-4444-555555555555");
        handler
    }

    fn event(name: &str, description: &str) -> Event {
        Event {
            event_type: EventType::Finish,
            name: name.to_owned(),
            description: description.to_owned(),
            result: Some(Status::Success),
            duration: Some(1.5),
            timestamp: 1_700_000_100.0,
        }
    }

    fn value_of(record: &[u8]) -> String {
        String::from_utf8_lossy(&record[MAX_KEY_SIZE..])
            .trim_end_matches('\0')
            .to_owned()
    }

    fn key_of(record: &[u8]) -> String {
        String::from_utf8_lossy(&record[..MAX_KEY_SIZE])
            .trim_end_matches('\0')
            .to_owned()
    }

    #[test]
    fn a_record_is_a_padded_key_then_a_padded_value() {
        let record = encode_item("k", "v");
        assert_eq!(record.len(), RECORD_SIZE);
        assert_eq!(&record[..2], b"k\0");
        assert_eq!(&record[MAX_KEY_SIZE..MAX_KEY_SIZE + 2], b"v\0");
        assert!(record[2..MAX_KEY_SIZE].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn an_oversized_field_is_cut_to_its_width_in_bytes_not_characters() {
        // B46: `struct.pack` truncates the UTF-8, not the string, so the last
        // character of a full multi-byte value lands in the pool half-written.
        let record = encode_item(&"k".repeat(600), &"\u{4e00}".repeat(2000));
        assert_eq!(record.len(), RECORD_SIZE);
        assert_eq!(key_of(&record).len(), MAX_KEY_SIZE);
        let value = &record[MAX_KEY_SIZE..];
        assert_eq!(
            value.iter().filter(|byte| **byte != 0).count(),
            MAX_VALUE_SIZE
        );
        assert!(std::str::from_utf8(value).is_err());
    }

    #[test]
    fn write_key_cuts_the_value_one_short_of_what_the_host_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pool");
        let mut logger = Logger::silent();
        let mut handler = handler(&path);

        handler.write_key("K", &"a".repeat(AZURE_MAX_VALUE_SIZE), &mut logger);
        let pool = std::fs::read(&path).unwrap();
        assert_eq!(pool.len(), RECORD_SIZE);
        assert_eq!(value_of(&pool).len(), AZURE_MAX_VALUE_SIZE - 1);
    }

    #[test]
    fn a_short_value_is_written_whole_and_appended_to() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pool");
        let mut logger = Logger::silent();
        let mut handler = handler(&path);

        handler.write_key("PROVISIONING_REPORT", "result=success", &mut logger);
        handler.write_key("PROVISIONING_REPORT", "result=error", &mut logger);

        let pool = std::fs::read(&path).unwrap();
        assert_eq!(pool.len(), 2 * RECORD_SIZE);
        assert_eq!(key_of(&pool), "PROVISIONING_REPORT");
        assert_eq!(value_of(&pool[..RECORD_SIZE]), "result=success");
        assert_eq!(value_of(&pool[RECORD_SIZE..]), "result=error");
    }

    #[test]
    fn an_unwritable_pool_is_a_warning_and_not_a_failure() {
        let mut logger = Logger::silent();
        let mut handler = handler(Path::new("/proc/nosuch/pool"));
        handler.write_key("K", "v", &mut logger);
        handler.publish(&event("n", "d"), &mut logger);
    }

    #[test]
    fn an_event_record_names_the_incarnation_the_type_and_the_vm() {
        let records =
            handler(Path::new("/nonexistent")).encode_event(&event("init", "d"));
        assert_eq!(records.len(), 1);
        let key = key_of(&records[0]);
        assert!(
            key.starts_with(
                "CLOUD_INIT|1700000000|finish|init|11111111-2222-3333-4444-555555555555|"
            ),
            "{key}"
        );
        assert_eq!(
            value_of(&records[0]),
            concat!(
                r#"{"name":"init","type":"finish","ts":"2023-11-14T22:15:00+00:00","#,
                r#""result":"SUCCESS","duration":1.5,"msg":"d"}"#
            )
        );
    }

    #[test]
    fn a_start_event_carries_neither_a_result_nor_a_duration() {
        let mut start = event("init", "d");
        start.event_type = EventType::Start;
        start.result = None;
        start.duration = None;
        let records = handler(Path::new("/nonexistent")).encode_event(&start);
        assert_eq!(
            value_of(&records[0]),
            r#"{"name":"init","type":"start","ts":"2023-11-14T22:15:00+00:00","msg":"d"}"#
        );
    }

    #[test]
    fn an_oversized_description_is_sliced_across_numbered_records() {
        let records = handler(Path::new("/nonexistent"))
            .encode_event(&event("init", &"x".repeat(3000)));
        assert!(records.len() > 2, "{}", records.len());

        for (index, record) in records.iter().enumerate() {
            assert!(key_of(record).ends_with(&format!("|{index}")));
            let value = value_of(record);
            assert!(value.len() <= AZURE_MAX_VALUE_SIZE, "{}", value.len());
            // `msg` moves behind `msg_i`, which is what deleting and re-adding
            // the key does to a Python dict.
            assert!(
                value.contains(&format!(r#""msg_i":{index},"msg":"#)),
                "{value}"
            );
        }

        let rejoined: String = records
            .iter()
            .map(|record| {
                let value = value_of(record);
                let start = value.find(r#""msg":""#).unwrap() + 7;
                value[start..value.len() - 2].to_owned()
            })
            .collect();
        assert_eq!(rejoined, "x".repeat(3000));
    }

    #[test]
    fn a_metadata_block_too_big_to_hold_any_description_stops_instead_of_spinning() {
        // B79: with the metadata alone over the value budget the slice stops
        // shortening the remainder, and upstream's loop then appends records
        // until the process is killed. The port stops at the record that made
        // no progress.
        let records = handler(Path::new("/nonexistent"))
            .encode_event(&event(&"n".repeat(2000), &"x".repeat(3000)));
        assert_eq!(records.len(), 2);
        // The metadata alone overruns the 2048-byte field, so what lands in
        // the pool is a truncated fragment of JSON either way.
        assert_eq!(value_of(&records[1]).len(), MAX_VALUE_SIZE);
    }

    #[test]
    fn the_event_types_filter_drops_everything_it_does_not_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pool");
        let mut logger = Logger::silent();
        let mut handler =
            HyperVKvpHandler::new(&path, Some(vec!["start".to_owned()]), &mut logger);

        handler.publish(&event("n", "d"), &mut logger);
        assert!(!path.exists());

        let mut start = event("n", "d");
        start.event_type = EventType::Start;
        handler.publish(&start, &mut logger);
        assert_eq!(std::fs::read(&path).unwrap().len(), RECORD_SIZE);
    }

    #[test]
    fn a_negative_slice_bound_is_read_from_the_end_the_way_python_reads_it() {
        let text: Vec<char> = "abcdef".chars().collect();
        assert_eq!(split_for_room(&text, 2).0, "ab");
        assert_eq!(split_for_room(&text, -2).0, "abcd");
        assert_eq!(split_for_room(&text, -20).0, "");
        assert_eq!(split_for_room(&text, 20).1, Vec::new());
    }
}
