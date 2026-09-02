//! Dump the processed parts of a user-data blob as JSON.
//!
//! Reads the blob on stdin. Exists so the walk can be diffed against
//! `cloudinit.user_data.UserDataProcessor`; see tests/differential/userdata.py.

use std::io::Read as _;

fn main() {
    let mut blob = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut blob) {
        eprintln!("read: {e}");
        std::process::exit(1);
    }
    let processed = match ci_userdata::process(&blob) {
        Ok(processed) => processed,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
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
