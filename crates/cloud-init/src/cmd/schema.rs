//! `cloud-init schema` — validate a cloud-config document against the schema.
//!
//! Port of `cloudinit/config/schema.py::handle_schema_args` and
//! `validate_cloudconfig_file`. The validation itself lives in `ci-schema`;
//! this module reproduces the CLI's output shape, which puts verdict lines on
//! stdout and every `Error:` line on stderr.

use std::fmt::Write as _;
use std::path::Path;

use ci_schema::marks::Marks;
use ci_schema::{Kind, Schema};
use ci_userdata::types::type_from_starts_with;

/// `USERDATA_VALID_HEADERS`, already joined the way the message needs it.
const VALID_HEADERS: &str = "#!, ## template: jinja, #cloud-boothook, #cloud-config, \
     #cloud-config-archive, #cloud-config-jsonp, #include, #include-once, #part-handler";

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Path of the cloud-config yaml file to validate.
    #[arg(short = 'c', long = "config-file")]
    config_file: Option<String>,

    /// The type of the config file to validate.
    #[arg(short = 't', long = "schema-type", value_parser = ["cloud-config", "network-config"])]
    schema_type: Option<String>,

    /// Path to instance-data.json file for variable expansion of '##template:
    /// jinja' user-data. Default: /run/cloud-init/instance-data.json.
    #[arg(short = 'i', long = "instance-data")]
    instance_data: Option<String>,

    /// Validate the system instance-data provided as vendordata, vendordata2,
    /// userdata and networkconfig.
    #[arg(long)]
    system: bool,

    /// Annotate existing cloud-config file with errors.
    #[arg(long)]
    annotate: bool,
}

/// One `SchemaProblem`: a flat config path and a message.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Problem {
    path: String,
    message: String,
}

impl Problem {
    fn format(&self) -> String {
        format!("{}: {}", self.path, self.message)
    }
}

/// What `validate_cloudconfig_file` communicates back to `handle_schema_args`.
enum Outcome {
    /// Validation ran; print the `Valid schema` line.
    Validated,
    /// Validation was skipped; upstream has already printed why.
    Skipped,
    /// `SchemaValidationError`.
    Invalid(String),
    /// `RuntimeError`, which is reported without the config path.
    Runtime(String),
}

pub fn run(args: &Args) -> u8 {
    // _assert_exclusive_args
    let exclusive = usize::from(args.config_file.is_some()) + usize::from(args.system);
    if exclusive != 1 {
        eprintln!("Error:\nExpected one of --config-file or --system arguments");
        return 1;
    }
    if args.system {
        eprintln!(
            "Error:\n--system is not implemented in this port. \
             Pass --config-file with an explicit path instead."
        );
        return 1;
    }
    if args.schema_type.as_deref() == Some("network-config") {
        eprintln!(
            "Error:\n--schema-type network-config is not implemented in this port."
        );
        return 1;
    }

    let Some(config_path) = args.config_file.as_deref() else {
        eprintln!("Error:\nExpected one of --config-file or --system arguments");
        return 1;
    };
    if !Path::new(config_path).exists() {
        eprintln!("Error: Config file {config_path} does not exist");
        return 1;
    }

    match validate_file(config_path, args.annotate) {
        Outcome::Validated => {
            println!("Valid schema {config_path}");
            0
        }
        Outcome::Skipped => 0,
        Outcome::Invalid(message) => {
            if !args.annotate {
                println!("Invalid user-data {config_path}");
                eprintln!("Error: {message}\n");
            }
            eprintln!("Error: Invalid schema: user-data\n");
            1
        }
        Outcome::Runtime(message) => {
            println!("Invalid user-data");
            eprintln!("Error: {message}\n");
            eprintln!("Error: Invalid schema: user-data\n");
            1
        }
    }
}

