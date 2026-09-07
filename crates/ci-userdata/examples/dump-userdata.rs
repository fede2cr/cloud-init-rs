//! Dump the processed parts of a user-data blob as JSON.
//!
//! Reads the blob on stdin. The optional argument is a `cloud_dir`, which only
//! matters for `#include-once`: that is where the URL cache lives. Exists so the
//! walk can be diffed against `cloudinit.user_data.UserDataProcessor`; see
//! tests/differential/userdata.py.

use std::io::Read as _;

fn main() {
    let mut blob = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut blob) {
        eprintln!("read: {e}");
        std::process::exit(1);
    }
    let mut paths = ci_core::Paths::default();
    let mut args = std::env::args().skip(1);
    let mut mime = false;
    for arg in args.by_ref() {
        if arg == "--mime" {
            mime = true;
        } else {
            paths.cloud_dir = arg.into();
        }
    }
    let processed = match ci_userdata::process(&blob, &paths) {
        Ok(processed) => processed,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    if mime {
        let boundary = processed.message.boundary().unwrap_or_default();
        let text = String::from_utf8_lossy(&processed.message.to_bytes()).into_owned();
        print!("{}", text.replace(&boundary, "BOUND"));
        return;
    }
    let parts: Vec<_> = processed
        .parts
        .iter()
        .map(|part| {
            serde_json::json!({
                "content_type": part.content_type,
                "filename": part.filename,
                "launch_index": part.launch_index,
                "payload": String::from_utf8_lossy(&part.payload),
            })
        })
        .collect();
    match serde_json::to_string_pretty(&parts) {
        Ok(text) => println!("{text}"),
        Err(e) => {
            eprintln!("encode: {e}");
            std::process::exit(1);
        }
    }
}
