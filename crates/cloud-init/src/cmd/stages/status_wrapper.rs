//! Port of `cmd/main.status_wrapper`: the `status.json` and `result.json`
//! bookkeeping that wraps every boot stage.
//!
//! The canonical copies live under `cloud_dir/data`; `run_dir` gets relative
//! symlinks to them, which is what `cloud-init status` reads.

use std::path::{Path, PathBuf};

use ci_core::Paths;
use ci_log::Logger;
use serde_json::{Map, Value};

/// The four stages recorded in `status.json`. `modules --mode=init` is
/// deliberately absent: upstream does not wrap it, because it is not a real
/// boot stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    InitLocal,
    Init,
    ModulesConfig,
    ModulesFinal,
}

/// Order matters only for `result.json`, which collects errors by walking the
/// stages in the order upstream's dict holds them.
const MODES: [Mode; 4] = [
    Mode::Init,
    Mode::InitLocal,
    Mode::ModulesConfig,
    Mode::ModulesFinal,
];

impl Mode {
    pub fn key(self) -> &'static str {
        match self {
            Self::InitLocal => "init-local",
            Self::Init => "init",
            Self::ModulesConfig => "modules-config",
            Self::ModulesFinal => "modules-final",
        }
    }

    /// The name the welcome message reports.
    pub fn bootstage_name(self) -> &'static str {
        match self {
            Self::InitLocal => "init-local",
            Self::Init => "init",
            Self::ModulesConfig => "modules:config",
            Self::ModulesFinal => "modules:final",
        }
    }

    /// The name the restart warning reports.
    pub fn stage_name(self) -> &'static str {
        match self {
            Self::InitLocal => "Local Stage",
            Self::Init => "Network Stage",
            Self::ModulesConfig => "Config Stage",
            Self::ModulesFinal => "Final Stage",
        }
    }
}

/// What a stage body reports back to the wrapper.
#[derive(Debug, Default)]
pub struct Outcome {
    pub datasource: Option<String>,
    pub errors: Vec<String>,
}

/// Run `body` as `mode`, recording the run in `status.json`.
///
/// Returns the number of errors the stage reported, which upstream uses as the
/// process exit code.
pub fn wrap(
    mode: Mode,
    paths: &Paths,
    logger: &mut Logger,
    body: impl FnOnce(&mut Logger) -> Outcome,
) -> u8 {
    let data_d = paths.data_dir();
    let link_d = paths.run_dir.clone();
    let status_path = data_d.join("status.json");
    let status_link = link_d.join("status.json");
    let result_path = data_d.join("result.json");
    let result_link = link_d.join("result.json");

    for dir in [&data_d, &link_d] {
        let _ = ci_sys::path::ensure_dir(dir, 0o755);
    }

    let loaded = if mode == Mode::InitLocal {
        for stale in [&status_link, &result_link, &status_path, &result_path] {
            let _ = std::fs::remove_file(stale);
        }
        None
    } else {
        read_v1(&status_path)
    };
    // A freshly built `v1` has all four stage entries sharing one `errors` list
    // and one `recoverable_errors` dict, because upstream builds them with
    // `nullstatus.copy()` — a shallow copy (B63). Loading from disk separates
    // them, so only this process's own writes alias.
    let aliased = loaded.is_none();
    let mut v1 = loaded.unwrap_or_else(blank);

    v1.insert("stage".to_owned(), Value::String(mode.key().to_owned()));
    if started_but_never_finished(&v1, mode) {
        logger.warning(
            "main.py",
            &format!(
                "Unexpected start time found for {}. Was this stage restarted?",
                mode.stage_name()
            ),
        );
    }
    set_field(&mut v1, mode, "start", uptime());
    write_json(&status_path, &wrap_v1(&v1));
    link_relative(&status_path, &status_link, &link_d);

    // Who started this boot? `init-local` claims it; the later stages check
    // (PLAN.md §6.7). A mid-boot implementation switch makes everything below
    // this point untrustworthy, so the body does not run — but the stage is
    // still recorded as started and finished with an error, because a stage
    // that vanishes from `status.json` is harder to diagnose than one that
    // says why it stopped.
    let claim =
        ci_core::impl_marker::claim(&paths.impl_marker(), mode == Mode::InitLocal);
    let outcome = if let Some(message) = claim.message().filter(|_| claim.is_fatal()) {
        logger.error("main.py", &message);
        eprintln!("{message}");
        Outcome {
            datasource: None,
            errors: vec![message],
        }
    } else {
        body(logger)
    };
    if let Some(datasource) = outcome.datasource {
        v1.insert("datasource".to_owned(), Value::String(datasource));
    }
    let mut errors = stage_errors(&v1, mode);
    errors.extend(outcome.errors);
    set_shared_field(
        &mut v1,
        mode,
        aliased,
        "errors",
        &Value::Array(errors.iter().cloned().map(Value::String).collect()),
    );
    set_field(&mut v1, mode, "finished", uptime());
    set_shared_field(
        &mut v1,
        mode,
        aliased,
        "recoverable_errors",
        &logger.recoverable_errors(),
    );
    logger.clear_recoverable_errors();
    v1.insert("stage".to_owned(), Value::Null);
    write_json(&status_path, &wrap_v1(&v1));

    if mode == Mode::ModulesFinal {
        let all: Vec<Value> = MODES
            .iter()
            .flat_map(|m| stage_errors(&v1, *m))
            .map(Value::String)
            .collect();
        let datasource = v1.get("datasource").cloned().unwrap_or(Value::Null);
        write_json(
            &result_path,
            &serde_json::json!({"v1": {"datasource": datasource, "errors": all}}),
        );
        link_relative(&result_path, &result_link, &link_d);
    }

    u8::try_from(errors.len()).unwrap_or(u8::MAX)
}

