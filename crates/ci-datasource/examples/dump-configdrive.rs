//! Dump what a config-drive directory yields, for the `configdrive.py`
//! differential.
//!
//! Payloads are base64 so a byte-for-byte comparison survives arbitrary
//! content, and metadata is canonical JSON because both sides parse it.

fn main() {
    let Some(dir) = std::env::args().nth(1) else {
        eprintln!("usage: dump-configdrive DIR");
        std::process::exit(2);
    };
    match ci_datasource::helpers::openstack::read_config_drive(std::path::Path::new(
        &dir,
    )) {
        Ok(results) => dump(&results),
        Err(err) => println!("{}={err}", kind(&err)),
    }
}

fn kind(err: &ci_datasource::helpers::openstack::Error) -> &'static str {
    match err {
        ci_datasource::helpers::openstack::Error::NonReadable(_) => "NonReadable",
        ci_datasource::helpers::openstack::Error::Broken(_) => "BrokenMetadata",
    }
}

fn dump(results: &ci_datasource::helpers::openstack::Results) {
    println!("version={}", results.version);
    println!(
        "metadata={}",
        ci_core::json_dumps(&ci_config::Value::Object(results.metadata.clone()))
    );
    println!("userdata={}", as_b64(results.userdata.as_deref()));
    println!("dsmode={}", results.dsmode.as_deref().unwrap_or("<none>"));
    println!("vendordata={}", as_json(results.vendordata.as_ref()));
    println!("vendordata2={}", as_json(results.vendordata2.as_ref()));
    println!("networkdata={}", as_json(results.networkdata.as_ref()));
    println!("ec2-metadata={}", as_json(results.ec2_metadata.as_ref()));
    println!(
        "network_config={}",
        results.network_config.as_deref().map_or_else(
            || "<none>".to_owned(),
            |text| ci_core::b64::encode(text.as_bytes())
        )
    );
    for (path, content) in &results.files {
        println!("file {path}={}", ci_core::b64::encode(content));
    }
}

fn as_json(value: Option<&ci_config::Value>) -> String {
    value.map_or_else(|| "<none>".to_owned(), ci_core::json_dumps)
}

fn as_b64(bytes: Option<&[u8]>) -> String {
    bytes.map_or_else(|| "<none>".to_owned(), ci_core::b64::encode)
}
