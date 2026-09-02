//! `cloud-init analyze boot` — kernel and cloud-init activation timestamps.
//!
//! Port of `show.dist_check_timestamp()` and `analyze.analyze_boot()`.

use serde_json::Value;

pub const SUCCESS_CODE: &str = "successful";
pub const FAIL_CODE: &str = "failure";
pub const CONTAINER_CODE: &str = "container";

const FAILURE_MSG: &str = "Your Linux distro or container does not support this \
functionality.\nYou must be running a Kernel Telemetry supported distro.\nPlease \
check https://docs.cloud-init.io/en/latest/topics/analyze.html for more information \
on supported distros.\n";

const NO_INIT_LOCAL: &str = "Could not find init-local log-line in cloud-init.log";

/// Timestamps upstream reports when it cannot read them: `TIMESTAMP_UNKNOWN`.
const UNKNOWN: (&str, f64, f64, f64) = (FAIL_CODE, -1.0, -1.0, -1.0);

/// Render the boot record, returning the text to write and the status code that
/// upstream hands to `sys.exit()`.
pub fn render(events: &[Value]) -> (String, &'static str) {
    let (mut status_code, kernel_start, kernel_end, systemd_activation) =
        dist_check_timestamp();

    let searched = last_init_local_search(events);
    if searched.is_none() {
        status_code = FAIL_CODE;
    }
    let ci_start = searched.map_or_else(
        || NO_INIT_LOCAL.to_owned(),
        ci_core::time::format_python_datetime_utc,
    );

    let kernel_started = ci_core::time::format_python_datetime_utc(kernel_start);
    let kernel_finished = ci_core::time::format_python_datetime_utc(kernel_end);
    let systemd_started = ci_core::time::format_python_datetime_utc(systemd_activation);
    let kernel_runtime = python_float(kernel_end - kernel_start);
    let between_process_runtime = python_float(systemd_activation - kernel_end);

    let text = match status_code {
        CONTAINER_CODE => format!(
            concat!(
                "-- Most Recent Container Boot Record --\n",
                "    Container started at: {k_s_t}\n",
                "    Cloud-init activated by systemd at: {ci_sysd_t}\n",
                "    Cloud-init start: {ci_start}\n",
            ),
            k_s_t = kernel_started,
            ci_sysd_t = systemd_started,
            ci_start = ci_start,
        ),
        SUCCESS_CODE => format!(
            concat!(
                "-- Most Recent Boot Record --\n",
                "    Kernel Started at: {k_s_t}\n",
                "    Kernel ended boot at: {k_e_t}\n",
                "    Kernel time to boot (seconds): {k_r}\n",
                "    Cloud-init activated by systemd at: {ci_sysd_t}\n",
                "    Time between Kernel end boot and Cloud-init activation ",
                "(seconds): {bt_r}\n",
                "    Cloud-init start: {ci_start}\n",
            ),
            k_s_t = kernel_started,
            k_e_t = kernel_finished,
            k_r = kernel_runtime,
            ci_sysd_t = systemd_started,
            bt_r = between_process_runtime,
            ci_start = ci_start,
        ),
        _ => FAILURE_MSG.to_owned(),
    };
    (text, status_code)
}

/// The last `init-local` event whose description mentions the datasource search.
fn last_init_local_search(events: &[Value]) -> Option<f64> {
    events
        .iter()
        .rev()
        .find(|event| {
            event.get("name").and_then(Value::as_str) == Some("init-local")
                && event
                    .get("description")
                    .is_some_and(|d| description_text(d).contains("starting search"))
        })
        .and_then(|event| event.get("timestamp"))
        .and_then(|ts| match ts {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        })
}

fn description_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

fn dist_check_timestamp() -> (&'static str, f64, f64, f64) {
    if uses_systemd() {
        return gather_timestamps_using_systemd();
    }
    // Upstream also has a dmesg path for FreeBSD and Gentoo; those distros arrive
    // with their `Distro` implementations in Phase 8.
    UNKNOWN
}

fn uses_systemd() -> bool {
    std::fs::symlink_metadata("/run/systemd/system").is_ok_and(|m| m.is_dir())
}

fn gather_timestamps_using_systemd() -> (&'static str, f64, f64, f64) {
    let container = is_container();
    let (start_property, offset_property) = if container {
        // lxc-style containers keep the host boot as the monotonic zero point.
        ("UserspaceTimestamp", "UserspaceTimestampMonotonic")
    } else {
        ("KernelTimestamp", "KernelTimestampMonotonic")
    };

    let gathered = (|| -> Result<(f64, f64, f64), String> {
        let kernel_start = systemctl_timestamp(start_property, None)?;
        let monotonic_offset = systemctl_timestamp(offset_property, None)?;
        let kernel_end = systemctl_timestamp("UserspaceTimestampMonotonic", None)?
            - monotonic_offset;
        let cloudinit_sysd = systemctl_timestamp(
            "InactiveExitTimestampMonotonic",
            Some("cloud-init-local"),
        )? - monotonic_offset;
        Ok((kernel_start, kernel_end, cloudinit_sysd))
    })();

    match gathered {
        Ok((kernel_start, kernel_end, cloudinit_sysd)) => {
            let status = if container {
                CONTAINER_CODE
            } else {
                SUCCESS_CODE
            };
            (
                status,
                kernel_start,
                kernel_start + kernel_end,
                kernel_start + cloudinit_sysd,
            )
        }
        Err(reason) => {
            // Upstream prints the raw exception on stdout before the failure text.
            println!("{reason}");
            UNKNOWN
        }
    }
}

