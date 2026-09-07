//! Dump what a seedfrom base yields, for the `seed.py` differential.
//!
//! Metadata and network config are compared as canonical JSON because upstream
//! parses them as YAML; the two payloads are base64 so a byte-for-byte
//! comparison survives arbitrary content.

fn main() {
    let Some(base) = std::env::args().nth(1) else {
        eprintln!("usage: dump-seed BASE");
        std::process::exit(2);
    };
    let mut logger = ci_log::Logger::silent();
    let config = ci_url::Config {
        timeout: std::time::Duration::from_secs(1),
        retries: 0,
        ..ci_url::Config::default()
    };
    let Some(seed) =
        ci_datasource::nocloud::read_seeded_with(&base, &config, &mut logger)
    else {
        println!("error");
        return;
    };
    println!("meta-data={}", as_json(seed_meta(&seed), true));
    println!("user-data={}", as_b64(seed.user_data.as_deref()));
    println!("vendor-data={}", as_b64(seed.vendor_data.as_deref()));
    println!(
        "network-config={}",
        as_json(seed.network_config.as_deref(), false)
    );
}

fn seed_meta(seed: &ci_datasource::nocloud::Seed) -> Option<&[u8]> {
    match &seed.meta {
        ci_datasource::nocloud::Meta::Raw(bytes) => Some(bytes),
        ci_datasource::nocloud::Meta::Parsed(_) => None,
    }
}

/// `load_yaml(blob, default, allowed=(dict,))` then `json_dumps`.
fn as_json(bytes: Option<&[u8]>, empty_default: bool) -> String {
    let default = if empty_default { "{}" } else { "<none>" };
    let Some(bytes) = bytes else {
        return default.to_owned();
    };
    let text = String::from_utf8_lossy(bytes);
    match ci_config::load_yaml(&text, ci_config::Limits::default()) {
        Ok(value) if value.is_object() => ci_core::json_dumps(&value),
        _ => default.to_owned(),
    }
}

fn as_b64(bytes: Option<&[u8]>) -> String {
    bytes.map_or_else(|| "<none>".to_owned(), ci_core::b64::encode)
}
