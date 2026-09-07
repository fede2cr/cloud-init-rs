//! Port of `sources/azure/kvp.py`: the provisioning report, and the Hyper-V
//! key-value pool it goes out through.
//!
//! The pool is the only channel that survives a guest whose networking never
//! came up, so a failure written here is what turns a silent bad boot into
//! something the platform can show. The transport itself lives in
//! [`ci_report::kvp`].

use ci_log::Logger;
use ci_report::Reporter;

use super::errors::{self, ReportableError};

/// `PROVISIONING_REPORT`, the key the host agent reads the report back out of.
const PROVISIONING_REPORT: &str = "PROVISIONING_REPORT";

/// The `PROVISIONING_REPORT` payload for a successful provision.
#[must_use]
pub fn success_report(vm_id: Option<&str>, timestamp: &str) -> String {
    errors::encode_report(&[
        "result=success".to_owned(),
        format!("agent={}", errors::agent()),
        format!("timestamp={timestamp}"),
        format!("vm_id={}", vm_id.unwrap_or("None")),
    ])
}

/// `get_kvp_handler`: the registered telemetry handler, with the vm id filled
/// in the first time something asks for it.
fn with_vm_id<'a>(
    reporter: &'a mut Reporter,
    log: &mut Logger,
) -> Option<&'a mut ci_report::kvp::HyperVKvpHandler> {
    let vm_id = if reporter.kvp_handler()?.vm_id() == ci_report::kvp::ZERO_GUID {
        super::identity::query_vm_id(log)
    } else {
        None
    };
    let handler = reporter.kvp_handler()?;
    if let Some(vm_id) = vm_id {
        handler.set_vm_id(&vm_id);
    }
    Some(handler)
}

/// `report_via_kvp`. `false` means there was no KVP handler to write through,
/// which is the ordinary case anywhere but Azure.
pub fn report_via_kvp(reporter: &mut Reporter, report: &str, log: &mut Logger) -> bool {
    let Some(handler) = with_vm_id(reporter, log) else {
        log.debug("kvp.py", "KVP handler not enabled, skipping host report.");
        return false;
    };
    handler.write_key(PROVISIONING_REPORT, report, log);
    true
}

/// `report_success_to_host`.
pub fn report_success_to_host(
    reporter: &mut Reporter,
    vm_id: Option<&str>,
    log: &mut Logger,
) -> bool {
    let timestamp =
        ci_core::time::format_python_isoformat_utc(ci_core::time::now_epoch());
    report_via_kvp(reporter, &success_report(vm_id, &timestamp), log)
}

/// The host half of `DataSourceAzure._report_failure`: whatever else that
/// method manages to do over the network, it always writes the encoded report
/// to the pool first.
pub fn report_failure_to_host(
    reporter: &mut Reporter,
    error: &ReportableError,
    vm_id: Option<&str>,
    log: &mut Logger,
) -> bool {
    report_via_kvp(reporter, &error.as_encoded_report(vm_id), log)
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
    use ci_report::kvp::{HyperVKvpHandler, MAX_KEY_SIZE, RECORD_SIZE};
    use std::path::Path;

    fn reporter_writing_to(path: &Path, log: &mut Logger) -> Reporter {
        let mut reporter = Reporter::silent();
        reporter.register(
            "telemetry",
            Box::new(HyperVKvpHandler::new(path, None, log)),
        );
        reporter
    }

    fn record_at(pool: &[u8], index: usize) -> (String, String) {
        let record = &pool[index * RECORD_SIZE..(index + 1) * RECORD_SIZE];
        let take = |bytes: &[u8]| {
            String::from_utf8_lossy(bytes)
                .trim_end_matches('\0')
                .to_owned()
        };
        (take(&record[..MAX_KEY_SIZE]), take(&record[MAX_KEY_SIZE..]))
    }

    #[test]
    fn the_success_report_orders_its_fields_the_way_the_host_reads_them() {
        let report = success_report(Some("abc"), "2026-09-02T00:00:00+00:00");
        assert!(report.starts_with("result=success|agent=Cloud-Init-rs/"));
        assert!(report.ends_with("|timestamp=2026-09-02T00:00:00+00:00|vm_id=abc"));
    }

    #[test]
    fn a_missing_vm_id_is_the_word_none() {
        assert!(success_report(None, "t").ends_with("|vm_id=None"));
    }

    #[test]
    fn a_failure_reaches_the_pool_under_the_key_the_host_enumerates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pool");
        let mut log = Logger::silent();
        let mut reporter = reporter_writing_to(&path, &mut log);

        let error = ReportableError::new("failed to identify");
        assert!(report_failure_to_host(
            &mut reporter,
            &error,
            Some("abc"),
            &mut log
        ));

        let pool = std::fs::read(&path).unwrap();
        assert_eq!(pool.len(), RECORD_SIZE);
        let (key, value) = record_at(&pool, 0);
        assert_eq!(key, "PROVISIONING_REPORT");
        assert_eq!(value, error.as_encoded_report(Some("abc")));
        assert!(value.starts_with("result=error|reason=failed to identify|"));
    }

    #[test]
    fn a_success_report_reaches_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pool");
        let mut log = Logger::silent();
        let mut reporter = reporter_writing_to(&path, &mut log);

        assert!(report_success_to_host(&mut reporter, Some("abc"), &mut log));

        let (key, value) = record_at(&std::fs::read(&path).unwrap(), 0);
        assert_eq!(key, "PROVISIONING_REPORT");
        assert!(value.starts_with("result=success|"));
        assert!(value.ends_with("|vm_id=abc"));
    }

    #[test]
    fn with_no_telemetry_handler_registered_nothing_is_written_and_it_says_so() {
        let mut log = Logger::silent();
        let mut reporter = Reporter::default();
        assert!(!report_via_kvp(&mut reporter, "result=success", &mut log));
    }

    #[test]
    fn a_handler_registered_under_another_name_is_not_the_telemetry_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pool");
        let mut log = Logger::silent();
        let mut reporter = Reporter::silent();
        reporter.register(
            "kvp",
            Box::new(HyperVKvpHandler::new(&path, None, &mut log)),
        );
        assert!(!report_via_kvp(&mut reporter, "result=success", &mut log));
        assert!(!path.exists());
    }
}
