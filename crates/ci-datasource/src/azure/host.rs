//! The [`Platform`] the Azure crawl runs against on a real machine.
//!
//! Every method here is one of the host operations [`super::crawl`] takes as an
//! argument. Keeping them in their own file is what lets the crawl's sequencing
//! stay readable and testable; it also means the parts that are not ported yet
//! are visible in one place rather than scattered through the crawl.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ci_config::{Object, Value};

use super::crawl::{Platform, Source, SourceError};
use super::ds::{self, OvfCrawl};

const SOURCE: &str = "azure.py";

/// `get_metadata_from_imds`' `retry_deadline`.
const IMDS_DEADLINE: Duration = Duration::from_secs(300);
/// The connection-error cap upstream applies while no route to IMDS has been
/// configured, which is always the case here.
const IMDS_MAX_CONNECTION_ERRORS: u32 = 11;

/// `retry_sleep`, between two attempts at a lease.
const DHCP_RETRY_SLEEP: Duration = Duration::from_secs(1);

/// The real machine.
#[derive(Debug)]
pub struct Host {
    seed_dir: PathBuf,
    data_dir: PathBuf,
    /// `paths.get_cpath("data")`, where the previous boot's id is kept.
    cloud_data_dir: PathBuf,
    reported_ready_marker: PathBuf,
    /// `self._wireserver_endpoint`, which a DHCP lease may correct.
    endpoint: String,
    system_uuid: Option<String>,
    /// `self._ephemeral_dhcp_ctx`. Held for as long as the crawl needs it and
    /// torn down when the `Host` goes away, which is upstream's
    /// `_teardown_ephemeral_networking`.
    ephemeral: Option<ci_net::ephemeral::Ephemeral>,
    /// `/sys/class/net`, kept as a field so a test can point it elsewhere.
    sys: ci_net::sysfs::Sys,
}

impl Host {
    #[must_use]
    pub fn new(paths: &ci_core::Paths, ds_cfg: &Object) -> Self {
        let data_dir = ds_cfg
            .get("data_dir")
            .and_then(Value::as_str)
            .unwrap_or(ds::AGENT_SEED_DIR);
        let cloud_data_dir = paths.cpath(ci_core::paths::Lookup::Data);
        Self {
            seed_dir: paths.seed_dir().join("azure"),
            data_dir: PathBuf::from(data_dir),
            reported_ready_marker: cloud_data_dir.join("reported_ready"),
            cloud_data_dir,
            endpoint: super::wire::DEFAULT_ENDPOINT.to_owned(),
            system_uuid: None,
            ephemeral: None,
            sys: ci_net::sysfs::Sys::real(),
        }
    }

    /// `self.seed_dir`, which `ds_detect` also looks in.
    #[must_use]
    pub fn seed_dir(&self) -> &Path {
        &self.seed_dir
    }

    /// The interfaces `generate_network_config` matches IMDS against.
    #[must_use]
    pub fn interfaces(&self) -> Vec<super::netcfg::Interface> {
        self.sys
            .interfaces(ci_net::sysfs::Filters::All)
            .into_iter()
            .map(|interface| super::netcfg::Interface {
                mac: interface.mac,
                driver: interface.driver,
            })
            .collect()
    }
}

/// `load_azure_ds_dir`, keeping the raw document as well as the crawl.
///
/// Upstream's `files["ovf-env.xml"]` holds the bytes it read, so the reader has
/// to hand both back.
fn load_dir(
    dir: &Path,
    log: &mut ci_log::Logger,
) -> Result<(OvfCrawl, Vec<u8>), SourceError> {
    let raw =
        std::fs::read(dir.join("ovf-env.xml")).map_err(|_| SourceError::NonAzure)?;
    let crawl = ds::read_azure_ovf(&String::from_utf8_lossy(&raw), log)
        .map_err(|_| SourceError::NonAzure)?;
    Ok((crawl, raw))
}

impl Platform for Host {
    fn system_uuid(&mut self) -> Result<String, String> {
        if let Some(uuid) = &self.system_uuid {
            return Ok(uuid.clone());
        }
        let mut log = ci_log::Logger::silent();
        let uuid = super::identity::query_system_uuid(&mut log)
            .ok_or_else(|| "failed to read system uuid".to_owned())?;
        self.system_uuid = Some(uuid.clone());
        Ok(uuid)
    }