fn validate_file(config_path: &str, annotate: bool) -> Outcome {
    let content = match std::fs::read_to_string(config_path) {
        Ok(content) => content,
        Err(e) => return Outcome::Runtime(format!("{config_path}: {e}")),
    };
    if content.is_empty() {
        println!("Empty 'cloud-config' found at {config_path}. Nothing to validate.");
        return Outcome::Skipped;
    }

    match type_from_starts_with(content.as_bytes(), None) {
        None => {
            let header = content.split('\n').next().unwrap_or_default();
            let problem = Problem {
                path: "format-l1.c1".to_owned(),
                message: format!(
                    "Unrecognized user-data header in {config_path}: \"{header}\".\n\
                     Expected first line to be one of: {VALID_HEADERS}"
                ),
            };
            // Raised before any annotation happens, so `--annotate` prints nothing.
            Outcome::Invalid(format_problems(
                &[problem],
                "Cloud config schema errors: ",
            ))
        }
        Some("text/cloud-config") => {
            validate_cloud_config(config_path, &content, annotate)
        }
        Some(other) => {
            println!(
                "User-data type '{other}' not currently evaluated by cloud-init schema"
            );
            Outcome::Skipped
        }
    }
}

fn validate_cloud_config(config_path: &str, content: &str, annotate: bool) -> Outcome {
    let limits = ci_config::Limits::default();
    let parsed = match ci_config::load_yaml(content, limits) {
        Ok(value) => value,
        Err(e) => {
            let (line, column) = yaml_position(&e);
            let problem = Problem {
                path: format!("format-l{line}.c{column}"),
                message: format!(
                    "File {config_path} is not valid YAML. {}",
                    yaml_reason(&e)
                ),
            };
            let problems = [problem];
            if annotate {
                println!(
                    "{}",
                    annotate_content(content, &Marks::new(), &problems, &[])
                );
            }
            return Outcome::Invalid(format_problems(
                &problems,
                "Cloud config schema errors: ",
            ));
        }
    };

    if !parsed.is_object() && !annotate {
        return Outcome::Runtime(format!(
            "cloud-config {config_path} is not a YAML dict."
        ));
    }

    let schema = ci_schema::cloud_config_schema();
    let (errors, deprecations) = collect_problems(schema, &parsed);

    if annotate {
        if errors.is_empty() && deprecations.is_empty() {
            return Outcome::Validated;
        }
        let marks = ci_schema::marks::scan(content);
        println!(
            "{}",
            annotate_content(content, &marks, &errors, &deprecations)
        );
    } else if !deprecations.is_empty() {
        println!(
            "{}",
            format_problems(&deprecations, "Cloud config schema deprecations: ")
        );
    }

    if errors.is_empty() {
        Outcome::Validated
    } else {
        Outcome::Invalid(format_problems(&errors, "Cloud config schema errors: "))
    }
}

/// Run the validator and split its findings, applying the top-level property
/// rewrite from `validate_cloudconfig_schema`.
fn collect_problems(
    schema: &Schema,
    instance: &ci_config::Value,
) -> (Vec<Problem>, Vec<Problem>) {
    let mut errors = Vec::new();
    let mut deprecations = Vec::new();
    for error in schema.validate(instance) {
        let mut path = error.path_string();
        if path.is_empty()
            && error.keyword == "additionalProperties"
            && error.at_root_schema
        {
            if let Some(name) = unexpected_property(&error.message) {
                path = name;
            }
        }
        let problem = Problem {
            path,
            message: error.message,
        };
        if matches!(error.kind, Kind::Deprecation { .. }) {
            deprecations.push(problem);
        } else {
            errors.push(problem);
        }
    }
    (dedup_sorted(errors), dedup_sorted(deprecations))
}

/// `re.match(r".*\('(?P<name>.*)' was unexpected\)", message)`, which only
/// matches the single-extra form.
fn unexpected_property(message: &str) -> Option<String> {
    let rest = message.strip_suffix("' was unexpected)")?;
    let start = rest.rfind("('")?;
    Some(rest.get(start + 2..)?.to_owned())
}

/// `sorted(list(set(problems)))`.
fn dedup_sorted(mut problems: Vec<Problem>) -> Vec<Problem> {
    problems.sort();
    problems.dedup();
    problems
}

