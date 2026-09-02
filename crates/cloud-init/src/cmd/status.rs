//! `cloud-init status` — port of `cloudinit/cmd/status.py::handle_status_args`.

use std::fmt::Write as _;
use std::time::Duration;

use ci_config::{Object, Value};
use ci_core::status::{get_status_details, StatusDetails};
use ci_core::Paths;
use clap::{Args as ClapArgs, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Tabular,
    Json,
    Yaml,
}

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Specify output format.
    #[arg(long, short = 'f', value_enum, default_value = "tabular")]
    pub format: Format,

    /// Report long format of statuses including run stage name and error messages.
    #[arg(long, short = 'l')]
    pub long: bool,

    /// Block waiting on cloud-init to complete.
    #[arg(long, short = 'w')]
    pub wait: bool,
}

const POLL_INTERVAL: Duration = Duration::from_millis(250);

pub fn run(args: &Args) -> u8 {
    let paths = Paths::read();

    let details = loop {
        let details = get_status_details(&paths, args.wait);
        if !args.wait || details.is_settled() {
            break details;
        }
        if args.format == Format::Tabular {
            print!(".");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    if args.wait && args.format == Format::Tabular {
        println!();
    }

    print!("{}", render(&details, args));

    // Exit codes are a public contract: 0 healthy, 1 error, 2 recoverable error.
    u8::try_from(details.exit_code()).unwrap_or(1)
}

fn render(details: &StatusDetails, args: &Args) -> String {
    let (status, extended_status) = details.translate();
    match args.format {
        Format::Tabular => {
            render_tabular(details, args.long, &status, &extended_status)
        }
        Format::Json => {
            let map = details_map(details, &status, &extended_status);
            format!("{}\n", ci_core::dumps_indent(&Value::Object(map), 2))
        }
        Format::Yaml => {
            let map = details_map(details, &status, &extended_status);
            render_yaml(&Value::Object(map))
        }
    }
}

fn render_tabular(
    details: &StatusDetails,
    long: bool,
    status: &str,
    extended_status: &str,
) -> String {
    let mut out = format!("status: {status}\n");
    if !long {
        return out;
    }

    let errors = if details.errors.is_empty() {
        " []".to_owned()
    } else {
        format!("\n\t- {}", details.errors.join("\n\t- "))
    };

    let recoverable = if details.recoverable_errors.is_empty() {
        " {}".to_owned()
    } else {
        let mut keys: Vec<&String> = details.recoverable_errors.keys().collect();
        keys.sort();
        let blocks: Vec<String> = keys
            .into_iter()
            .filter_map(|key| {
                let messages: Vec<String> = details
                    .recoverable_errors
                    .get(key)?
                    .as_array()?
                    .iter()
                    .map(value_text)
                    .collect();
                Some(format!("{key}:\n\t- {}", messages.join("\n\t- ")))
            })
            .collect();
        format!("\n{}", blocks.join("\n"))
    };

    let last_update = if details.last_update.is_empty() {
        String::new()
    } else {
        format!("last_update: {}\n", details.last_update)
    };

    let _ = write!(
        out,
        "extended_status: {extended_status}\n\
         boot_status_code: {}\n\
         {last_update}detail: {}\n\
         errors:{errors}\n\
         recoverable_errors:{recoverable}\n",
        details.boot_status_code.value(),
        details.description,
    );
    out
}

/// The reported mapping, in upstream key order (JSON output sorts it anyway).
fn details_map(details: &StatusDetails, status: &str, extended_status: &str) -> Object {
    let mut map = Object::new();
    map.insert(
        "datasource".into(),
        Value::String(details.datasource.clone()),
    );
    map.insert(
        "boot_status_code".into(),
        Value::String(details.boot_status_code.value().to_owned()),
    );
    map.insert("status".into(), Value::String(status.to_owned()));
    map.insert(
        "extended_status".into(),
        Value::String(extended_status.to_owned()),
    );
    map.insert("detail".into(), Value::String(details.description.clone()));
    map.insert(
        "errors".into(),
        Value::Array(
            details
                .errors
                .iter()
                .map(|e| Value::String(e.clone()))
                .collect(),
        ),
    );
    map.insert(
        "recoverable_errors".into(),
        Value::Object(details.recoverable_errors.clone()),
    );
    map.insert(
        "last_update".into(),
        Value::String(details.last_update.clone()),
    );
    for (key, value) in &details.v1 {
        map.insert(key.clone(), value.clone());
    }
    map
}

/// `safeyaml.dumps()` emits explicit document markers, and upstream `print()`s the
/// result, adding one more newline after the closing `...`.
fn render_yaml(value: &Value) -> String {
    let sorted = ci_core::jsonfmt::sort_keys(value);
    match serde_yaml_ng::to_string(&sorted) {
        Ok(body) => format!("---\n{body}...\n\n"),
        Err(_) => String::new(),
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use ci_core::status::{ConditionStatus, EnabledStatus, RunningStatus};
    use serde_json::json;

    fn details() -> StatusDetails {
        StatusDetails {
            running_status: RunningStatus::Done,
            condition_status: ConditionStatus::Peachy,
            boot_status_code: EnabledStatus::EnabledByGenerator,
            description: "DataSourceNoCloud".to_owned(),
            errors: Vec::new(),
            recoverable_errors: Object::new(),
            last_update: "Mon, 01 Jan 2024 00:00:00 +0000".to_owned(),
            datasource: "nocloud".to_owned(),
            v1: Object::new(),
        }
    }

    fn args(format: Format, long: bool) -> Args {
        Args {
            format,
            long,
            wait: false,
        }
    }

    #[test]
    fn short_tabular_is_one_line() {
        assert_eq!(
            render(&details(), &args(Format::Tabular, false)),
            "status: done\n"
        );
    }

    #[test]
    fn long_tabular_matches_upstream_template() {
        let mut d = details();
        d.errors = vec!["bad".to_owned()];
        d.condition_status = ConditionStatus::Error;
        d.recoverable_errors =
            json!({"WARNING": ["careful"]}).as_object().unwrap().clone();

        assert_eq!(
            render(&d, &args(Format::Tabular, true)),
            "status: error\n\
             extended_status: error - done\n\
             boot_status_code: enabled-by-generator\n\
             last_update: Mon, 01 Jan 2024 00:00:00 +0000\n\
             detail: DataSourceNoCloud\n\
             errors:\n\t- bad\n\
             recoverable_errors:\nWARNING:\n\t- careful\n"
        );
    }

    #[test]
    fn empty_last_update_is_omitted() {
        let mut d = details();
        d.last_update = String::new();
        let out = render(&d, &args(Format::Tabular, true));
        assert!(!out.contains("last_update"), "{out}");
    }

    #[test]
    fn json_output_is_sorted_and_indented_by_two() {
        let out = render(&details(), &args(Format::Json, false));
        assert!(out.starts_with("{\n  \"boot_status_code\": "), "{out}");
        assert!(out.ends_with("}\n"), "{out}");
    }
}