fn set_field(v1: &mut Map<String, Value>, mode: Mode, field: &str, value: Value) {
    let entry = v1.entry(mode.key().to_owned()).or_insert_with(null_status);
    if let Some(entry) = entry.as_object_mut() {
        entry.insert(field.to_owned(), value);
    }
}

/// Upstream mutates `errors` and `recoverable_errors` in place rather than
/// assigning them, so while the entries are still aliased every stage sees the
/// write (B63). `start` and `finished` are assigned, and stay per-stage.
fn set_shared_field(
    v1: &mut Map<String, Value>,
    mode: Mode,
    aliased: bool,
    field: &str,
    value: &Value,
) {
    if aliased {
        for shared in MODES {
            set_field(v1, shared, field, value.clone());
        }
    } else {
        set_field(v1, mode, field, value.clone());
    }
}

fn started_but_never_finished(v1: &Map<String, Value>, mode: Mode) -> bool {
    let Some(entry) = v1.get(mode.key()).and_then(Value::as_object) else {
        return false;
    };
    // Upstream tests the uptimes for truthiness, so an uptime of exactly 0.0
    // reads as "never started".
    let set = |field: &str| {
        entry
            .get(field)
            .and_then(Value::as_f64)
            .is_some_and(|uptime| uptime != 0.0)
    };
    set("start") && !set("finished")
}

fn stage_errors(v1: &Map<String, Value>, mode: Mode) -> Vec<String> {
    v1.get(mode.key())
        .and_then(|entry| entry.get("errors"))
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|e| e.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn null_status() -> Value {
    serde_json::json!({
        "errors": [],
        "recoverable_errors": {},
        "start": Value::Null,
        "finished": Value::Null,
    })
}

fn blank() -> Map<String, Value> {
    let mut v1 = Map::new();
    v1.insert("datasource".to_owned(), Value::Null);
    for mode in MODES {
        v1.insert(mode.key().to_owned(), null_status());
    }
    v1
}

fn wrap_v1(v1: &Map<String, Value>) -> Value {
    serde_json::json!({ "v1": Value::Object(v1.clone()) })
}

/// A `status.json` that is not the shape this stage expects is treated as
/// absent. Upstream dies on the `KeyError` instead.
fn read_v1(path: &Path) -> Option<Map<String, Value>> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get("v1").and_then(Value::as_object).cloned())
}

fn uptime() -> Value {
    ci_core::time::uptime()
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map_or(Value::Null, Value::Number)
}