fn format_problems(problems: &[Problem], prefix: &str) -> String {
    let joined = problems
        .iter()
        .map(Problem::format)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{prefix}{joined}")
}

/// Port of `_Annotator.annotate`.
fn annotate_content(
    content: &str,
    marks: &Marks,
    errors: &[Problem],
    deprecations: &[Problem],
) -> String {
    let errors_by_line = problems_by_line(marks, errors);
    let deprecations_by_line = problems_by_line(marks, deprecations);

    let mut out: Vec<String> = Vec::new();
    let mut error_footer: Vec<String> = Vec::new();
    let mut deprecation_footer: Vec<String> = Vec::new();
    let mut error_index = 1usize;
    let mut deprecation_index = 1usize;

    for (offset, line) in content.split('\n').enumerate() {
        let number = offset + 1;
        let line_errors = errors_by_line.get(&number);
        let line_deprecations = deprecations_by_line.get(&number);
        if line_errors.is_none() && line_deprecations.is_none() {
            out.push(line.to_owned());
            continue;
        }
        let mut labels: Vec<String> = Vec::new();
        error_index = add_problems(
            line_errors.map_or(&[][..], Vec::as_slice),
            &mut labels,
            &mut error_footer,
            error_index,
            'E',
        );
        deprecation_index = add_problems(
            line_deprecations.map_or(&[][..], Vec::as_slice),
            &mut labels,
            &mut deprecation_footer,
            deprecation_index,
            'D',
        );
        out.push(format!("{line}\t\t# {}", labels.join(",")));
    }

    for (title, footer) in [
        ("Errors", &error_footer),
        ("Deprecations", &deprecation_footer),
    ] {
        if !footer.is_empty() {
            let mut section = String::new();
            let _ = write!(
                section,
                "# {title}: -------------\n{}\n\n",
                footer.join("\n")
            );
            out.push(section);
        }
    }
    out.join("\n")
}

/// `_Annotator._build_errors_by_line`, with a fallback where upstream raises
/// `KeyError` (docs/COMPAT.md B12, B13).
fn problems_by_line(
    marks: &Marks,
    problems: &[Problem],
) -> std::collections::BTreeMap<usize, Vec<String>> {
    let mut by_line: std::collections::BTreeMap<usize, Vec<String>> =
        std::collections::BTreeMap::new();
    for problem in problems {
        let line = format_position(&problem.path)
            .or_else(|| marks.get(&problem.path).copied())
            .unwrap_or(1);
        by_line
            .entry(line)
            .or_default()
            .push(problem.message.clone());
    }
    by_line
}

/// `r"format-l(?P<line>\d+)\.c(?P<col>\d+).*"`.
fn format_position(path: &str) -> Option<usize> {
    let rest = path.strip_prefix("format-l")?;
    let (line, rest) = rest.split_once(".c")?;
    if !rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    line.parse().ok()
}

fn add_problems(
    problems: &[String],
    labels: &mut Vec<String>,
    footer: &mut Vec<String>,
    mut index: usize,
    prefix: char,
) -> usize {
    for problem in problems {
        let label = format!("{prefix}{index}");
        footer.push(format!("# {label}: {problem}"));
        labels.push(label);
        index += 1;
    }
    index
}

fn yaml_position(error: &ci_config::YamlError) -> (usize, usize) {
    let ci_config::YamlError::Parse(parse) = error else {
        return (1, 1);
    };
    (parse.line(), parse.column())
}

