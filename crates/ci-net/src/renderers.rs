//! `cloudinit.net.renderers`: which renderer this machine can actually use.
//!
//! This is the first thing in `ci-net` that looks at the machine it is running
//! on. It has to be: upstream picks a renderer by probing for `netplan`, for
//! `ifup`, for a running `NetworkManager`, and the answer is different on every
//! image. The probes are kept here, apart from the renderers themselves, so
//! that the rendering half stays a pure function of its input.

use ci_config::Object;
// `distros.uses_systemd`, which is where upstream keeps it too.
use ci_core::status::uses_systemd;
use ci_sys::subp;

use crate::renderer::{Netplan, Renderer};

/// `renderers.DEFAULT_PRIORITY`.
pub const DEFAULT_PRIORITY: &[&str] = &[
    "eni",
    "sysconfig",
    "netplan",
    "network-manager",
    "freebsd",
    "netbsd",
    "openbsd",
    "networkd",
];

/// One renderer module: its `NAME_TO_RENDERER` key and its `available()`.
pub type Probe = (&'static str, fn() -> bool);

/// `renderers.NAME_TO_RENDERER`, paired with each module's `available()`.
///
/// Note the order: upstream's dict is alphabetical and only `DEFAULT_PRIORITY`
/// orders the search, so nothing may depend on this sequence beyond the error
/// message that lists unknown names.
pub static NAME_TO_RENDERER: &[Probe] = &[
    ("eni", eni_available),
    ("freebsd", freebsd_available),
    ("netbsd", netbsd_available),
    ("netplan", netplan_available),
    ("network-manager", network_manager_available),
    ("networkd", networkd_available),
    ("openbsd", openbsd_available),
    ("sysconfig", sysconfig_available),
];

/// Why [`select`] could not name a renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// `ValueError`: the priority list names something not in
    /// `NAME_TO_RENDERER`.
    Unknown(Vec<String>),
    /// `RendererNotFoundError`: nothing in the priority list is available.
    NotFound(Vec<String>),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(names) => write!(
                f,
                "Unknown renderers provided in priority list: {}",
                py_list(names)
            ),
            Self::NotFound(priority) => write!(
                f,
                "No available network renderers found. Searched through list: {}",
                py_list(priority)
            ),
        }
    }
}

/// `"%s" % a_list_of_str`, which Python renders with `repr` on each element.
pub(crate) fn py_list(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| ci_config::repr_str(n)).collect();
    format!("[{}]", quoted.join(", "))
}

impl std::error::Error for Error {}

/// `renderers.search`: every renderer in `priority` that this machine has.
///
/// `first` stops at the first hit, which is the only thing [`select`] wants
/// and saves probing for an `ifup` that will never be consulted.
pub fn search(
    priority: Option<&[String]>,
    first: bool,
) -> Result<Vec<&'static str>, Error> {
    let default: Vec<String> =
        DEFAULT_PRIORITY.iter().map(|s| (*s).to_owned()).collect();
    let priority = priority.unwrap_or(&default);

    let unknown: Vec<String> = priority
        .iter()
        .filter(|name| !NAME_TO_RENDERER.iter().any(|(k, _)| k == name))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(Error::Unknown(unknown));
    }

    let mut found = Vec::new();
    for name in priority {
        let Some((name, available)) = NAME_TO_RENDERER.iter().find(|(k, _)| k == name)
        else {
            continue;
        };
        if available() {
            found.push(*name);
            if first {
                return Ok(found);
            }
        }
    }
    Ok(found)
}

/// `renderers.select`: the one renderer this machine will use.
pub fn select(priority: Option<&[String]>) -> Result<&'static str, Error> {
    let found = search(priority, true)?;
    found.first().copied().ok_or_else(|| {
        Error::NotFound(priority.map_or_else(
            || DEFAULT_PRIORITY.iter().map(|s| (*s).to_owned()).collect(),
            <[String]>::to_vec,
        ))
    })
}

/// Instantiate the renderer `select` named, with the distro's config for it.
///
/// Seven of the eight have no body in this port yet. They still take part in
/// selection, because a machine that upstream would render with `sysconfig`
/// must not silently get netplan instead — it must fail the way it fails.
pub fn build(
    name: &str,
    config: &Object,
) -> Result<Box<dyn Renderer>, crate::renderer::Error> {
    match name {
        "netplan" => Ok(Box::new(Netplan::from_config(config))),
        "eni" => Err(crate::renderer::Error::NotImplemented("eni")),
        "sysconfig" => Err(crate::renderer::Error::NotImplemented("sysconfig")),
        "network-manager" => {
            Err(crate::renderer::Error::NotImplemented("network-manager"))
        }
        "networkd" => Err(crate::renderer::Error::NotImplemented("networkd")),
        "freebsd" => Err(crate::renderer::Error::NotImplemented("freebsd")),
        "netbsd" => Err(crate::renderer::Error::NotImplemented("netbsd")),
        "openbsd" => Err(crate::renderer::Error::NotImplemented("openbsd")),
        _ => Err(crate::renderer::Error::NotImplemented("unknown")),
    }
}

/// `eni.available`: the ifupdown tools *and* the file they read.
pub(crate) fn eni_available() -> bool {
    let search = ["/sbin", "/usr/sbin"];
    ["ifquery", "ifup", "ifdown"]
        .iter()
        .all(|p| subp::which_in(p, &search).is_some())
        && std::path::Path::new("/etc/network/interfaces").is_file()
}