/// `SystemctlReader` — read one `systemctl show` property as epoch seconds.
fn systemctl_timestamp(property: &str, unit: Option<&str>) -> Result<f64, String> {
    let mut argv = vec!["systemctl".to_owned(), "show".to_owned()];
    if let Some(unit) = unit {
        argv.push(unit.to_owned());
    }
    argv.push("-p".to_owned());
    argv.push(property.to_owned());
    argv.push("--timestamp=us+utc".to_owned());

    let output = ci_sys::subp::run(&argv).map_err(|e| e.to_string())?;
    if !output.success() || !output.stderr.is_empty() {
        return Err(format!(
            "Subprocess call to systemctl has failed, returning error code ({})",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = output.stdout_lossy();
    let value = stdout
        .split('=')
        .nth(1)
        .ok_or_else(|| format!("systemctl show -p {property} returned no value"))?
        .trim();

    // Monotonic properties are microsecond integers; wall-clock ones are strings.
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        let micros: f64 = value.parse().map_err(|_| format!("bad value {value}"))?;
        return Ok(micros / 1e6);
    }
    parse_systemd_stamp(value)
}

/// `%a %Y-%m-%d %H:%M:%S.%f %Z`, which `--timestamp=us+utc` always renders in UTC.
fn parse_systemd_stamp(value: &str) -> Result<f64, String> {
    let fields: Vec<&str> = value.split_whitespace().collect();
    let [_weekday, date, clock, _zone] = fields.as_slice() else {
        return Err(format!("time data '{value}' does not match format"));
    };
    let date: Vec<&str> = date.split('-').collect();
    let [year, month, day] = date.as_slice() else {
        return Err(format!("time data '{value}' does not match format"));
    };
    let (whole, frac) = clock.split_once('.').unwrap_or((clock, "0"));
    let clock: Vec<&str> = whole.split(':').collect();
    let [hour, minute, second] = clock.as_slice() else {
        return Err(format!("time data '{value}' does not match format"));
    };

    let number = |s: &str| -> Result<i64, String> {
        s.parse()
            .map_err(|_| format!("time data '{value}' does not match format"))
    };
    let secs = ci_core::time::epoch_from_civil(
        number(year)?,
        number(month)?,
        number(day)?,
        number(hour)?,
        number(minute)?,
        number(second)?,
    );
    let mut padded = frac.to_owned();
    padded.truncate(6);
    while padded.len() < 6 {
        padded.push('0');
    }
    #[allow(clippy::cast_precision_loss)]
    Ok(secs as f64 + number(&padded)? as f64 / 1e6)
}

/// `util.is_container()`.
fn is_container() -> bool {
    if cmd_exits_zero(&["systemd-detect-virt", "--quiet", "--container"])
        || cmd_exits_zero(&["lxc-is-container"])
    {
        return true;
    }
    if let Ok(environ) = std::fs::read("/proc/1/environ") {
        for entry in environ.split(|b| *b == 0) {
            let key = entry.split(|b| *b == b'=').next().unwrap_or_default();
            if key == b"container" || key == b"LIBVIRT_LXC_UUID" {
                return true;
            }
        }
    }
    if std::path::Path::new("/proc/vz").is_dir()
        && !std::path::Path::new("/proc/bc").is_dir()
    {
        return true;
    }
    // Linux-VServer: a non-zero context id means we are inside a guest.
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(value) = line.trim().strip_prefix("VxID:") {
                if value != "0" {
                    return true;
                }
            }
        }
    }
    false
}

fn cmd_exits_zero(argv: &[&str]) -> bool {
    ci_sys::subp::run(argv).is_ok_and(|out| out.success())
}

/// Python's `str(float)`: shortest round-trip form, always with a decimal point.
fn python_float(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    let rendered = format!("{value:?}");
    let Some(idx) = rendered.find(['e', 'E']) else {
        return rendered;
    };
    // Rust writes `1e20`; Python writes `1e+20`.
    match rendered.get(idx.saturating_add(1)..) {
        Some(exp) if !exp.starts_with('-') && !exp.starts_with('+') => {
            let head = rendered.get(..idx).unwrap_or_default();
            format!("{head}e+{exp}")
        }
        _ => rendered,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_the_systemd_utc_stamp() {
        assert_eq!(
            parse_systemd_stamp("Tue 2026-09-01 16:03:13.481295 UTC").unwrap(),
            1_788_278_593.481_295
        );
    }

    #[test]
    fn rejects_an_empty_systemd_property() {
        assert!(parse_systemd_stamp("").is_err());
    }

    #[test]
    fn takes_the_last_init_local_search_event() {
        let events = vec![
            json!({"name": "init-local", "description": "starting search for local \
                   datasources", "timestamp": 10.0}),
            json!({"name": "init-local", "description": "starting search for local \
                   datasources", "timestamp": 20.5}),
            json!({"name": "modules-final", "description": "x", "timestamp": 30.0}),
        ];
        assert_eq!(last_init_local_search(&events), Some(20.5));
    }

    #[test]
    fn reports_a_missing_init_local_event() {
        assert_eq!(last_init_local_search(&[]), None);
    }

    #[test]
    fn renders_floats_the_way_python_does() {
        assert_eq!(python_float(3.0), "3.0");
        assert_eq!(python_float(3.858), "3.858");
        assert_eq!(python_float(-1.0), "-1.0");
        assert_eq!(python_float(1e20), "1e+20");
    }
}
