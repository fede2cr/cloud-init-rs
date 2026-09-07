//! Dump network renderer availability and selection, for differential testing.
//!
//! Usage: `dump-renderers [priority,priority,...]`
//!
//! Unlike the other dumpers this one has no fixture: `available()` probes the
//! machine it runs on, so the only thing worth comparing is what the two
//! implementations say about *this* host. That is also the only comparison
//! that matters — the pair either agrees about which renderer a boot here
//! would use, or one of them would misconfigure the network.

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

    // Every renderer's `available()`, independent of any priority list, so a
    // disagreement names the probe that differs rather than just the winner.
    let mut available = ci_config::Object::new();
    for (name, _) in ci_net::renderers::NAME_TO_RENDERER {
        let found = ci_net::renderers::search(Some(&[(*name).to_owned()]), false);
        available.insert(
            (*name).to_owned(),
            ci_config::Value::Bool(matches!(found, Ok(ref f) if !f.is_empty())),
        );
    }
    out.insert("available".to_owned(), ci_config::Value::Object(available));

    match ci_net::renderers::search(priority.as_deref(), false) {
        Ok(found) => out.insert(
            "search".to_owned(),
            ci_config::Value::Array(
                found
                    .iter()
                    .map(|n| ci_config::Value::String((*n).to_owned()))
                    .collect(),
            ),
        ),
        Err(err) => out.insert(
            "search_error".to_owned(),
            ci_config::Value::String(err.to_string()),
        ),
    };

    match ci_net::renderers::select(priority.as_deref()) {
        Ok(name) => out.insert(
            "select".to_owned(),
            ci_config::Value::String(name.to_owned()),
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
