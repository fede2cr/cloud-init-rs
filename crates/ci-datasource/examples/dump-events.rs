//! Dump the update-event decisions, for the `events.py` differential.

use ci_datasource::event::{self, Scope, Type};

fn fail(message: &str) -> ! {
    eprintln!("{message}");
    std::process::exit(2)
}

fn read_cfg(path: &str) -> ci_config::Object {
    match std::fs::read_to_string(path).map(|text| serde_json::from_str(&text)) {
        Ok(Ok(cfg)) => cfg,
        Ok(Err(error)) => fail(&format!("{path}: {error}")),
        Err(error) => fail(&format!("{path}: {error}")),
    }
}

/// `json.dumps(..., sort_keys=True)` of the mapping, with the event names
/// sorted too so the comparison does not depend on either side's set order.
fn show(mapping: &event::Events) -> String {
    let entries: Vec<String> = mapping
        .iter()
        .map(|(scope, types)| {
            let mut names: Vec<&str> =
                types.iter().map(|event| event.as_str()).collect();
            names.sort_unstable();
            let names: Vec<String> =
                names.iter().map(|name| format!("\"{name}\"")).collect();
            format!("\"{}\": [{}]", scope.as_str(), names.join(", "))
        })
        .collect();
    format!("{{{}}}", entries.join(", "))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |index: usize| args.get(index).map_or("", String::as_str);
    let mut logger = ci_log::Logger::silent();

    match arg(0) {
        "convert" => {
            let cfg = read_cfg(arg(1));
            let mapping = event::userdata_to_events(cfg.get("updates"), &mut logger);
            println!("events={}", show(&mapping));
        }
        "enabled" => {
            let Some(probe) = ci_datasource::probe_for_class(arg(1)) else {
                fail(&format!("unknown datasource class {}", arg(1)));
            };
            let cfg = read_cfg(arg(2));
            let Some(event_type) = Type::parse(arg(3)) else {
                fail(&format!("unknown event type {}", arg(3)));
            };
            let paths = ci_core::Paths {
                cloud_dir: std::path::PathBuf::from(arg(4)),
                ..ci_core::Paths::default()
            };
            let enabled = event::update_event_enabled(
                probe.as_ref(),
                &cfg,
                event_type,
                Scope::Network,
                &paths,
                &mut logger,
            );
            println!("enabled={}", if enabled { "True" } else { "False" });
        }
        other => fail(&format!("unknown mode {other}")),
    }

    if let Some(warnings) = logger.recoverable_errors().get("WARNING") {
        for message in warnings.as_array().into_iter().flatten() {
            println!("warning={}", message.as_str().unwrap_or_default());
        }
    }
}
