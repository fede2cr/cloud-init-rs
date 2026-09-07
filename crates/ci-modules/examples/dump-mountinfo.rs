//! `util.parse_mount_info` and `util.parse_mtab`, for the differential
//! harness. Paired with `tests/differential/mountinfo.py`.
//!
//! Usage: `dump-mountinfo --batch <cases-file>`
//!        `dump-mountinfo <kind> <path> <line>\x1f<line>...`
//!
//! `<kind>` is `mountinfo` or `mtab`. The file contents are passed in rather
//! than read so that the comparison can cover shapes the host is not in —
//! bind mounts, btrfs subvolumes, overmounts, and the malformed lines that
//! make the parser bail.
//!
//! It lives in `ci-modules` rather than `ci-sys`, whose code this is, because
//! `ci-sys` is the root of the crate graph and has no JSON dependency to lean
//! on. `cc_mounts`, `cc_resizefs` and `cc_growpart` are the callers anyway.
//!
//! Batch mode takes one tab-separated argument list per line and emits a
//! `## <line>` marker before each record.

use ci_config::{Object, Value};
use ci_sys::mount::{parse_mount_info, parse_mtab, MountInfo};

/// Separates the file's lines within a single tab-delimited field.
const LINE_SEP: char = '\u{1f}';

fn main() -> std::process::ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let arg = |index: usize| argv.get(index).map_or("", String::as_str);

    if arg(1) == "--batch" {
        let Ok(text) = std::fs::read_to_string(arg(2)) else {
            eprintln!("dump-mountinfo: cannot read {:?}", arg(2));
            return std::process::ExitCode::from(2);
        };
        for line in text.lines() {
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            println!("## {line}");
            println!("{}", ci_core::jsonfmt::dumps_indent(&one(&fields), 1));
        }
        return std::process::ExitCode::SUCCESS;
    }

    if argv.len() < 3 {
        eprintln!("usage: dump-mountinfo <kind> <path> <lines> | --batch <cases>");
        return std::process::ExitCode::from(2);
    }
    let fields: Vec<&str> = (1..4).map(arg).collect();
    println!("{}", ci_core::jsonfmt::dumps_indent(&one(&fields), 1));
    std::process::ExitCode::SUCCESS
}

fn one(fields: &[&str]) -> Value {
    let at = |index: usize| fields.get(index).copied().unwrap_or("");
    let path = at(1);
    let blob = at(2);
    let lines: Vec<&str> = if blob.is_empty() {
        Vec::new()
    } else {
        blob.split(LINE_SEP).collect()
    };

    let mut out = Object::new();
    let mut debug: Vec<Value> = Vec::new();
    let result = if at(0) == "mtab" {
        // `parse_mtab` ignores the log entirely, and ignores `get_mnt_opts`
        // too -- hence three fields, not four.
        parse_mtab(path, &lines.join("\n")).map(|info| triple(&info))
    } else {
        match parse_mount_info(path, &lines) {
            Ok(found) => found.map(|info| quad(&info)),
            Err(error) => {
                debug.push(Value::String(error.to_string()));
                None
            }
        }
    };

    out.insert("debug".to_owned(), Value::Array(debug));
    out.insert("result".to_owned(), result.unwrap_or(Value::Null));
    Value::Object(out)
}

fn triple(info: &MountInfo) -> Value {
    Value::Array(vec![
        Value::String(info.devpth.clone()),
        Value::String(info.fs_type.clone()),
        Value::String(info.mount_point.clone()),
    ])
}

fn quad(info: &MountInfo) -> Value {
    let Value::Array(mut items) = triple(info) else {
        unreachable!("triple builds an array")
    };
    items.push(Value::String(info.opts.clone()));
    Value::Array(items)
}
