//! Print what the `Ec2` metadata crawler makes of a metadata service.

use ci_datasource::helpers::ec2;

/// The crawler reads through this; the datasource's own caller adds the
/// `IMDSv2` token, which this deliberately does not.
struct Plain(ci_url::Config);

impl ec2::Caller for Plain {
    fn fetch(&mut self, url: &str) -> Result<Vec<u8>, ci_url::Error> {
        ci_url::readurl(url, &self.0).map(|response| response.contents)
    }
}

fn main() {
    let Some(address) = std::env::args().nth(1) else {
        eprintln!("usage: dump-ec2 URL");
        std::process::exit(2);
    };

    let mut caller = Plain(ci_url::Config {
        timeout: std::time::Duration::from_secs(2),
        retries: 0,
        ..ci_url::Config::default()
    });
    let mut logger = ci_log::Logger::silent();

    let metadata = ec2::instance_metadata("latest", &address, &mut caller, &mut logger);
    let userdata = ec2::instance_userdata("latest", &address, &mut caller, &mut logger);
    let identity = ec2::instance_identity("latest", &address, &mut caller, &mut logger);

    println!(
        "metadata={}",
        ci_core::json_dumps(&ci_config::Value::Object(metadata))
    );
    println!(
        "identity={}",
        ci_core::json_dumps(&ci_config::Value::Object(identity))
    );
    println!(
        "userdata={}",
        if userdata.is_empty() {
            "<none>".to_owned()
        } else {
            ci_core::b64::encode(&userdata)
        }
    );
    // What `Ec2::get_data` stores.
    let decoded = ci_datasource::ec2::maybe_b64decode(&userdata);
    println!(
        "decoded={}",
        if decoded.is_empty() {
            "<none>".to_owned()
        } else {
            ci_core::b64::encode(&decoded)
        }
    );
}
