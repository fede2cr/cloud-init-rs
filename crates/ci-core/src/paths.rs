//! Port of `cloudinit/helpers.py::Paths`.
//!
//! Every path cloud-init reads or writes is derived here so that the filesystem
//! contract (PLAN.md §6.2) has a single source of truth in the code as well as in
//! packaging.

use std::path::{Path, PathBuf};

use ci_config::Object;

/// An entry in upstream's `Paths.lookups`.
///
/// Upstream indexes a dict, so an unknown name is a `KeyError` at runtime; an
/// enum moves that to compile time. `key()` is the upstream dict key, which
/// [`LOOKUPS`] pairs with the filename so the table can be diffed against
/// Python.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lookup {
    BootHooks,
    CloudConfig,
    CombinedCloudConfig,
    Data,
    Handlers,
    HotplugEnabled,
    InstanceData,
    InstanceDataSensitive,
    InstanceId,
    ManualCleanMarker,
    NetworkConfig,
    ObjPkl,
    Scripts,
    Sem,
    SkipNetwork,
    UserData,
    UserDataRaw,
    Vendor2CloudConfig,
    VendorCloudConfig,
    VendorData,
    VendorData2,
    VendorData2Raw,
    VendorDataRaw,
    VendorScripts,
    Warnings,
}

impl Lookup {
    /// Every entry in upstream's `Paths.lookups`.
    pub const ALL: &'static [Lookup] = &[
        Lookup::BootHooks,
        Lookup::CloudConfig,
        Lookup::CombinedCloudConfig,
        Lookup::Data,
        Lookup::Handlers,
        Lookup::HotplugEnabled,
        Lookup::InstanceData,
        Lookup::InstanceDataSensitive,
        Lookup::InstanceId,
        Lookup::ManualCleanMarker,
        Lookup::NetworkConfig,
        Lookup::ObjPkl,
        Lookup::Scripts,
        Lookup::Sem,
        Lookup::SkipNetwork,
        Lookup::UserData,
        Lookup::UserDataRaw,
        Lookup::Vendor2CloudConfig,
        Lookup::VendorCloudConfig,
        Lookup::VendorData,
        Lookup::VendorData2,
        Lookup::VendorData2Raw,
        Lookup::VendorDataRaw,
        Lookup::VendorScripts,
        Lookup::Warnings,
    ];

    /// The upstream `Paths.lookups` key.
    pub fn key(self) -> &'static str {
        match self {
            Self::BootHooks => "boothooks",
            Self::CloudConfig => "cloud_config",
            Self::CombinedCloudConfig => "combined_cloud_config",
            Self::Data => "data",
            Self::Handlers => "handlers",
            Self::HotplugEnabled => "hotplug.enabled",
            Self::InstanceData => "instance_data",
            Self::InstanceDataSensitive => "instance_data_sensitive",
            Self::InstanceId => "instance_id",
            Self::ManualCleanMarker => "manual_clean_marker",
            Self::NetworkConfig => "network_config",
            Self::ObjPkl => "obj_pkl",
            Self::Scripts => "scripts",
            Self::Sem => "sem",
            Self::SkipNetwork => ".skip-network",
            Self::UserData => "userdata",
            Self::UserDataRaw => "userdata_raw",
            Self::Vendor2CloudConfig => "vendor2_cloud_config",
            Self::VendorCloudConfig => "vendor_cloud_config",
            Self::VendorData => "vendordata",
            Self::VendorData2 => "vendordata2",
            Self::VendorData2Raw => "vendordata2_raw",
            Self::VendorDataRaw => "vendordata_raw",
            Self::VendorScripts => "vendor_scripts",
            Self::Warnings => "warnings",
        }
    }

    /// The path fragment appended to the base directory.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::BootHooks => "boothooks",
            Self::CloudConfig => "cloud-config.txt",
            Self::CombinedCloudConfig => "combined-cloud-config.json",
            Self::Data => "data",
            Self::Handlers => "handlers",
            Self::HotplugEnabled => "hotplug.enabled",
            Self::InstanceData => "instance-data.json",
            Self::InstanceDataSensitive => "instance-data-sensitive.json",
            Self::InstanceId => ".instance-id",
            Self::ManualCleanMarker => "manual-clean",
            Self::NetworkConfig => "network-config.json",
            Self::ObjPkl => "obj.pkl",
            Self::Scripts => "scripts",
            Self::Sem => "sem",
            Self::SkipNetwork => ".skip-network",
            Self::UserData => "user-data.txt.i",
            Self::UserDataRaw => "user-data.txt",
            Self::Vendor2CloudConfig => "vendor2-cloud-config.txt",
            Self::VendorCloudConfig => "vendor-cloud-config.txt",
            Self::VendorData => "vendor-data.txt.i",
            Self::VendorData2 => "vendor-data2.txt.i",
            Self::VendorData2Raw => "vendor-data2.txt",
            Self::VendorDataRaw => "vendor-data.txt",
            Self::VendorScripts => "scripts/vendor",
            Self::Warnings => "warnings",
        }
    }
}

