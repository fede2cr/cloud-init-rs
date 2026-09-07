//! Dump `util.get_hostname_fqdn` for one case, for differential testing.
//!
//! Usage: `dump-hostname <root> <cfg-json> <metadata-json>`.
//!
//! The precedence between config, metadata and the running system is the whole
//! of this module's behaviour, and getting it wrong is not a crash — it is a
//! machine that comes up under the wrong name. So it is compared case by case
//! against the Python rather than trusted to unit tests alone.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(root), Some(cfg), Some(metadata)) =
        (args.first(), args.get(1), args.get(2))
    else {
        eprintln!("usage: dump-hostname <root> <cfg-json> <metadata-json>");
        std::process::exit(2);
    };

    let parse = |text: &str| -> ci_config::Object {
        if let Some(ci_config::Value::Object(object)) =
            ci_core::jsonfmt::json_loads(text)
        {
            return object;
        }
        eprintln!("not a JSON object: {text}");
        std::process::exit(2);
    };

    let cfg = parse(cfg);
    let metadata = parse(metadata);
    let got = ci_core::hostname::get_hostname_fqdn(
        &cfg,
        Some(&metadata),
        std::path::Path::new(root),
    );

    let mut out = ci_config::Object::new();
    out.insert("hostname".to_owned(), got.hostname.into());
    out.insert("fqdn".to_owned(), got.fqdn.into());
    out.insert("is_default".to_owned(), got.is_default.into());
    println!(
        "{}",
        ci_core::jsonfmt::dumps_indent(&ci_config::Value::Object(out), 1)
    );
}