    fn is_gen1(&mut self) -> bool {
        super::identity::is_vm_gen1()
    }

    fn candidates(&mut self) -> Vec<Source> {
        // `list_possible_azure_ds`: the seed directory, the fixed provisioning
        // ISO node, then whatever blkid reports as iso9660 or udf, then the
        // cached `data_dir`. Upstream yields `cache_dir` only when it is set;
        // here it always is, because `ds_config` supplies the default.
        let mut sources = vec![
            Source::Dir(self.seed_dir.clone()),
            Source::Device(PathBuf::from(ds::DEFAULT_PROVISIONING_ISO_DEV)),
        ];
        for criteria in ["TYPE=iso9660", "TYPE=udf"] {
            sources.extend(
                ci_sys::mount::find_devs_with(Some(criteria))
                    .into_iter()
                    .map(Source::Device),
            );
        }
        sources.push(Source::Dir(self.data_dir.clone()));
        sources
    }

    fn load(
        &mut self,
        source: &Source,
        log: &mut ci_log::Logger,
    ) -> Result<(OvfCrawl, Vec<u8>), SourceError> {
        match source {
            Source::Dir(dir) => load_dir(dir, log),
            Source::Device(device) => {
                let mut warnings = Vec::new();
                let mounted = ci_sys::mount::mount_cb(
                    device,
                    &["iso9660", "udf"],
                    &mut warnings,
                    |point| load_dir(point, log),
                );
                for warning in warnings {
                    log.debug(SOURCE, &warning);
                }
                mounted.map_err(|error| {
                    log.debug(SOURCE, &error.to_string());
                    SourceError::MountFailed
                })?
            }
        }
    }

    fn setup_ephemeral_networking(
        &mut self,
        timeout_minutes: u64,
        log: &mut ci_log::Logger,
    ) -> bool {
        if self.ephemeral.is_some() {
            log.warning(SOURCE, "Bringing up networking when already configured.");
            return true;
        }

        // `iface` is always None here: the crawl never asks for a particular
        // interface, so the primary NIC is re-chosen on every attempt. That
        // matters on Azure, where the NIC the platform will answer DHCP on may
        // not have appeared yet when the first attempt runs.
        let deadline = Instant::now() + Duration::from_secs(timeout_minutes * 60);
        let lease = loop {
            let primary = self.sys.fallback_nic();
            log.debug(
                SOURCE,
                &format!(
                    "Bringing up ephemeral networking with iface={}",
                    primary.as_deref().unwrap_or("<none>")
                ),
            );
            match ci_net::ephemeral::obtain_lease(primary.as_deref(), &self.sys, log) {
                Ok(lease) => break Some(lease),
                Err(ci_net::ephemeral::Error::MissingClient) => {
                    // Upstream re-raises this one rather than retrying: no
                    // amount of waiting installs a DHCP client.
                    log.error(SOURCE, "dhcp client executable not found");
                    break None;
                }
                Err(error) => {
                    log.error(SOURCE, &format!("Failed to obtain DHCP lease: {error}"));
                }
            }
            if Instant::now() + DHCP_RETRY_SLEEP >= deadline {
                break None;
            }
            std::thread::sleep(DHCP_RETRY_SLEEP);
        };

        let Some(lease) = lease else {
            return false;
        };

        // Option 245 is how the platform tells a VM which wireserver to talk
        // to; the compiled-in default is only a fallback.
        if let Some(endpoint) = lease.wireserver_endpoint() {
            endpoint.clone_into(&mut self.endpoint);
        }
        log.debug(
            SOURCE,
            &format!(
                "Obtained DHCP lease on interface {:?} (router={:?} routes={:?})",
                lease.interface, lease.router, lease.static_routes
            ),
        );
        self.ephemeral = Some(lease);
        true
    }