/// Resolved cloud-init directory layout.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `/var/lib/cloud`
    pub cloud_dir: PathBuf,
    /// `/run/cloud-init`
    pub run_dir: PathBuf,
    /// `/etc/cloud/templates`
    pub templates_dir: PathBuf,
    /// `/usr/share/doc/cloud-init`
    pub docs_dir: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            cloud_dir: PathBuf::from("/var/lib/cloud"),
            run_dir: PathBuf::from(ci_config::builtin::DEFAULT_RUN_DIR),
            templates_dir: PathBuf::from("/etc/cloud/templates/"),
            docs_dir: PathBuf::from("/usr/share/doc/cloud-init/"),
        }
    }
}

impl Paths {
    /// Build from a merged base config, reading `system_info.paths`.
    pub fn from_config(cfg: &Object) -> Self {
        let mut paths = Self::default();
        let Some(configured) = cfg
            .get("system_info")
            .and_then(|si| si.get("paths"))
            .and_then(|p| p.as_object())
        else {
            return paths;
        };
        let get = |key: &str| {
            configured
                .get(key)
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
        };
        if let Some(dir) = get("cloud_dir") {
            paths.cloud_dir = dir;
        }
        if let Some(dir) = get("run_dir") {
            paths.run_dir = dir;
        }
        if let Some(dir) = get("templates_dir") {
            paths.templates_dir = dir;
        }
        if let Some(dir) = get("docs_dir") {
            paths.docs_dir = dir;
        }
        paths
    }

    /// Read the base config from disk and resolve paths from it.
    pub fn read() -> Self {
        let cfg =
            ci_config::read::fetch_base_config(None, ci_config::Limits::default())
                .unwrap_or_default();
        Self::from_config(&cfg)
    }

    /// `/var/lib/cloud/instance` — symlink to the current instance directory.
    pub fn instance_link(&self) -> PathBuf {
        self.cloud_dir.join("instance")
    }

    /// `/var/lib/cloud/instances`
    pub fn instances_dir(&self) -> PathBuf {
        self.cloud_dir.join("instances")
    }

    /// `/var/lib/cloud/seed`
    pub fn seed_dir(&self) -> PathBuf {
        self.cloud_dir.join("seed")
    }

    /// `/var/lib/cloud/instance/boot-finished`
    pub fn boot_finished(&self) -> PathBuf {
        self.instance_link().join("boot-finished")
    }

    /// `get_cpath` — a path under `cloud_dir`.
    pub fn cpath(&self, name: Lookup) -> PathBuf {
        self.cloud_dir.join(name.file_name())
    }

    /// `get_runpath` — a path under `run_dir`.
    pub fn run_path(&self, name: Lookup) -> PathBuf {
        self.run_dir.join(name.file_name())
    }

    /// `get_ipath_cur` — a path under the `instance` symlink.
    pub fn instance_path(&self, name: Lookup) -> PathBuf {
        self.instance_link().join(name.file_name())
    }

    /// `get_ipath` — a path under `instances/<iid>`, with `/` escaped as `_`.
    ///
    /// Upstream reads the instance id from the active datasource and returns
    /// `None` when there is none; the caller supplies it here until datasources
    /// land in Phase 3.
    pub fn instance_path_for(&self, iid: &str, name: Lookup) -> PathBuf {
        self.instances_dir()
            .join(iid.replace('/', "_"))
            .join(name.file_name())
    }

    /// `/etc/cloud/templates/<name>.tmpl`
    pub fn template_tpl(&self, name: &str) -> PathBuf {
        self.templates_dir.join(format!("{name}.tmpl"))
    }

    /// Marker recording which implementation ran this boot (PLAN.md §6.7).
    pub fn impl_marker(&self) -> PathBuf {
        self.run_dir.join(".impl")
    }

    /// `/run/cloud-init/status.json` — the symlink `cloud-init status` reads.
    ///
    /// Not a `lookups` entry: `main.py:885` joins the name directly, writing the
    /// canonical copy under [`Self::data_dir`] and linking it here.
    pub fn status_file(&self) -> PathBuf {
        self.run_dir.join("status.json")
    }

