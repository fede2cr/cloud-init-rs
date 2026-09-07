//! Port of `cloudinit/config/cc_final_message.py`.
//!
//! The last thing a successful boot says, and the only module whose output is
//! meant for a human watching the console. It does two separate things under
//! one name: renders a template to stderr, and drops `boot-finished` in the
//! instance directory — the file everything from `cloud-init status` to a
//! tenant's own `until-cloud-init-is-done` loop watches for.
//!
//! Nothing here is allowed to fail the boot. A template that will not parse, a
//! `boot-finished` that cannot be written: both are logged and stepped over,
//! because a module that runs `PER_ALWAYS` at the very end has nothing left to
//! protect and everything to lose by aborting.

use ci_config::{Object, Value};
use ci_sys::atomic::{self, WriteOptions};

use super::Args;

const SOURCE: &str = "cc_final_message.py";

/// Upstream's `FINAL_MESSAGE_DEF`, spacing included: one space after the
/// timestamp's full stop and two before `Up`.
const FINAL_MESSAGE_DEF: &str = concat!(
    "## template: jinja\n",
    "Cloud-init v. {{version}} finished at {{timestamp}}.",
    " Datasource {{datasource}}.  Up {{uptime}} seconds",
);

pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let configured = match args.args.as_array().and_then(|list| list.first()) {
        Some(first) => super::py_str(first),
        // `util.get_cfg_option_str(cfg, "final_message", "")`: a non-string
        // value is stringified rather than refused.
        None => match args.cfg.get("final_message") {
            Some(Value::String(text)) => text.clone(),
            Some(other) => super::py_str(other),
            None => String::new(),
        },
    };
    // Python's `str.strip()` also eats \x1c-\x1f, which `char::is_whitespace`
    // does not.
    let trimmed = configured.trim_matches(|c: char| {
        c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
    });
    let template = if trimmed.is_empty() {
        FINAL_MESSAGE_DEF
    } else {
        trimmed
    };

    let uptime = ci_core::time::uptime();
    // `util.time_rfc2822`, which is the format `status --long` already uses.
    let timestamp = ci_core::time::format_last_update(ci_core::time::now_epoch());
    let version = ci_core::version::version_string();
    // `str(cloud.datasource)`. With no datasource at all this is `str(None)`,
    // which is where the literal below comes from.
    let datasource = args
        .datasource
        .map_or("None", |ds| ds.class_name)
        .to_owned();

    let mut subs = Object::new();
    for (key, value) in [
        ("uptime", &uptime),
        ("timestamp", &timestamp),
        ("version", &version),
        ("datasource", &datasource),
    ] {
        subs.insert(key.to_owned(), Value::String(value.clone()));
        subs.insert(key.to_uppercase(), Value::String(value.clone()));
    }

    match ci_template::render_string(template, &Value::Object(subs)) {
        // `multi_log(console=False, stderr=True)`: straight to stderr, with no
        // log formatting, because this line is the module's whole purpose.
        Ok(rendered) => {
            eprintln!("{rendered}");
            args.debug(SOURCE, &rendered);
        }
        Err(err) if err.is_syntax_error() => {
            let message = format!("Failed to render templated final message: {err}");
            args.warning(SOURCE, &message);
        }
        Err(_) => args.warning(SOURCE, "Failed to render final message template"),
    }

    let boot_finished = args.paths.boot_finished();
    let contents = format!("{uptime} - {timestamp} - v. {version}\n");
    // `ensure_dir_exists=False`: no instance directory means no file, and the
    // stage carries on regardless.
    if atomic::write_file(&boot_finished, contents, WriteOptions::mode(0o644)).is_err()
    {
        let message = format!(
            "Failed to write boot finished file {}",
            boot_finished.display()
        );
        args.warning(SOURCE, &message);
    }

    let Some(ds) = args.datasource else {
        return Err("'NoneType' object has no attribute 'dsname'".to_owned());
    };
    let only_none = Value::Array(vec![Value::String("None".to_owned())]);
    if ds.dsname == "None" && ds.sys_cfg.get("datasource_list") != Some(&only_none) {
        args.warning(SOURCE, "Used fallback datasource");
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use std::path::Path;

    use ci_log::Logger;
    use serde_json::json;

    use super::*;
    use crate::cc::Datasource;

    /// A `log_cfgs` entry pointing the real logger at a file, so a test can
    /// read back what the module logged. Everything this module reports —
    /// the message itself included — is a log line or a file, so there is
    /// nothing else to assert on.
    fn logger_to(path: &Path) -> Logger {
        let ini = format!(
            "[loggers]\nkeys=root\n\n[handlers]\nkeys=h\n\n[formatters]\nkeys=f\n\n\
             [logger_root]\nlevel=DEBUG\nhandlers=h\n\n\
             [formatter_f]\nformat=%(filename)s[%(levelname)s]: %(message)s\n\n\
             [handler_h]\nclass=FileHandler\nlevel=DEBUG\nformatter=f\n\
             args=('{}', 'a')\n",
            path.display()
        );
        let mut cfg = Object::new();
        cfg.insert(
            "log_cfgs".to_owned(),
            Value::Array(vec![Value::String(ini)]),
        );
        Logger::from_config(&cfg)
    }

    struct Case {
        cfg: serde_json::Value,
        module_args: Value,
        dsname: &'static str,
        sys_cfg: serde_json::Value,
        /// Whether the instance directory exists before the module runs.
        /// `boot-finished` goes inside it and nothing creates it here, so a
        /// stage that never got that far is the interesting case.
        instance_dir: bool,
    }

    impl Case {
        fn new(cfg: serde_json::Value) -> Self {
            Self {
                cfg,
                module_args: Value::Array(Vec::new()),
                dsname: "NoCloud",
                sys_cfg: json!({}),
                instance_dir: true,
            }
        }
    }

    /// Runs the module under `root`, returning its outcome and its log.
    fn run(root: &Path, case: &Case) -> (Result<(), String>, Vec<String>) {
        let mut cfg =
            json!({"system_info": {"paths": {"cloud_dir": root.join("cloud")}}})
                .as_object()
                .unwrap()
                .clone();
        for (key, value) in case.cfg.as_object().unwrap() {
            cfg.insert(key.clone(), value.clone());
        }
        let paths = ci_core::Paths::from_config(&cfg);
        if case.instance_dir {
            std::fs::create_dir_all(paths.instance_link()).unwrap();
        }

        let log_file = root.join("cloud-init.log");
        let sys_cfg = case.sys_cfg.as_object().unwrap().clone();
        let metadata = Object::new();
        let mut logger = logger_to(&log_file);
        let outcome = handle(&mut Args {
            system_info: crate::cc::tests::no_system_info(),
            name: "final_message",
            cfg: &cfg,
            args: &case.module_args,
            paths: &paths,
            root,
            distro: crate::cc::tests::fixture_distro(),
            datasource: Some(Datasource {
                class_name: "DataSourceNoCloudNet",
                dsname: case.dsname,
                instance_id: "i-test",
                metadata: &metadata,
                sys_cfg: &sys_cfg,
                public_keys: &[],
            }),
            logger: &mut logger,
        });
        logger.flush();
        let written = std::fs::read_to_string(&log_file).unwrap_or_default();
        let _ = std::fs::remove_file(&log_file);
        let lines = written
            .lines()
            .filter_map(|line| line.split_once(": "))
            .map(|(_, message)| message.to_owned())
            .collect();
        (outcome, lines)
    }

    fn boot_finished(root: &Path) -> String {
        std::fs::read_to_string(root.join("cloud/instance/boot-finished")).unwrap()
    }

    #[test]
    fn the_default_template_names_the_datasource_class() {
        let dir = tempfile::tempdir().unwrap();
        let (outcome, log) = run(dir.path(), &Case::new(json!({})));
        assert_eq!(outcome, Ok(()));
        let rendered = log.last().unwrap();
        assert!(
            rendered.contains("Datasource DataSourceNoCloudNet.  Up "),
            "{rendered}"
        );
        assert!(
            rendered.starts_with("Cloud-init v. 26.1 finished at "),
            "{rendered}"
        );
    }

    #[test]
    fn boot_finished_records_uptime_timestamp_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let (outcome, _) = run(dir.path(), &Case::new(json!({})));
        assert_eq!(outcome, Ok(()));
        let contents = boot_finished(dir.path());
        assert!(contents.ends_with(" - v. 26.1\n"), "{contents}");
        assert_eq!(contents.matches(" - ").count(), 2, "{contents}");
    }

    #[test]
    fn a_configured_message_replaces_the_default_and_sees_both_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let case = Case::new(json!({"final_message": "  ${datasource}/${VERSION}  "}));
        let (outcome, log) = run(dir.path(), &case);
        assert_eq!(outcome, Ok(()));
        assert_eq!(log.last().unwrap(), "DataSourceNoCloudNet/26.1");
    }

    #[test]
    fn a_whitespace_only_message_falls_back_to_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let (_, log) = run(dir.path(), &Case::new(json!({"final_message": " \n\t"})));
        assert!(log.last().unwrap().starts_with("Cloud-init v. "));
    }

    #[test]
    fn module_args_win_over_the_config_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut case = Case::new(json!({"final_message": "from config"}));
        case.module_args = json!(["from args", "ignored"]);
        let (_, log) = run(dir.path(), &case);
        assert_eq!(log.last().unwrap(), "from args");
    }

    #[test]
    fn a_broken_template_is_logged_but_does_not_fail_the_module() {
        let dir = tempfile::tempdir().unwrap();
        let case = Case::new(json!({
            "final_message": "## template: jinja\n{% for x in %}",
        }));
        let (outcome, log) = run(dir.path(), &case);
        assert_eq!(outcome, Ok(()));
        assert!(
            log.iter()
                .any(|line| line
                    .starts_with("Failed to render templated final message: ")),
            "{log:?}"
        );
        // The file still lands: the two halves are independent.
        assert!(boot_finished(dir.path()).ends_with(" - v. 26.1\n"));
    }

    #[test]
    fn a_missing_instance_directory_is_logged_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut case = Case::new(json!({}));
        case.instance_dir = false;
        let (outcome, log) = run(dir.path(), &case);
        assert_eq!(outcome, Ok(()));
        assert!(
            log.iter()
                .any(|line| line.starts_with("Failed to write boot finished file ")),
            "{log:?}"
        );
    }

    #[test]
    fn the_fallback_datasource_warning_is_only_for_an_unconfigured_none() {
        let dir = tempfile::tempdir().unwrap();
        let warned = |case: &Case| {
            run(dir.path(), case)
                .1
                .iter()
                .any(|line| line == "Used fallback datasource")
        };

        let mut case = Case::new(json!({}));
        case.dsname = "None";
        assert!(warned(&case));

        case.sys_cfg = json!({"datasource_list": ["None"]});
        assert!(!warned(&case));

        case.sys_cfg = json!({"datasource_list": ["NoCloud", "None"]});
        assert!(warned(&case));

        case.dsname = "NoCloud";
        assert!(!warned(&case));
    }
}