fn write_json(path: &Path, value: &Value) {
    let text = format!("{}\n", ci_core::jsonfmt::json_dumps(value));
    let _ = ci_sys::atomic::write_file(
        path,
        text.as_bytes(),
        ci_sys::atomic::WriteOptions::PUBLIC,
    );
}

fn link_relative(target: &Path, link: &Path, base: &Path) {
    let _ = ci_sys::path::sym_link(relative_to(target, base), link, true);
}

/// `os.path.relpath`, which is lexical: neither path is resolved.
fn relative_to(target: &Path, base: &Path) -> PathBuf {
    let mut target_parts = target.components().peekable();
    let mut base_parts = base.components().peekable();
    while target_parts.peek().is_some() && target_parts.peek() == base_parts.peek() {
        target_parts.next();
        base_parts.next();
    }
    let mut out = PathBuf::new();
    for _ in base_parts {
        out.push("..");
    }
    out.extend(target_parts);
    out
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

    fn paths(dir: &Path) -> Paths {
        Paths {
            cloud_dir: dir.join("var"),
            run_dir: dir.join("run"),
            ..Paths::default()
        }
    }

    fn status_of(paths: &Paths) -> Value {
        let text =
            std::fs::read_to_string(paths.data_dir().join("status.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// Most of these tests do not care about the log, so they get their own
    /// throwaway one.
    fn wrap(mode: Mode, paths: &Paths, body: impl FnOnce() -> Outcome) -> u8 {
        super::wrap(mode, paths, &mut Logger::silent(), |_| body())
    }

    #[test]
    fn a_restarted_stage_is_warned_about_and_the_warning_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut logger = Logger::silent();

        // A stage that started and never finished, as an interrupted boot
        // leaves it.
        super::wrap(Mode::Init, &paths, &mut logger, |_| Outcome::default());
        let mut status = status_of(&paths);
        status["v1"]["init"]["finished"] = Value::Null;
        std::fs::write(
            paths.data_dir().join("status.json"),
            serde_json::to_string(&status).unwrap(),
        )
        .unwrap();

        super::wrap(Mode::Init, &paths, &mut logger, |_| Outcome::default());

        assert_eq!(
            status_of(&paths)["v1"]["init"]["recoverable_errors"],
            serde_json::json!({
                "WARNING": [
                    "Unexpected start time found for Network Stage. \
                     Was this stage restarted?"
                ]
            })
        );
    }

    #[test]
    fn each_stage_records_only_the_warnings_logged_during_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut logger = Logger::silent();

        super::wrap(Mode::InitLocal, &paths, &mut logger, |logger| {
            logger.warning("main.py", "local trouble");
            Outcome::default()
        });
        super::wrap(Mode::Init, &paths, &mut logger, |logger| {
            logger.warning("main.py", "network trouble");
            Outcome::default()
        });

        let status = status_of(&paths);
        assert_eq!(
            status["v1"]["init-local"]["recoverable_errors"],
            serde_json::json!({"WARNING": ["local trouble"]})
        );
        assert_eq!(
            status["v1"]["init"]["recoverable_errors"],
            serde_json::json!({"WARNING": ["network trouble"]})
        );
    }

    /// Upstream's `nullstatus.copy()` is shallow, so an error recorded by the
    /// stage that creates `status.json` lands in all four entries (B63).
    #[test]
    fn an_error_in_init_local_is_recorded_against_every_stage() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());

        let code = wrap(Mode::InitLocal, &paths, || Outcome {
            datasource: None,
            errors: vec!["no distro".to_owned()],
        });

        // The exit code is the length of the stage's own list, which is one:
        // the aliasing shares the list, it does not concatenate.
        assert_eq!(code, 1);
        let status = status_of(&paths);
        for mode in MODES {
            assert_eq!(
                status["v1"][mode.key()]["errors"],
                serde_json::json!(["no distro"]),
                "{}",
                mode.key()
            );
        }

        // Reading it back separates the entries again, so the next stage
        // appends to its own copy and exits 2.
        let code = wrap(Mode::Init, &paths, || Outcome {
            datasource: None,
            errors: vec!["no datasource".to_owned()],
        });

        assert_eq!(code, 2);
        let status = status_of(&paths);
        assert_eq!(
            status["v1"]["init"]["errors"],
            serde_json::json!(["no distro", "no datasource"])
        );
        assert_eq!(
            status["v1"]["init-local"]["errors"],
            serde_json::json!(["no distro"])
        );
    }

    #[test]
    fn a_clean_stage_records_no_errors_and_exits_zero() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());

        let code = wrap(Mode::InitLocal, &paths, Outcome::default);

        assert_eq!(code, 0);
        let status = status_of(&paths);
        assert_eq!(status["v1"]["stage"], Value::Null);
        assert_eq!(status["v1"]["init-local"]["errors"], serde_json::json!([]));
        assert!(status["v1"]["init-local"]["start"].is_number());
        assert!(status["v1"]["init-local"]["finished"].is_number());
    }

    #[test]
    fn the_exit_code_is_the_number_of_errors() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());

        let code = wrap(Mode::Init, &paths, || Outcome {
            datasource: None,
            errors: vec!["one".to_owned(), "two".to_owned()],
        });

        assert_eq!(code, 2);
        assert_eq!(
            status_of(&paths)["v1"]["init"]["errors"],
            serde_json::json!(["one", "two"])
        );
    }

    #[test]
    fn run_dir_gets_a_relative_link_to_the_canonical_copy() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());

        wrap(Mode::InitLocal, &paths, Outcome::default);

        let link = std::fs::read_link(paths.status_file()).unwrap();
        assert_eq!(link, Path::new("../var/data/status.json"));
    }

    #[test]
    fn the_local_stage_starts_a_fresh_record() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());

        wrap(Mode::Init, &paths, || Outcome {
            datasource: Some("DataSourceNone".to_owned()),
            errors: vec!["stale".to_owned()],
        });
        wrap(Mode::InitLocal, &paths, Outcome::default);

        let status = status_of(&paths);
        assert_eq!(status["v1"]["init"]["errors"], serde_json::json!([]));
        assert_eq!(status["v1"]["datasource"], Value::Null);
    }

    #[test]
    fn a_later_stage_keeps_what_the_earlier_ones_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());

        wrap(Mode::InitLocal, &paths, Outcome::default);
        wrap(Mode::Init, &paths, || Outcome {
            datasource: Some("DataSourceNoCloud".to_owned()),
            errors: vec!["network".to_owned()],
        });
        wrap(Mode::ModulesConfig, &paths, Outcome::default);

        let status = status_of(&paths);
        assert_eq!(status["v1"]["datasource"], "DataSourceNoCloud");
        assert_eq!(
            status["v1"]["init"]["errors"],
            serde_json::json!(["network"])
        );
    }

    #[test]
    fn the_final_stage_writes_every_error_to_result_json() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());

        wrap(Mode::InitLocal, &paths, Outcome::default);
        wrap(Mode::Init, &paths, || Outcome {
            datasource: None,
            errors: vec!["no datasource".to_owned()],
        });
        wrap(Mode::ModulesConfig, &paths, Outcome::default);
        wrap(Mode::ModulesFinal, &paths, || Outcome {
            datasource: None,
            errors: vec!["late".to_owned()],
        });

        let text =
            std::fs::read_to_string(paths.data_dir().join("result.json")).unwrap();
        let result: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            result["v1"]["errors"],
            serde_json::json!(["no datasource", "late"])
        );
        assert_eq!(
            std::fs::read_link(paths.result_file()).unwrap(),
            Path::new("../var/data/result.json")
        );
    }

    #[test]
    fn a_corrupt_status_file_is_replaced_rather_than_read() {
        for corrupt in [&b"not json"[..], b"{\"foo\": 1}", b"null", b"[]", b""] {
            let dir = tempfile::tempdir().unwrap();
            let paths = paths(dir.path());
            ci_sys::path::ensure_dir(paths.data_dir(), 0o755).unwrap();
            std::fs::write(paths.data_dir().join("status.json"), corrupt).unwrap();

            let code = wrap(Mode::ModulesFinal, &paths, Outcome::default);

            assert_eq!(code, 0);
            assert_eq!(
                status_of(&paths)["v1"]["modules-final"]["errors"],
                serde_json::json!([])
            );
        }
    }
}
