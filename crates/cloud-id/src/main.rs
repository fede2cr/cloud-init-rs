//! `cloud-id` — port of `cloudinit/cmd/cloud_id.py`.
//!
//! Exit codes are a public contract: 0 success, 1 error, 2 cloud-init disabled,
//! 3 cloud-init has not run yet.

use std::path::PathBuf;
use std::process::ExitCode;

use ci_core::cloud_id::{canonical_cloud_id, METADATA_UNKNOWN};
use ci_core::status::{get_status_details, RunningStatus};
use ci_core::Paths;
use clap::Parser;
use serde_json::Value;

#[derive(Debug, Parser)]
#[command(
    name = "cloud-id",
    about = "Report the canonical cloud-id for this instance"
)]
struct Cli {
    /// Report all standardized cloud-id information as json.
    #[arg(long, short = 'j')]
    json: bool,

    /// Report extended cloud-id information as tab-delimited string.
    #[arg(long, short = 'l')]
    long: bool,

    /// Path to instance-data.json file.
    #[arg(long, short = 'i')]
    instance_data: Option<PathBuf>,
}

fn main() -> ExitCode {
    ExitCode::from(run(&Cli::parse()))
}

fn run(args: &Cli) -> u8 {
    let paths = Paths::read();
    let status = get_status_details(&paths, false);
    match status.running_status {
        RunningStatus::Disabled => {
            println!("{}", RunningStatus::Disabled.value());
            return 2;
        }
        RunningStatus::NotStarted => {
            println!("{}", RunningStatus::NotStarted.value());
            return 3;
        }
        RunningStatus::Running | RunningStatus::Done => {}
    }

    let instance_data_fn = args
        .instance_data
        .clone()
        .unwrap_or_else(|| paths.instance_data_file());

    let Ok(raw) = ci_sys::path::read_text_capped(
        &instance_data_fn,
        ci_sys::path::DEFAULT_MAX_BYTES,
    ) else {
        return error(&format!(
            "File not found '{}'. Provide a path to instance data json file using \
             --instance-data",
            instance_data_fn.display()
        ));
    };

    let instance_data: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(e) => {
            return error(&format!(
                "File '{}' is not valid json. {e}",
                instance_data_fn.display()
            ))
        }
    };

    let mut v1 = instance_data
        .get("v1")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let field = |key: &str| {
        v1.get(key)
            .and_then(Value::as_str)
            .unwrap_or(METADATA_UNKNOWN)
            .to_owned()
    };
    let region = field("region");
    let cloud_id =
        canonical_cloud_id(&field("cloud_name"), &region, &field("platform"));

    let response = if args.json {
        eprintln!("DEPRECATED: Use: cloud-init query v1");
        v1.insert("cloud_id".into(), Value::String(cloud_id));
        ci_core::json_dumps(&Value::Object(v1))
    } else if args.long {
        format!("{cloud_id}\t{region}")
    } else {
        cloud_id
    };

    println!("{response}");
    0
}

fn error(message: &str) -> u8 {
    eprintln!("ERROR: {message}");
    1
}
