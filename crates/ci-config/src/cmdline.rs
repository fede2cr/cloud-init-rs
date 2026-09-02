//! Port of `util.get_cmdline` / `util.read_cc_from_cmdline`.
//!
//! The kernel command line is attacker-controlled in some hypervisor setups and
//! carries the highest-priority cloud-config, so parsing is strict and bounded.

use crate::yaml::{load_mapping, Limits};
use crate::Object;

const TAG_BEGIN: &str = "cc:";
const TAG_END: &str = "end_cc";

/// Override used by tests and by `DEBUG_PROC_CMDLINE` upstream.
pub const DEBUG_ENV: &str = "DEBUG_PROC_CMDLINE";

/// Read the kernel command line.
pub fn get_cmdline() -> String {
    if let Ok(value) = std::env::var(DEBUG_ENV) {
        return value;
    }
    ci_sys::path::read_text_optional("/proc/cmdline", 64 * 1024)
        .ok()
        .flatten()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Extract the cloud-config embedded between `cc:` and `end_cc` markers.
pub fn read_cc_from_cmdline(cmdline: &str) -> String {
    let mut tokens = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = cmdline.get(search_from..).and_then(|s| s.find(TAG_BEGIN)) {
        let begin = search_from + rel + TAG_BEGIN.len();
        let end = cmdline
            .get(begin..)
            .and_then(|s| s.find(TAG_END))
            .map_or(cmdline.len(), |rel| begin + rel);
        if let Some(chunk) = cmdline.get(begin..end) {
            tokens.push(percent_decode(chunk.trim_start()).replace("\\n", "\n"));
        }
        search_from = end + TAG_END.len();
        if search_from >= cmdline.len() {
            break;
        }
    }
    tokens.join("\n")
}

/// Config supplied on the kernel command line, if any.
pub fn read_conf_from_cmdline(cmdline: &str, limits: Limits) -> Object {
    let payload = read_cc_from_cmdline(cmdline);
    if payload.trim().is_empty() {
        return Object::new();
    }
    load_mapping(&payload, limits).unwrap_or_default()
}

/// `urllib.parse.unquote`, restricted to well-formed `%XX` sequences.
fn percent_decode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut bytes = input.bytes().peekable();
    let mut pending: Vec<u8> = Vec::new();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = bytes.peek().copied().and_then(hex_val);
            if let Some(hi) = hi {
                bytes.next();
                let lo = bytes.peek().copied().and_then(hex_val);
                if let Some(lo) = lo {
                    bytes.next();
                    pending.push(hi * 16 + lo);
                    continue;
                }
                // `%X` with no second digit: emit verbatim.
                pending.push(b'%');
                pending.push(hi_char(hi));
                continue;
            }
            pending.push(b'%');
            continue;
        }
        pending.push(b);
    }
    out.push_str(&String::from_utf8_lossy(&pending));
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hi_char(v: u8) -> u8 {
    if v < 10 {
        b'0' + v
    } else {
        b'a' + (v - 10)
    }
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

    #[test]
    fn extracts_a_single_block() {
        let cmdline = "root=/dev/sda1 cc: ssh_pwauth: true end_cc quiet";
        assert_eq!(read_cc_from_cmdline(cmdline), "ssh_pwauth: true ");
    }

    #[test]
    fn extracts_multiple_blocks_and_decodes() {
        let cmdline = "cc: a%3A%201 end_cc noise cc: b: 2 end_cc";
        assert_eq!(read_cc_from_cmdline(cmdline), "a: 1 \nb: 2 ");
    }

    #[test]
    fn unterminated_block_runs_to_end_of_line() {
        assert_eq!(read_cc_from_cmdline("cc: hostname: x"), "hostname: x");
    }

    #[test]
    fn escaped_newlines_become_newlines() {
        let cfg = read_conf_from_cmdline("cc: a: 1\\nb: 2 end_cc", Limits::default());
        assert_eq!(cfg["a"], serde_json::Value::from(1));
        assert_eq!(cfg["b"], serde_json::Value::from(2));
    }

    #[test]
    fn absent_marker_yields_nothing() {
        assert!(read_conf_from_cmdline("quiet splash", Limits::default()).is_empty());
    }
}
