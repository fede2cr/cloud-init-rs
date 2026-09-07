//! Dump a parsed dhcpcd lease, for differential testing.
//!
//! Usage: `dump-dhcp <fixture-json>`
//!
//! The fixture stands in for the two things a real run would read: the text
//! `dhcpcd --dumplease` prints, and the binary lease packet the unknown options
//! live in. Nothing here runs `dhcpcd`, so the comparison is of the parsing,
//! which is the half that decides where a machine on Azure sends report-ready.
//!
//! Keys: `dump` (string), `interface` (string), `packet` (array of byte values,
//! optional), `routes` (string, optional — parsed separately).

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: dump-dhcp <fixture-json>");
        return ExitCode::FAILURE;
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            eprintln!("{path}: {error}");
            return ExitCode::FAILURE;
        }
    };
    let doc: ci_config::Value = match serde_json::from_str(&text) {
        Ok(doc) => doc,
        Err(error) => {
            eprintln!("{path}: {error}");
            return ExitCode::FAILURE;
        }
    };

    let dump = doc
        .get("dump")
        .and_then(ci_config::Value::as_str)
        .unwrap_or("");
    let interface = doc
        .get("interface")
        .and_then(ci_config::Value::as_str)
        .unwrap_or("eth0");
    let packet: Option<Vec<u8>> =
        doc.get("packet").and_then(|v| v.as_array()).map(|bytes| {
            bytes
                .iter()
                .filter_map(|b| b.as_u64().and_then(|n| u8::try_from(n).ok()))
                .collect()
        });

    let mut log = ci_log::Logger::silent();

    if let Some(routes) = doc.get("routes").and_then(ci_config::Value::as_str) {
        let parsed = ci_net::dhcp::parse_static_routes(routes, &mut log);
        for (dest, gateway) in parsed {
            println!("route {dest} {gateway}");
        }
    }

    match ci_net::dhcp::parse_lease(dump, interface, packet.as_deref(), &mut log) {
        Ok(lease) => {
            for (key, value) in &lease {
                println!("{key}={}", value.as_str().unwrap_or_default());
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            println!("error={error}");
            ExitCode::FAILURE
        }
    }
}
