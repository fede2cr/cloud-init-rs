//! The `/sys/class/net` half of `cloudinit.net`: which interfaces exist, and
//! which one to try DHCP on.
//!
//! This is the counterpart to the rest of `ci-net`, which only ever describes a
//! configuration. Everything here reads the live kernel, because there is no
//! other way to answer "what NICs does this machine have" — and an Azure VM
//! cannot get a lease, and therefore cannot reach IMDS, until that question is
//! answered.
//!
//! The root is configurable so the whole thing can be pointed at a fixture;
//! [`Sys::real`] is `/sys/class/net`.

use ci_config::{Object, Value};
use std::path::PathBuf;

/// `DEFAULT_PRIMARY_INTERFACE`.
pub const DEFAULT_PRIMARY_INTERFACE: &str = "eth0";

/// A mac of 16 `00` octets — longer than any real one, so the prefix compare
/// upstream does covers every address length.
const ZERO_MAC: &str = "00:00:00:00:00:00:00:00:00:00:00:00:00:00:00:00";

/// `sys_dev_path`: `/sys/class/net`, or a fixture standing in for it.
#[derive(Debug, Clone)]
pub struct Sys {
    root: PathBuf,
}

impl Default for Sys {
    fn default() -> Self {
        Self::real()
    }
}

/// One row of `get_interfaces`: upstream's `(name, mac, driver, device_id)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub name: String,
    pub mac: String,
    pub driver: Option<String>,
    pub device_id: Option<String>,
}

/// Which of `get_interfaces`' filters to apply.
///
/// Upstream spells these as seven keyword arguments, all defaulting to true
/// except `log_filtered_reasons`. Only two callers ever change them, and they
/// change them wholesale, so the two shapes are named here rather than left as
/// seven booleans at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filters {
    /// Every filter on: `get_interfaces()` with no arguments, which is what
    /// the Azure network-config generator uses.
    All,
    /// What `find_candidate_nics_on_linux` asks for — only bridges, bonds,
    /// failover NICs and Hyper-V VFs are dropped, because a slave or a NIC
    /// with an inherited mac may still be the one holding the lease.
    Candidates,
}

impl Sys {
    #[must_use]
    pub fn real() -> Self {
        Self {
            root: PathBuf::from("/sys/class/net"),
        }
    }

    #[must_use]
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn dev_path(&self, devname: &str, path: &str) -> PathBuf {
        self.root.join(devname).join(path)
    }

    /// `get_devicelist`, sorted so the walk is deterministic. Upstream relies
    /// on `os.listdir` order, which is not.
    #[must_use]
    pub fn devicelist(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort_by_key(|name| natural_sort_key(name));
        names
    }