    fn fetch_imds(
        &mut self,
        _report_failure: bool,
        log: &mut ci_log::Logger,
    ) -> Object {
        let deadline = Instant::now() + IMDS_DEADLINE;
        match super::imds::fetch_metadata_with_api_fallback(
            super::imds::IMDS_URL,
            Some(deadline),
            Some(IMDS_MAX_CONNECTION_ERRORS),
            log,
        ) {
            Ok(Value::Object(imds)) => imds,
            Ok(_) => Object::new(),
            Err(error) => {
                log.warning(SOURCE, &format!("Ignoring IMDS metadata due to: {error}"));
                Object::new()
            }
        }
    }

    fn report_ready(
        &mut self,
        pubkey_info: Option<&[Value]>,
        log: &mut ci_log::Logger,
    ) -> Result<Vec<String>, String> {
        super::wire::report_ready(&self.endpoint, pubkey_info, log)
            .map_err(|error| error.to_string())
    }

    fn previous_instance_id(&mut self) -> Option<String> {
        std::fs::read_to_string(self.cloud_data_dir.join("instance-id")).ok()
    }

    fn random_seed(&mut self) -> Option<String> {
        let path = Path::new(ds::PLATFORM_ENTROPY_SOURCE);
        path.exists().then(|| ds::random_seed(path))
    }

    fn reported_ready_marker(&mut self) -> bool {
        self.reported_ready_marker.is_file()
    }

    fn cleanup_markers(&mut self, log: &mut ci_log::Logger) {
        if let Err(error) = std::fs::remove_file(&self.reported_ready_marker) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log.warning(
                    SOURCE,
                    &format!(
                        "Failed to remove {}: {error}",
                        self.reported_ready_marker.display()
                    ),
                );
            }
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

    fn host(root: &Path) -> Host {
        let paths = ci_core::Paths {
            cloud_dir: root.join("var"),
            ..ci_core::Paths::default()
        };
        let mut ds_cfg = Object::new();
        ds_cfg.insert(
            "data_dir".to_owned(),
            Value::from(root.join("waagent").to_string_lossy().to_string()),
        );
        Host::new(&paths, &ds_cfg)
    }

    #[test]
    fn the_candidates_start_at_the_seed_dir_and_end_at_the_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let candidates = host(dir.path()).candidates();
        // Whatever blkid reports on the machine running the tests sits in the
        // middle, so only the fixed ends are asserted.
        assert!(candidates.len() >= 3);
        assert_eq!(
            candidates[0],
            Source::Dir(dir.path().join("var/seed/azure"))
        );
        assert_eq!(candidates[1], Source::Device(PathBuf::from("/dev/sr0")));
        assert_eq!(
            candidates.last(),
            Some(&Source::Dir(dir.path().join("waagent")))
        );
        assert!(candidates[2..candidates.len() - 1]
            .iter()
            .all(|source| matches!(source, Source::Device(_))));
    }

    #[test]
    fn a_directory_without_an_ovf_is_not_azure_and_a_missing_device_will_not_mount() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = host(dir.path());
        let mut log = ci_log::Logger::silent();
        assert!(matches!(
            host.load(&Source::Dir(dir.path().to_owned()), &mut log),
            Err(SourceError::NonAzure)
        ));
        assert!(matches!(
            host.load(&Source::Device(dir.path().join("no-such-device")), &mut log),
            Err(SourceError::MountFailed)
        ));
    }

    #[test]
    fn the_marker_is_removed_and_removing_an_absent_one_is_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = host(dir.path());
        let mut log = ci_log::Logger::silent();
        assert!(!host.reported_ready_marker());
        host.cleanup_markers(&mut log);

        std::fs::create_dir_all(dir.path().join("var/data")).unwrap();
        std::fs::write(dir.path().join("var/data/reported_ready"), "").unwrap();
        assert!(host.reported_ready_marker());
        host.cleanup_markers(&mut log);
        assert!(!host.reported_ready_marker());
    }

    #[test]
    fn the_previous_instance_id_comes_from_the_cloud_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = host(dir.path());
        assert_eq!(host.previous_instance_id(), None);
        std::fs::create_dir_all(dir.path().join("var/data")).unwrap();
        std::fs::write(dir.path().join("var/data/instance-id"), "old\n").unwrap();
        assert_eq!(host.previous_instance_id().as_deref(), Some("old\n"));
    }
}
