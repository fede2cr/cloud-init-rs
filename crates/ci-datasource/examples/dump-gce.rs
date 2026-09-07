//! Print what `read_md` makes of a `GCE` metadata service.

fn main() {
    let Some(address) = std::env::args().nth(1) else {
        eprintln!("usage: dump-gce URL");
        std::process::exit(2);
    };

    let config = ci_url::Config {
        timeout: std::time::Duration::from_secs(2),
        retries: 0,
        headers: vec![("Metadata-Flavor".to_owned(), "Google".to_owned())],
        ..ci_url::Config::default()
    };

    let result = ci_datasource::gce::read_md(&address, &config);
    let Some(metadata) = result.metadata else {
        println!("failed={}", result.reason.unwrap_or_default());
        return;
    };

    println!(
        "metadata={}",
        ci_core::json_dumps(&ci_config::Value::Object(metadata))
    );
    println!(
        "userdata={}",
        result
            .userdata
            .as_deref()
            .map_or_else(|| "<none>".to_owned(), ci_core::b64::encode)
    );
}