    /// `/run/cloud-init/result.json` — see [`Self::status_file`].
    pub fn result_file(&self) -> PathBuf {
        self.run_dir.join("result.json")
    }

    /// `/var/lib/cloud/data` — where `status.json` and `result.json` really live.
    pub fn data_dir(&self) -> PathBuf {
        self.cpath(Lookup::Data)
    }

    pub fn instance_data_file(&self) -> PathBuf {
        self.run_path(Lookup::InstanceData)
    }

    pub fn instance_data_sensitive_file(&self) -> PathBuf {
        self.run_path(Lookup::InstanceDataSensitive)
    }

    /// Whether `path` is inside `cloud_dir` or `run_dir`.
    ///
    /// Used to refuse operations (notably `clean`) on paths outside the state
    /// directories, so a hostile config cannot turn a maintenance command into an
    /// arbitrary-file-removal primitive.
    pub fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.cloud_dir) || path.starts_with(&self.run_dir)
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

    #[test]
    fn defaults_match_upstream_layout() {
        let paths = Paths::default();
        assert_eq!(
            paths.status_file(),
            Path::new("/run/cloud-init/status.json")
        );
        assert_eq!(
            paths.instance_data_sensitive_file(),
            Path::new("/run/cloud-init/instance-data-sensitive.json")
        );
        assert_eq!(
            paths.instance_path(Lookup::Sem),
            Path::new("/var/lib/cloud/instance/sem")
        );
        assert_eq!(paths.data_dir(), Path::new("/var/lib/cloud/data"));
        assert_eq!(
            paths.template_tpl("hosts.debian"),
            Path::new("/etc/cloud/templates/hosts.debian.tmpl")
        );
    }

    /// The table captured from `Paths({}).lookups` on the targeted release.
    /// Every divergence here is a path cloud-init would read or write in the
    /// wrong place.
    #[test]
    fn the_lookup_table_matches_upstream() {
        let upstream = [
            (".skip-network", ".skip-network"),
            ("boothooks", "boothooks"),
            ("cloud_config", "cloud-config.txt"),
            ("combined_cloud_config", "combined-cloud-config.json"),
            ("data", "data"),
            ("handlers", "handlers"),
            ("hotplug.enabled", "hotplug.enabled"),
            ("instance_data", "instance-data.json"),
            ("instance_data_sensitive", "instance-data-sensitive.json"),
            ("instance_id", ".instance-id"),
            ("manual_clean_marker", "manual-clean"),
            ("network_config", "network-config.json"),
            ("obj_pkl", "obj.pkl"),
            ("scripts", "scripts"),
            ("sem", "sem"),
            ("userdata", "user-data.txt.i"),
            ("userdata_raw", "user-data.txt"),
            ("vendor2_cloud_config", "vendor2-cloud-config.txt"),
            ("vendor_cloud_config", "vendor-cloud-config.txt"),
            ("vendor_scripts", "scripts/vendor"),
            ("vendordata", "vendor-data.txt.i"),
            ("vendordata2", "vendor-data2.txt.i"),
            ("vendordata2_raw", "vendor-data2.txt"),
            ("vendordata_raw", "vendor-data.txt"),
            ("warnings", "warnings"),
        ];
        let mut ours: Vec<(&str, &str)> = Lookup::ALL
            .iter()
            .map(|l| (l.key(), l.file_name()))
            .collect();
        ours.sort_unstable();
        assert_eq!(ours, upstream);
    }

    #[test]
    fn an_instance_id_with_a_separator_is_escaped() {
        let paths = Paths::default();
        assert_eq!(
            paths.instance_path_for("iid/with/slash", Lookup::ObjPkl),
            Path::new("/var/lib/cloud/instances/iid_with_slash/obj.pkl")
        );
    }

    #[test]
    fn honours_system_info_paths() {
        let cfg = ci_config::yaml::load_mapping(
            "system_info:\n  paths:\n    cloud_dir: /srv/cloud\n",
            ci_config::Limits::default(),
        )
        .unwrap();
        let paths = Paths::from_config(&cfg);
        assert_eq!(paths.cloud_dir, Path::new("/srv/cloud"));
        assert_eq!(paths.seed_dir(), Path::new("/srv/cloud/seed"));
    }

    #[test]
    fn rejects_paths_outside_the_state_dirs() {
        let paths = Paths::default();
        assert!(paths.contains(Path::new("/var/lib/cloud/instances/iid-1")));
        assert!(!paths.contains(Path::new("/etc/shadow")));
    }
}
