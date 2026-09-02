//! Port of `cloudinit/cmd/status.py`.
//!
//! `cloud-init status` is the contract other tooling (cloud images, CI, support
//! scripts) depends on most, including its exit codes: 0 healthy, 1 error,
//! 2 degraded.

use std::path::Path;
use std::time::Duration;

use ci_config::{Object, Value};

use crate::paths::Paths;
use crate::time::format_last_update;

/// `settings`-level marker that disables cloud-init entirely.
pub const CLOUDINIT_DISABLED_FILE: &str = "/etc/cloud/cloud-init.disabled";

const SYSTEMD_UNITS: [&str; 5] = [
    "cloud-final.service",
    "cloud-config.service",
    "cloud-init-network.service",
    "cloud-init-local.service",
    "cloud-init-main.service",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunningStatus {
    NotStarted,
    Running,
    Done,
    Disabled,
}

impl RunningStatus {
    pub fn value(self) -> &'static str {
        match self {
            Self::NotStarted => "not started",
            Self::Running => "running",
            Self::Done => "done",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConditionStatus {
    Error,
    Degraded,
    Peachy,
}

impl ConditionStatus {
    pub fn value(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Degraded => "degraded",
            Self::Peachy => "healthy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnabledStatus {
    DisabledByGenerator,
    DisabledByKernelCmdline,
    DisabledByMarkerFile,
    DisabledByEnvVariable,
    EnabledByGenerator,
    EnabledByKernelCmdline,
    EnabledBySysvinit,
    Unknown,
}

impl EnabledStatus {
    pub fn value(self) -> &'static str {
        match self {
            Self::DisabledByGenerator => "disabled-by-generator",
            Self::DisabledByKernelCmdline => "disabled-by-kernel-command-line",
            Self::DisabledByMarkerFile => "disabled-by-marker-file",
            Self::DisabledByEnvVariable => "disabled-by-environment-variable",
            Self::EnabledByGenerator => "enabled-by-generator",
            Self::EnabledByKernelCmdline => "enabled-by-kernel-command-line",
            Self::EnabledBySysvinit => "enabled-by-sysvinit",
            Self::Unknown => "unknown",
        }
    }

    pub fn is_disabled(self) -> bool {
        matches!(
            self,
            Self::DisabledByGenerator
                | Self::DisabledByKernelCmdline
                | Self::DisabledByMarkerFile
                | Self::DisabledByEnvVariable
        )
    }
}

/// Everything `cloud-init status` needs to render its output.
#[derive(Debug, Clone)]
pub struct StatusDetails {
    pub running_status: RunningStatus,
    pub condition_status: ConditionStatus,
    pub boot_status_code: EnabledStatus,
    pub description: String,
    pub errors: Vec<String>,
    pub recoverable_errors: Object,
    pub last_update: String,
    pub datasource: String,
    /// Remaining `v1` keys, spliced into the reported mapping.
    pub v1: Object,
}

impl StatusDetails {
    /// `translate_status()` — `(status, extended_status)`.
    pub fn translate(&self) -> (String, String) {
        let running = self.running_status.value();
        let condition = self.condition_status.value();
        match self.condition_status {
            ConditionStatus::Error => {
                ("error".to_owned(), format!("{condition} - {running}"))
            }
            ConditionStatus::Degraded
                if matches!(
                    self.running_status,
                    RunningStatus::Done | RunningStatus::Running
                ) =>
            {
                (running.to_owned(), format!("{condition} {running}"))
            }
            _ => (running.to_owned(), running.to_owned()),
        }
    }

    /// Process exit code: 1 for errors, 2 for recoverable errors, else 0.
    pub fn exit_code(&self) -> i32 {
        match self.condition_status {
            ConditionStatus::Error => 1,
            ConditionStatus::Degraded => 2,
            ConditionStatus::Peachy => 0,
        }
    }

    pub fn is_settled(&self) -> bool {
        !matches!(
            self.running_status,
            RunningStatus::NotStarted | RunningStatus::Running
        )
    }
}

/// `get_status_details()`.
pub fn get_status_details(paths: &Paths, wait: bool) -> StatusDetails {
    let mut condition_status = ConditionStatus::Peachy;
    let status_file = paths.status_file();
    let result_file = paths.result_file();
    let (boot_status_code, boot_description) =
        get_bootstatus(Path::new(CLOUDINIT_DISABLED_FILE), paths, wait);

    let mut status_v1 = read_status_v1(&status_file);
    let datasource = get_datasource(&status_v1);
    let mut description = get_description(&status_v1, &boot_description);

    let latest_event = get_latest_event(&status_v1);
    let last_update = if latest_event > 0.0 {
        format_last_update(latest_event)
    } else {
        String::new()
    };

    let (mut errors, recoverable_errors) = get_errors(&status_v1);
    if !errors.is_empty() {
        condition_status = ConditionStatus::Error;
    } else if !recoverable_errors.is_empty() {
        condition_status = ConditionStatus::Degraded;
    }

    let mut running_status =
        get_running_status(&status_file, &result_file, boot_status_code, latest_event);

    if running_status == RunningStatus::Running
        && uses_systemd()
        && systemd_failed(wait)
    {
        running_status = RunningStatus::Done;
        condition_status = ConditionStatus::Error;
        "Failed due to systemd unit failure".clone_into(&mut description);
        errors.push(
            "Failed due to systemd unit failure. Ensure all cloud-init services are \
             enabled, and check 'systemctl' or 'journalctl' for more information."
                .to_owned(),
        );
    }

    // Reported separately; upstream drops the duplicate before splicing v1.
    status_v1.remove("datasource");

    StatusDetails {
        running_status,
        condition_status,
        boot_status_code,
        description,
        errors,
        recoverable_errors,
        last_update,
        datasource,
        v1: status_v1,
    }
}

fn read_status_v1(status_file: &Path) -> Object {
    let Ok(Some(text)) = ci_sys::path::read_text_optional(status_file, 8 * 1024 * 1024)
    else {
        return Object::new();
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.get("v1").and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

/// `get_bootstatus()`.
pub fn get_bootstatus(
    disable_file: &Path,
    paths: &Paths,
    wait: bool,
) -> (EnabledStatus, String) {
    let cmdline = ci_config::cmdline::get_cmdline();
    let has = |token: &str| cmdline.split_whitespace().any(|p| p == token);

    if !uses_systemd() {
        return (
            EnabledStatus::EnabledBySysvinit,
            "Cloud-init enabled on sysvinit".to_owned(),
        );
    }
    if has("cloud-init=enabled") {
        return (
            EnabledStatus::EnabledByKernelCmdline,
            "Cloud-init enabled by kernel command line cloud-init=enabled".to_owned(),
        );
    }
    if disable_file.exists() {
        return (
            EnabledStatus::DisabledByMarkerFile,
            format!("Cloud-init disabled by {}", disable_file.display()),
        );
    }
    if has("cloud-init=disabled") {
        return (
            EnabledStatus::DisabledByKernelCmdline,
            "Cloud-init disabled by kernel parameter cloud-init=disabled".to_owned(),
        );
    }
    let env_disabled = std::env::var("KERNEL_CMDLINE")
        .is_ok_and(|v| v.contains("cloud-init=disabled"))
        || disabled_via_environment(wait);
    if env_disabled {
        return (
            EnabledStatus::DisabledByEnvVariable,
            "Cloud-init disabled by environment variable \
             KERNEL_CMDLINE=cloud-init=disabled"
                .to_owned(),
        );
    }
    if paths.run_dir.join("disabled").exists() {
        return (
            EnabledStatus::DisabledByGenerator,
            "Cloud-init disabled by cloud-init-generator".to_owned(),
        );
    }
    if paths.run_dir.join("enabled").exists() {
        return (
            EnabledStatus::EnabledByGenerator,
            "Cloud-init enabled by systemd cloud-init-generator".to_owned(),
        );
    }
    (
        EnabledStatus::Unknown,
        "Systemd generator may not have run yet.".to_owned(),
    )
}

fn get_running_status(
    status_file: &Path,
    result_file: &Path,
    boot_status_code: EnabledStatus,
    latest_event: f64,
) -> RunningStatus {
    if boot_status_code.is_disabled() {
        RunningStatus::Disabled
    } else if status_file.exists() && !result_file.exists() {
        RunningStatus::Running
    } else if latest_event > 0.0 {
        RunningStatus::Done
    } else {
        RunningStatus::NotStarted
    }
}

/// `"DataSourceNoCloud [seed=...]"` becomes `"nocloud"`.
fn get_datasource(status_v1: &Object) -> String {
    status_v1
        .get("datasource")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|ds| {
            ds.split(' ')
                .next()
                .unwrap_or(ds)
                .to_lowercase()
                .replace("datasource", "")
        })
        .unwrap_or_default()
}

fn get_description(status_v1: &Object, boot_description: &str) -> String {
    if let Some(ds) = status_v1.get("datasource").and_then(Value::as_str) {
        if !ds.is_empty() {
            return ds.to_owned();
        }
    }
    if let Some(stage) = status_v1.get("stage").and_then(Value::as_str) {
        return format!("Running in stage: {stage}");
    }
    boot_description.to_owned()
}

fn get_latest_event(status_v1: &Object) -> f64 {
    let mut latest = 0.0f64;
    for stage in status_v1.values().filter_map(Value::as_object) {
        for key in ["start", "finished"] {
            if let Some(t) = stage.get(key).and_then(Value::as_f64) {
                latest = latest.max(t);
            }
        }
    }
    latest
}

fn get_errors(status_v1: &Object) -> (Vec<String>, Object) {
    let mut keys: Vec<&String> = status_v1.keys().collect();
    keys.sort();

    let mut errors = Vec::new();
    let mut recoverable = Object::new();
    for key in keys {
        let Some(stage) = status_v1.get(key).and_then(Value::as_object) else {
            continue;
        };
        if let Some(list) = stage.get("errors").and_then(Value::as_array) {
            errors.extend(list.iter().map(value_to_string));
        }
        let Some(current) = stage.get("recoverable_errors").and_then(Value::as_object)
        else {
            continue;
        };
        for (err_type, items) in current {
            match recoverable.get_mut(err_type).and_then(Value::as_array_mut) {
                Some(existing) => {
                    if let Some(more) = items.as_array() {
                        existing.extend(more.iter().cloned());
                    }
                }
                None => {
                    recoverable.insert(err_type.clone(), items.clone());
                }
            }
        }
    }
    (errors, recoverable)
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub fn uses_systemd() -> bool {
    Path::new("/run/systemd/system").is_dir()
}

fn disabled_via_environment(wait: bool) -> bool {
    query_systemctl(&["show-environment"], wait)
        .is_some_and(|out| out.contains("cloud-init=disabled"))
}

/// `systemd_failed()` — whether any cloud-init unit reports a failure.
fn systemd_failed(wait: bool) -> bool {
    for service in SYSTEMD_UNITS {
        let Some(stdout) = query_systemctl(
            &[
                "show",
                "--property=ActiveState,UnitFileState,SubState,MainPID",
                service,
            ],
            wait,
        ) else {
            // Systemd is not ready; assume nothing rather than reporting a failure.
            return false;
        };
        let states: std::collections::HashMap<&str, &str> = stdout
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();
        let unit_file_state = states.get("UnitFileState").copied().unwrap_or_default();
        let active_state = states.get("ActiveState").copied().unwrap_or_default();
        let sub_state = states.get("SubState").copied().unwrap_or_default();
        let main_pid = states.get("MainPID").copied().unwrap_or_default();

        if !(unit_file_state.starts_with("enabled") || unit_file_state == "static") {
            return true;
        }
        if active_state == "active" {
            if sub_state == "exited" {
                continue;
            }
            if sub_state == "running" && main_pid == "0" {
                return false;
            }
        } else if active_state == "failed" || sub_state == "failed" {
            return true;
        }
        return false;
    }
    false
}

fn query_systemctl(args: &[&str], wait: bool) -> Option<String> {
    loop {
        let mut command = vec!["systemctl"];
        command.extend_from_slice(args);
        match ci_sys::Subp::new(command)
            .timeout(Some(Duration::from_secs(30)))
            .check()
        {
            Ok(out) => return Some(out.stdout_trimmed()),
            Err(_) if wait => std::thread::sleep(Duration::from_millis(250)),
            Err(_) => return None,
        }
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
    use serde_json::json;

    fn v1(value: serde_json::Value) -> Object {
        match value {
            Value::Object(map) => map,
            _ => Object::new(),
        }
    }

    #[test]
    fn datasource_is_normalised() {
        let status = v1(json!({"datasource": "DataSourceNoCloud [seed=/dev/sr0]"}));
        assert_eq!(get_datasource(&status), "nocloud");
    }

    #[test]
    fn latest_event_is_the_max_across_stages() {
        let status = v1(json!({
            "init": {"start": 10.0, "finished": 20.5},
            "modules-final": {"start": 30.25, "finished": null},
            "stage": null,
        }));
        assert!((get_latest_event(&status) - 30.25).abs() < f64::EPSILON);
    }

    #[test]
    fn errors_are_aggregated_in_key_order() {
        let status = v1(json!({
            "modules-final": {"errors": ["late"], "recoverable_errors": {"WARNING": ["w2"]}},
            "init": {"errors": ["early"], "recoverable_errors": {"WARNING": ["w1"]}},
        }));
        let (errors, recoverable) = get_errors(&status);
        assert_eq!(errors, vec!["early".to_owned(), "late".to_owned()]);
        assert_eq!(recoverable["WARNING"], json!(["w1", "w2"]));
    }

    #[test]
    fn translation_matches_upstream_matrix() {
        let base = StatusDetails {
            running_status: RunningStatus::Done,
            condition_status: ConditionStatus::Peachy,
            boot_status_code: EnabledStatus::EnabledByGenerator,
            description: String::new(),
            errors: Vec::new(),
            recoverable_errors: Object::new(),
            last_update: String::new(),
            datasource: String::new(),
            v1: Object::new(),
        };

        assert_eq!(base.translate(), ("done".into(), "done".into()));
        assert_eq!(base.exit_code(), 0);

        let degraded = StatusDetails {
            condition_status: ConditionStatus::Degraded,
            ..base.clone()
        };
        assert_eq!(
            degraded.translate(),
            ("done".into(), "degraded done".into())
        );
        assert_eq!(degraded.exit_code(), 2);

        let failed = StatusDetails {
            condition_status: ConditionStatus::Error,
            running_status: RunningStatus::Running,
            ..base
        };
        assert_eq!(
            failed.translate(),
            ("error".into(), "error - running".into())
        );
        assert_eq!(failed.exit_code(), 1);
    }
}
