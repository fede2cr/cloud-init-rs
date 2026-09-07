//! Dump network activator availability and selection, for differential testing.
//!
//! Usage: `dump-activators [priority,priority,...]`
//!
//! Like `dump-renderers` this has no fixture: `available()` probes the machine
//! it runs on. Selection is the only part that can be compared — actually
//! bringing an interface up would reconfigure the host running the test.

use ci_net::activators;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let priority: Option<Vec<String>> = args.first().map(|list| {
        if list.is_empty() {
            Vec::new()
        } else {
            list.split(',').map(str::to_owned).collect()
        }
    });

    let mut out = ci_config::Object::new();

    // Every activator's `available()`, independent of any priority list, so a
    // disagreement names the probe that differs rather than just the winner.
    let mut available = ci_config::Object::new();
    for name in activators::DEFAULT_PRIORITY {
        let found = activators::by_name(name)
            .is_some_and(activators::NetworkActivator::available);
        available.insert((*name).to_owned(), ci_config::Value::Bool(found));
    }
    out.insert("available".to_owned(), ci_config::Value::Object(available));

    match activators::search(priority.as_deref()) {
        Ok(found) => out.insert(
            "search".to_owned(),
            found.map_or(ci_config::Value::Null, |a| {
                ci_config::Value::String(a.py_repr().to_owned())
            }),
        ),
        Err(err) => out.insert(
            "search_error".to_owned(),
            ci_config::Value::String(err.to_string()),
        ),
    };

    let mut log = ci_log::Logger::silent();
    match activators::select(priority.as_deref(), &mut log) {
        Ok(activator) => out.insert(
            "select".to_owned(),
            ci_config::Value::String(activator.py_repr().to_owned()),
        ),
        Err(err) => out.insert(
            "select_error".to_owned(),
            ci_config::Value::String(err.to_string()),
        ),
    };

    println!(
        "{}",
        ci_core::jsonfmt::dumps_indent(&ci_config::Value::Object(out), 1)
    );
}