/// The parser's own words. `PyYAML`'s wording differs; see docs/COMPAT.md.
fn yaml_reason(error: &ci_config::YamlError) -> String {
    match error {
        ci_config::YamlError::Parse(parse) => parse.to_string(),
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

    fn problem(path: &str, message: &str) -> Problem {
        Problem {
            path: path.to_owned(),
            message: message.to_owned(),
        }
    }

    #[test]
    fn detects_user_data_types_by_longest_prefix() {
        assert_eq!(
            type_from_starts_with(b"#cloud-config\nx: 1", None),
            Some("text/cloud-config")
        );
        assert_eq!(
            type_from_starts_with(b"#cloud-config-jsonp\n[]", None),
            Some("text/cloud-config-jsonp")
        );
        assert_eq!(
            type_from_starts_with(b"#!/bin/sh\n", None),
            Some("text/x-shellscript")
        );
        assert_eq!(
            type_from_starts_with(b"## template: jinja\n", None),
            Some("text/jinja2")
        );
        assert_eq!(type_from_starts_with(b"runcmd:\n", None), None);
    }

    #[test]
    fn recovers_a_single_unexpected_top_level_property() {
        assert_eq!(
            unexpected_property(
                "Additional properties are not allowed ('bogus_key_here' was unexpected)"
            )
            .as_deref(),
            Some("bogus_key_here")
        );
        // The plural form is deliberately not matched, as upstream's regex
        // requires "was unexpected".
        assert_eq!(
            unexpected_property(
                "Additional properties are not allowed ('a', 'b' were unexpected)"
            ),
            None
        );
    }

    #[test]
    fn formats_problems_the_way_the_error_line_reads() {
        let problems = vec![
            problem("bogus_key_here", "Additional properties are not allowed"),
            problem("runcmd", "5 is not of type 'array'"),
        ];
        assert_eq!(
            format_problems(&problems, "Cloud config schema errors: "),
            "Cloud config schema errors: bogus_key_here: Additional properties are not allowed, \
             runcmd: 5 is not of type 'array'"
        );
    }

    #[test]
    fn annotates_lines_and_appends_a_footer() {
        let content = "#cloud-config\nruncmd: 5\nbogus_key_here: 1\n";
        let marks = ci_schema::marks::scan(content);
        let errors = vec![
            problem("bogus_key_here", "Additional properties are not allowed"),
            problem("runcmd", "5 is not of type 'array'"),
        ];
        let annotated = annotate_content(content, &marks, &errors, &[]);
        assert_eq!(
            annotated,
            "#cloud-config\n\
             runcmd: 5\t\t# E1\n\
             bogus_key_here: 1\t\t# E2\n\
             \n\
             # Errors: -------------\n\
             # E1: 5 is not of type 'array'\n\
             # E2: Additional properties are not allowed\n\n"
        );
    }

    #[test]
    fn numbers_deprecations_separately_from_errors() {
        let content = "#cloud-config\napt_reboot_if_required: true\n";
        let marks = ci_schema::marks::scan(content);
        let deprecations = vec![problem(
            "apt_reboot_if_required",
            " Deprecated in version 22.2.",
        )];
        let annotated = annotate_content(content, &marks, &[], &deprecations);
        assert!(
            annotated.contains("apt_reboot_if_required: true\t\t# D1"),
            "{annotated}"
        );
        assert!(
            annotated.contains("# Deprecations: -------------\n# D1: "),
            "{annotated}"
        );
    }

    #[test]
    fn reads_the_line_out_of_a_format_position() {
        assert_eq!(format_position("format-l2.c1"), Some(2));
        assert_eq!(format_position("format-l12.c34"), Some(12));
        assert_eq!(format_position("runcmd"), None);
    }

    #[test]
    fn falls_back_to_line_one_for_a_root_level_error() {
        // Upstream raises KeyError here; see docs/COMPAT.md B13.
        let by_line = problems_by_line(&Marks::new(), &[problem("", "root problem")]);
        assert_eq!(
            by_line.get(&1).map(Vec::as_slice),
            Some(&["root problem".to_owned()][..])
        );
    }

    #[test]
    fn drops_duplicate_problems_and_sorts_them() {
        let problems = dedup_sorted(vec![
            problem("b", "second"),
            problem("a", "first"),
            problem("b", "second"),
        ]);
        assert_eq!(
            problems,
            vec![problem("a", "first"), problem("b", "second")]
        );
    }
}