    /// `read_sys_net_safe`: the attribute's text, stripped, or `None`.
    #[must_use]
    pub fn read(&self, devname: &str, field: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.dev_path(devname, field)).ok()?;
        Some(text.trim().to_owned())
    }

    /// `read_sys_net_int`.
    #[must_use]
    pub fn read_int(&self, devname: &str, field: &str) -> Option<i64> {
        self.read(devname, field)?.parse().ok()
    }

    #[must_use]
    pub fn is_bridge(&self, devname: &str) -> bool {
        self.dev_path(devname, "bridge").exists()
    }

    #[must_use]
    pub fn is_bond(&self, devname: &str) -> bool {
        self.dev_path(devname, "bonding").exists()
    }

    /// `is_vlan`: `DEVTYPE=vlan` as a whole line of `uevent`.
    #[must_use]
    pub fn is_vlan(&self, devname: &str) -> bool {
        self.read(devname, "uevent")
            .is_some_and(|uevent| uevent.lines().any(|line| line == "DEVTYPE=vlan"))
    }

    /// `is_renamed`: name assign type 3 (user) or 4 (renamed).
    #[must_use]
    pub fn is_renamed(&self, devname: &str) -> bool {
        matches!(
            self.read(devname, "name_assign_type").as_deref(),
            Some("3" | "4")
        )
    }

    /// `interface_has_own_mac`: `addr_assign_type` 2 means the address was
    /// stolen from another device. A missing attribute counts as its own.
    #[must_use]
    pub fn has_own_mac(&self, devname: &str) -> bool {
        self.read_int(devname, "addr_assign_type") != Some(2)
    }

    #[must_use]
    pub fn master(&self, devname: &str) -> Option<PathBuf> {
        let path = self.dev_path(devname, "master");
        path.exists().then_some(path)
    }

    #[must_use]
    pub fn master_is_bridge_or_bond(&self, devname: &str) -> bool {
        let Some(master) = self.master(devname) else {
            return false;
        };
        master.join("bonding").exists() || master.join("bridge").exists()
    }

    /// `master_is_openvswitch`, which looks for the `upper_ovs-system` link on
    /// the *device*, not on its master.
    #[must_use]
    pub fn master_is_openvswitch(&self, devname: &str) -> bool {
        self.master(devname).is_some()
            && self.dev_path(devname, "upper_ovs-system").exists()
    }

    /// `get_interface_mac`, preferring a bond slave's permanent address.
    #[must_use]
    pub fn mac(&self, devname: &str) -> Option<String> {
        if self.dev_path(devname, "bonding_slave").is_dir() {
            return self.read(devname, "bonding_slave/perm_hwaddr");
        }
        self.read(devname, "address")
    }

    /// `device_driver`: the basename of the `device/driver` symlink.
    #[must_use]
    pub fn driver(&self, devname: &str) -> Option<String> {
        let link = std::fs::read_link(self.dev_path(devname, "device/driver")).ok()?;
        Some(link.file_name()?.to_string_lossy().into_owned())
    }

    /// `device_devid`.
    #[must_use]
    pub fn device_id(&self, devname: &str) -> Option<String> {
        self.read(devname, "device/device")
    }

    /// `has_netfail_standby_feature`: bit 62 of `device/features`.
    #[must_use]
    fn has_netfail_standby_feature(&self, devname: &str) -> bool {
        let features = self.read(devname, "device/features").unwrap_or_default();
        features.len() >= 64 && features.as_bytes().get(62) == Some(&b'1')
    }

    /// `is_netfailover`: the primary or the standby of a virtio-net failover
    /// trio. Both are meant to be driven through the master, never directly.
    #[must_use]
    pub fn is_netfailover(&self, devname: &str, driver: Option<&str>) -> bool {
        // Both halves need a master; without one this is the master itself.
        let Some(master) = self.master(devname) else {
            return false;
        };
        let driver = driver
            .map(ToOwned::to_owned)
            .or_else(|| self.driver(devname));
        if driver.as_deref() == Some("virtio_net") {
            // standby: the device itself carries the standby bit.
            return self.has_netfail_standby_feature(devname);
        }
        // primary: a non-virtio device whose master is a virtio-net standby.
        let Some(master_name) = std::fs::read_link(&master).ok().and_then(|target| {
            Some(target.file_name()?.to_string_lossy().into_owned())
        }) else {
            return false;
        };
        self.driver(&master_name).as_deref() == Some("virtio_net")
            && self.has_netfail_standby_feature(&master_name)
    }

    /// `get_interfaces`.
    ///
    /// The Open vSwitch filter is not ported: it shells out to `ovs-vsctl` to
    /// enumerate internal ports, and a machine with OVS running is not a
    /// machine this port can provision yet. The Hyper-V VF filter *is* ported,
    /// because it is the one that matters on Azure — a VF NIC mirrors the
    /// synthetic `hv_netvsc` mac and must never be configured directly.
    #[must_use]
    pub fn interfaces(&self, filters: Filters) -> Vec<Interface> {
        let all = filters == Filters::All;
        let mut found = Vec::new();
        for name in self.devicelist() {
            if self.is_bridge(&name) || self.is_bond(&name) {
                continue;
            }
            if all && self.is_vlan(&name) {
                continue;
            }
            if all && !self.has_own_mac(&name) {
                continue;
            }
            if all
                && self.master(&name).is_some()
                && !self.master_is_bridge_or_bond(&name)
                && !self.master_is_openvswitch(&name)
            {
                continue;
            }
            if self.is_netfailover(&name, None) {
                continue;
            }
            let Some(mac) = self.mac(&name).filter(|mac| !mac.is_empty()) else {
                continue;
            };
            if all && name != "lo" && ZERO_MAC.starts_with(&mac) {
                continue;
            }
            found.push(Interface {
                driver: self.driver(&name),
                device_id: self.device_id(&name),
                name,
                mac,
            });
        }
        // Unconditional upstream: `filter_hyperv_vf_with_synthetic` is the one
        // argument of `get_interfaces` no caller ever turns off, and leaving
        // it out of the candidate pass put an accelerated-networking VF in
        // front of the synthetic NIC on Azure.
        filter_hyperv_vf_with_synthetic(&mut found);
        found
    }

    /// `find_candidate_nics_on_linux`, minus the `udevadm settle` it opens
    /// with — settling is a side effect on the machine, and the caller retries
    /// anyway, so it is left to the boot's own udev.
    ///
    /// The ordering is upstream's: interfaces with carrier first, then those
    /// that could plausibly acquire one, each naturally sorted with `eth0`
    /// pulled to the front of its group.
    #[must_use]
    pub fn candidate_nics(&self) -> Vec<String> {
        let mut connected = Vec::new();
        let mut possibly = Vec::new();
        for interface in self.interfaces(Filters::Candidates) {
            let name = interface.name;
            if name == "lo" || name.starts_with("veth") {
                continue;
            }
            if self.read_int(&name, "carrier").unwrap_or(0) != 0 {
                connected.push(name);
                continue;
            }
            if self.read_int(&name, "dormant").unwrap_or(0) != 0 {
                possibly.push(name);
                continue;
            }
            if matches!(
                self.read(&name, "operstate").as_deref(),
                Some("dormant" | "down" | "lowerlayerdown" | "unknown")
            ) {
                possibly.push(name);
            }
        }

        let mut ordered = Vec::new();
        for mut group in [connected, possibly] {
            group.sort_by_key(|name| natural_sort_key(name));
            if let Some(index) =
                group.iter().position(|n| n == DEFAULT_PRIMARY_INTERFACE)
            {
                let primary = group.remove(index);
                group.insert(0, primary);
            }
            ordered.extend(group);
        }
        ordered
    }

    /// `find_fallback_nic_on_linux`, and Azure's `find_primary_nic` — the two
    /// are the same function, both being the first candidate.
    #[must_use]
    pub fn fallback_nic(&self) -> Option<String> {
        self.candidate_nics().into_iter().next()
    }

    /// `is_netfail_master`: the virtio-net device the failover pair hangs off.
    ///
    /// Distinct from [`Self::is_netfailover`], which answers the opposite
    /// question — that one is true for the two *members*, this one for the
    /// master they are enslaved to.
    #[must_use]
    pub fn is_netfail_master(&self, devname: &str) -> bool {
        self.master(devname).is_none()
            && self.driver(devname).as_deref() == Some("virtio_net")
            && self.has_netfail_standby_feature(devname)
    }

    /// `generate_fallback_config`: v2 dhcp on the NIC most likely connected.
    ///
    /// `None` when there is no candidate at all, which upstream treats as
    /// "give up" rather than as an error.
    #[must_use]
    pub fn generate_fallback_config(&self, config_driver: bool) -> Option<Object> {
        let target = self.fallback_nic()?;

        let mut matcher = Object::new();
        if self.is_netfail_master(&target) {
            // A netfail pair duplicates its mac across all three devices, so
            // the name is the only thing that picks one out.
            matcher.insert("name".to_owned(), Value::from(target.clone()));
        } else {
            let mac = self.read(&target, "address").unwrap_or_default();
            matcher.insert("macaddress".to_owned(), Value::from(mac.to_lowercase()));
        }
        if config_driver {
            if let Some(driver) = self.driver(&target) {
                matcher.insert("driver".to_owned(), Value::from(driver));
            }
        }

        let mut cfg = Object::new();
        cfg.insert("dhcp4".to_owned(), Value::Bool(true));
        cfg.insert("dhcp6".to_owned(), Value::Bool(true));
        cfg.insert("set-name".to_owned(), Value::from(target.clone()));
        cfg.insert("match".to_owned(), Value::Object(matcher));

        let mut ethernets = Object::new();
        ethernets.insert(target, Value::Object(cfg));

        let mut out = Object::new();
        out.insert("ethernets".to_owned(), Value::Object(ethernets));
        out.insert("version".to_owned(), Value::from(2));
        Some(out)
    }
}