/// `netplan.available`.
pub(crate) fn netplan_available() -> bool {
    subp::which_in("netplan", &["/usr/sbin", "/sbin"]).is_some()
}

/// `networkd.available`. Note the search list is `/usr/sbin` and `/bin`, not
/// the `/sbin` pair the others use.
pub(crate) fn networkd_available() -> bool {
    let search = ["/usr/sbin", "/bin"];
    ["ip", "systemctl"]
        .iter()
        .all(|p| subp::which_in(p, &search).is_some())
}

/// `network_manager.available`: `nmcli` present, and the unit not disabled.
///
/// Upstream only asks systemd when systemd is running, and treats a failing
/// `is-enabled` — including "no such unit" — as "not active".
pub(crate) fn network_manager_available() -> bool {
    if subp::which("nmcli").is_none() {
        return false;
    }
    if !uses_systemd() {
        return true;
    }
    subp::run(["systemctl", "is-enabled", "NetworkManager.service"])
        .is_ok_and(|out| out.code == Some(0))
}

/// `sysconfig.available`: a distro that ships it, plus one of two layouts.
fn sysconfig_available() -> bool {
    let variant = ci_core::sysinfo::system_info()
        .get("variant")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    if !KNOWN_DISTROS.contains(&variant.as_str()) {
        return false;
    }
    available_sysconfig() || available_nm_ifcfg_rh()
}

/// `sysconfig.KNOWN_DISTROS`.
const KNOWN_DISTROS: &[&str] = &[
    "almalinux",
    "centos",
    "cloudlinux",
    "eurolinux",
    "fedora",
    "miraclelinux",
    "openeuler",
    "OpenCloudOS",
    "openmandriva",
    "rhel",
    "rocky",
    "suse",
    "TencentOS",
    "virtuozzo",
];

fn available_sysconfig() -> bool {
    let search = ["/sbin", "/usr/sbin"];
    let tools = ["ifup", "ifdown"]
        .iter()
        .all(|p| subp::which_in(p, &search).is_some());
    let layout = [
        "/etc/sysconfig/network-scripts/network-functions",
        "/etc/sysconfig/config",
    ]
    .iter()
    .any(|p| std::path::Path::new(p).is_file());
    tools && layout
}

/// The `NetworkManager` ifcfg-rh plugin, which reads the same files.
///
/// Upstream globs `usr/lib*/NetworkManager/*/libnm-settings-plugin-ifcfg-rh.so`;
/// the two-level wildcard is spelled out here rather than pulling in a glob
/// crate for one call site.
pub(crate) fn available_nm_ifcfg_rh() -> bool {
    const PLUGIN: &str = "libnm-settings-plugin-ifcfg-rh.so";
    let Ok(usr) = std::fs::read_dir("/usr") else {
        return false;
    };
    usr.flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("lib"))
        .any(|entry| {
            std::fs::read_dir(entry.path().join("NetworkManager"))
                .into_iter()
                .flatten()
                .flatten()
                .any(|version| version.path().join(PLUGIN).exists())
        })
}

/// `freebsd.available`, which also covers `DragonFly`.
fn freebsd_available() -> bool {
    false
}

/// `netbsd.available`.
fn netbsd_available() -> bool {
    false
}

/// `openbsd.available`.
fn openbsd_available() -> bool {
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn every_default_priority_entry_is_a_known_renderer() {
        for name in DEFAULT_PRIORITY {
            assert!(NAME_TO_RENDERER.iter().any(|(k, _)| k == name), "{name}");
        }
        assert_eq!(DEFAULT_PRIORITY.len(), NAME_TO_RENDERER.len());
    }

    #[test]
    fn an_unknown_name_is_rejected_before_anything_is_probed() {
        let err =
            search(Some(&names(&["netplan", "nosuch", "alsono"])), false).unwrap_err();
        assert_eq!(
            err,
            Error::Unknown(vec!["nosuch".to_owned(), "alsono".to_owned()])
        );
        assert_eq!(
            err.to_string(),
            "Unknown renderers provided in priority list: ['nosuch', 'alsono']"
        );
    }

    #[test]
    fn the_bsd_renderers_are_never_available_on_linux() {
        assert_eq!(
            search(Some(&names(&["freebsd", "netbsd", "openbsd"])), false),
            Ok(vec![])
        );
        let err = select(Some(&names(&["freebsd"]))).unwrap_err();
        assert_eq!(
            err.to_string(),
            "No available network renderers found. Searched through list: \
             ['freebsd']"
        );
    }

    #[test]
    fn an_empty_priority_list_finds_nothing_and_says_so() {
        assert_eq!(search(Some(&[]), false), Ok(vec![]));
        assert_eq!(select(Some(&[])), Err(Error::NotFound(vec![])));
    }

    #[test]
    fn searching_the_live_host_agrees_with_selecting_it() {
        // Whatever this machine has, `select` must name the first thing
        // `search` found and nothing else.
        let all = search(None, false).unwrap();
        match select(None) {
            Ok(first) => assert_eq!(Some(first), all.first().copied()),
            Err(err) => {
                assert!(all.is_empty());
                assert!(matches!(err, Error::NotFound(_)));
            }
        }
    }

    #[test]
    fn only_netplan_can_be_built() {
        assert!(build("netplan", &Object::new()).is_ok());
        for name in ["eni", "sysconfig", "networkd", "network-manager"] {
            let Err(err) = build(name, &Object::new()) else {
                panic!("{name} built");
            };
            assert_eq!(
                err.to_string(),
                format!("network renderer '{name}' is not implemented yet")
            );
        }
    }
}
