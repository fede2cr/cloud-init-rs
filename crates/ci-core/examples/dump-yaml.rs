//! Dumps JSON from stdin as `safeyaml.dumps` would, for differential testing.

use std::io::Read as _;

fn main() {
    let mut input = String::new();
    if let Err(error) = std::io::stdin().read_to_string(&mut input) {
        eprintln!("read: {error}");
        std::process::exit(1);
    }
    match serde_json::from_str::<serde_json::Value>(&input) {
        Ok(value) => print!("{}", ci_core::yamlfmt::dumps(&value)),
        Err(error) => {
            eprintln!("parse: {error}");
            std::process::exit(1);
        }
    }
}