/// `filter_hyperv_vf_with_synthetic_interface`.
///
/// A Hyper-V SR-IOV VF registers with the same mac as the synthetic
/// `hv_netvsc` interface it will be enslaved to. Configuring the VF instead of
/// the synthetic NIC races the kernel and loses, so any duplicate of an
/// `hv_netvsc` mac is dropped.
fn filter_hyperv_vf_with_synthetic(interfaces: &mut Vec<Interface>) {
    let synthetic: Vec<String> = interfaces
        .iter()
        .filter(|i| i.driver.as_deref() == Some("hv_netvsc"))
        .map(|i| i.mac.clone())
        .collect();
    if synthetic.is_empty() {
        return;
    }
    interfaces.retain(|i| {
        i.driver.as_deref() == Some("hv_netvsc") || !synthetic.contains(&i.mac)
    });
}

/// `util.natural_sort_key`: split into digit and non-digit runs so `eth10`
/// sorts after `eth2`.
fn natural_sort_key(name: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut rest = name;
    while !rest.is_empty() {
        let digits = rest.starts_with(|c: char| c.is_ascii_digit());
        let end = rest
            .find(|c: char| c.is_ascii_digit() != digits)
            .unwrap_or(rest.len());
        let (head, tail) = rest.split_at(end);
        chunks.push(if digits {
            // A run of digits longer than a u64 sorts as text; no interface
            // name has one, and falling back beats refusing to sort.
            head.parse()
                .map_or_else(|_| Chunk::Text(head.to_lowercase()), Chunk::Number)
        } else {
            Chunk::Text(head.to_lowercase())
        });
        rest = tail;
    }
    chunks
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Chunk {
    // Upstream compares `int` against `str` chunk-wise; Python 3 would raise,
    // but the alternation of the split means a number is only ever compared
    // with a number. Numbers order before text here so a mismatch is at least
    // total.
    Number(u64),
    Text(String),
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

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn sys(&self) -> Sys {
            Sys::at(self.dir.path())
        }

        fn dev(&self, name: &str) -> PathBuf {
            let path = self.dir.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            path
        }

        fn attr(&self, name: &str, field: &str, value: &str) {
            let path = self.dev(name).join(field);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, value).unwrap();
        }

        /// A plain ethernet NIC with a mac, a driver and a carrier.
        fn nic(&self, name: &str, mac: &str, driver: &str) {
            self.attr(name, "address", &format!("{mac}\n"));
            self.attr(name, "addr_assign_type", "0\n");
            self.attr(name, "carrier", "1\n");
            self.attr(name, "uevent", "DEVTYPE=eth\n");
            let driver_dir = self.dir.path().join("drivers").join(driver);
            std::fs::create_dir_all(&driver_dir).unwrap();
            let device = self.dev(name).join("device");
            std::fs::create_dir_all(&device).unwrap();
            std::os::unix::fs::symlink(&driver_dir, device.join("driver")).unwrap();
        }
    }

    #[test]
    fn a_plain_nic_is_reported_with_its_mac_and_driver() {
        let fixture = Fixture::new();
        fixture.nic("eth0", "00:11:22:33:44:55", "hv_netvsc");
        let interfaces = fixture.sys().interfaces(Filters::All);
        assert_eq!(interfaces.len(), 1);
        assert_eq!(interfaces[0].name, "eth0");
        assert_eq!(interfaces[0].mac, "00:11:22:33:44:55");
        assert_eq!(interfaces[0].driver.as_deref(), Some("hv_netvsc"));
    }

    #[test]
    fn bridges_bonds_vlans_and_stolen_macs_are_dropped() {
        let fixture = Fixture::new();
        fixture.nic("eth0", "00:11:22:33:44:55", "hv_netvsc");

        fixture.nic("br0", "00:11:22:33:44:66", "bridge");
        std::fs::create_dir_all(fixture.dev("br0").join("bridge")).unwrap();

        fixture.nic("bond0", "00:11:22:33:44:77", "bonding");
        std::fs::create_dir_all(fixture.dev("bond0").join("bonding")).unwrap();

        fixture.nic("eth0.10", "00:11:22:33:44:55", "hv_netvsc");
        fixture.attr("eth0.10", "uevent", "DEVTYPE=vlan\n");

        fixture.nic("stolen", "00:11:22:33:44:88", "hv_netvsc");
        fixture.attr("stolen", "addr_assign_type", "2\n");

        let names: Vec<String> = fixture
            .sys()
            .interfaces(Filters::All)
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names, vec!["eth0"]);
    }

    #[test]
    fn a_hyperv_vf_sharing_the_synthetic_mac_is_dropped_but_the_synthetic_stays() {
        let fixture = Fixture::new();
        fixture.nic("eth0", "00:11:22:33:44:55", "hv_netvsc");
        fixture.nic("enP1s2", "00:11:22:33:44:55", "mana");

        // Both passes drop the VF: `filter_hyperv_vf_with_synthetic` is the
        // one `get_interfaces` argument no upstream caller turns off. An
        // accelerated-networking Azure VM has exactly this pair, and the
        // candidate walk feeds the fallback network config.
        for filters in [Filters::All, Filters::Candidates] {
            let found = fixture.sys().interfaces(filters);
            assert_eq!(found.len(), 1, "{filters:?}");
            assert_eq!(found[0].name, "eth0", "{filters:?}");
        }
    }

    #[test]
    fn a_nic_with_carrier_beats_one_that_is_merely_down() {
        let fixture = Fixture::new();
        fixture.nic("eth9", "00:11:22:33:44:99", "hv_netvsc");
        fixture.attr("eth9", "carrier", "0\n");
        fixture.attr("eth9", "operstate", "down\n");
        fixture.nic("eth10", "00:11:22:33:44:aa", "hv_netvsc");

        assert_eq!(
            fixture.sys().candidate_nics(),
            vec!["eth10".to_owned(), "eth9".to_owned()]
        );
        assert_eq!(fixture.sys().fallback_nic().as_deref(), Some("eth10"));
    }

    #[test]
    fn eth0_is_pulled_to_the_front_of_its_group() {
        let fixture = Fixture::new();
        for (name, mac) in [
            ("eth1", "00:11:22:33:44:01"),
            ("eth0", "00:11:22:33:44:00"),
            ("eth2", "00:11:22:33:44:02"),
        ] {
            fixture.nic(name, mac, "hv_netvsc");
        }
        assert_eq!(fixture.sys().fallback_nic().as_deref(), Some("eth0"));
    }

    #[test]
    fn loopback_and_veth_are_never_candidates() {
        let fixture = Fixture::new();
        fixture.nic("lo", "00:00:00:00:00:00", "hv_netvsc");
        fixture.nic("veth0", "00:11:22:33:44:bb", "veth");
        assert_eq!(fixture.sys().fallback_nic(), None);
    }

    #[test]
    fn eth10_sorts_after_eth2() {
        assert!(natural_sort_key("eth2") < natural_sort_key("eth10"));
        assert!(natural_sort_key("eth2") < natural_sort_key("eth2a"));
    }
}
