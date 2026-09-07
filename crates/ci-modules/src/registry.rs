//! The `cc_*` module table.
//!
//! Upstream discovers modules by importing `cloudinit.config.cc_<name>` and
//! reading the module-level `meta` dict. There is no import to do here, so the
//! same information is a static table: the set of names that resolve, and for
//! each one the three `meta` fields the engine actually reads (`frequency`,
//! `distros`, `activate_by_schema_keys`). Everything else in `meta` — `id`,
//! `examples`, the schema — belongs to the module itself.
//!
//! Generated from the installed cloud-init; regenerate with the snippet in
//! `docs/COMPAT.md` when the reference version moves. The table is what makes
//! "module not found" reproducible: a name that is not here gets upstream's
//! warning and is dropped, exactly as a failed import would be.

use ci_core::semaphore::Frequency;

/// One `cc_*` module, as far as the engine is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Module {
    /// Import name, `cc_` prefix included, as `form_module_name` produces it.
    pub name: &'static str,
    /// `meta["frequency"]`: the default when the config does not say.
    pub frequency: Frequency,
    /// `meta["distros"]`: `["all"]` means every distro.
    pub distros: &'static [&'static str],
    /// `meta["activate_by_schema_keys"]`: empty means "always applicable".
    pub activate_by_schema_keys: &'static [&'static str],
}

/// Every module `cloudinit.config` ships, sorted by name.
pub const MODULES: &[Module] = &[
    Module {
        name: "cc_ansible",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &["ansible"],
    },
    Module {
        name: "cc_apk_configure",
        frequency: Frequency::Instance,
        distros: &["alpine"],
        activate_by_schema_keys: &["apk_repos"],
    },
    Module {
        name: "cc_apt_configure",
        frequency: Frequency::Instance,
        distros: &["ubuntu", "debian", "raspberry-pi-os"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_apt_pipelining",
        frequency: Frequency::Instance,
        distros: &["ubuntu", "debian", "raspberry-pi-os"],
        activate_by_schema_keys: &["apt_pipelining"],
    },
    Module {
        name: "cc_bootcmd",
        frequency: Frequency::Always,
        distros: &["all"],
        activate_by_schema_keys: &["bootcmd"],
    },
    Module {
        name: "cc_byobu",
        frequency: Frequency::Instance,
        distros: &["ubuntu", "debian", "raspberry-pi-os"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_ca_certs",
        frequency: Frequency::Instance,
        distros: &[
            "almalinux",
            "aosc",
            "centos",
            "cloudlinux",
            "alpine",
            "debian",
            "fedora",
            "raspberry-pi-os",
            "rhel",
            "rocky",
            "opensuse",
            "opensuse-microos",
            "opensuse-tumbleweed",
            "opensuse-leap",
            "sle_hpc",
            "sle-micro",
            "sles",
            "ubuntu",
            "photon",
        ],
        activate_by_schema_keys: &["ca_certs", "ca-certs"],
    },
    Module {
        name: "cc_chef",
        frequency: Frequency::Always,
        distros: &["all"],
        activate_by_schema_keys: &["chef"],
    },
    Module {
        name: "cc_disable_ec2_metadata",
        frequency: Frequency::Always,
        distros: &["all"],
        activate_by_schema_keys: &["disable_ec2_metadata"],
    },
    Module {
        name: "cc_disk_setup",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &["disk_setup", "fs_setup"],
    },
    Module {
        name: "cc_fan",
        frequency: Frequency::Instance,
        distros: &["ubuntu"],
        activate_by_schema_keys: &["fan"],
    },
    Module {
        name: "cc_final_message",
        frequency: Frequency::Always,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_growpart",
        frequency: Frequency::Always,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_grub_dpkg",
        frequency: Frequency::Instance,
        distros: &["ubuntu", "debian"],
        activate_by_schema_keys: &["grub_dpkg", "grub-dpkg"],
    },
    Module {
        name: "cc_install_hotplug",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_keyboard",
        frequency: Frequency::Instance,
        distros: &[
            "alpine",
            "arch",
            "debian",
            "ubuntu",
            "raspberry-pi-os",
            "almalinux",
            "amazon",
            "azurelinux",
            "centos",
            "cloudlinux",
            "eurolinux",
            "fedora",
            "mariner",
            "miraclelinux",
            "openmandriva",
            "photon",
            "rhel",
            "rocky",
            "virtuozzo",
            "opensuse",
            "opensuse-leap",
            "opensuse-microos",
            "opensuse-tumbleweed",
            "sle_hpc",
            "sle-micro",
            "sles",
            "suse",
        ],
        activate_by_schema_keys: &["keyboard"],
    },
    Module {
        name: "cc_keys_to_console",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_landscape",
        frequency: Frequency::Instance,
        distros: &["ubuntu"],
        activate_by_schema_keys: &["landscape"],
    },
    Module {
        name: "cc_locale",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_lxd",
        frequency: Frequency::Instance,
        distros: &["ubuntu"],
        activate_by_schema_keys: &["lxd"],
    },
    Module {
        name: "cc_mcollective",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &["mcollective"],
    },
    Module {
        name: "cc_mounts",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_ntp",
        frequency: Frequency::Instance,
        distros: &[
            "almalinux",
            "alpine",
            "aosc",
            "azurelinux",
            "centos",
            "cloudlinux",
            "cos",
            "debian",
            "eurolinux",
            "fedora",
            "freebsd",
            "mariner",
            "miraclelinux",
            "openbsd",
            "openeuler",
            "OpenCloudOS",
            "openmandriva",
            "opensuse",
            "opensuse-microos",
            "opensuse-tumbleweed",
            "opensuse-leap",
            "photon",
            "raspberry-pi-os",
            "rhel",
            "rocky",
            "sle_hpc",
            "sle-micro",
            "sles",
            "TencentOS",
            "ubuntu",
            "virtuozzo",
        ],
        activate_by_schema_keys: &["ntp"],
    },
    Module {
        name: "cc_package_update_upgrade_install",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[
            "apt_update",
            "package_update",
            "apt_upgrade",
            "package_upgrade",
            "packages",
        ],
    },
    Module {
        name: "cc_phone_home",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &["phone_home"],
    },
    Module {
        name: "cc_power_state_change",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &["power_state"],
    },
    Module {
        name: "cc_puppet",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &["puppet"],
    },
    Module {
        name: "cc_raspberry_pi",
        frequency: Frequency::Instance,
        distros: &["raspberry-pi-os"],
        activate_by_schema_keys: &["rpi"],
    },
    Module {
        name: "cc_reset_rmc",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_resizefs",
        frequency: Frequency::Always,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_resolv_conf",
        frequency: Frequency::Instance,
        distros: &[
            "alpine",
            "azurelinux",
            "fedora",
            "mariner",
            "opensuse",
            "opensuse-leap",
            "opensuse-microos",
            "opensuse-tumbleweed",
            "photon",
            "rhel",
            "sle_hpc",
            "sle-micro",
            "sles",
            "openeuler",
        ],
        activate_by_schema_keys: &["manage_resolv_conf"],
    },
    Module {
        name: "cc_rh_subscription",
        frequency: Frequency::Instance,
        distros: &["fedora", "rhel"],
        activate_by_schema_keys: &["rh_subscription"],
    },
    Module {
        name: "cc_rsyslog",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &["rsyslog"],
    },
    Module {
        name: "cc_runcmd",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &["runcmd"],
    },
    Module {
        name: "cc_salt_minion",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &["salt_minion"],
    },
    Module {
        name: "cc_scripts_per_boot",
        frequency: Frequency::Always,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_scripts_per_instance",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_scripts_per_once",
        frequency: Frequency::Once,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_scripts_user",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_scripts_vendor",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_seed_random",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_set_hostname",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_set_passwords",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_snap",
        frequency: Frequency::Instance,
        distros: &["ubuntu"],
        activate_by_schema_keys: &["snap"],
    },
    Module {
        name: "cc_spacewalk",
        frequency: Frequency::Instance,
        distros: &["rhel", "fedora", "openeuler"],
        activate_by_schema_keys: &["spacewalk"],
    },
    Module {
        name: "cc_ssh",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_ssh_authkey_fingerprints",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_ssh_import_id",
        frequency: Frequency::Instance,
        distros: &["alpine", "cos", "debian", "raspberry-pi-os", "ubuntu"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_timezone",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &["timezone"],
    },
    Module {
        name: "cc_ubuntu_autoinstall",
        frequency: Frequency::Once,
        distros: &["ubuntu"],
        activate_by_schema_keys: &["autoinstall"],
    },
    Module {
        name: "cc_ubuntu_drivers",
        frequency: Frequency::Instance,
        distros: &["ubuntu"],
        activate_by_schema_keys: &["drivers"],
    },
    Module {
        name: "cc_ubuntu_pro",
        frequency: Frequency::Instance,
        distros: &["ubuntu"],
        activate_by_schema_keys: &[
            "ubuntu_pro",
            "ubuntu-advantage",
            "ubuntu_advantage",
        ],
    },
    Module {
        name: "cc_update_etc_hosts",
        frequency: Frequency::Always,
        distros: &["all"],
        activate_by_schema_keys: &["manage_etc_hosts"],
    },
    Module {
        name: "cc_update_hostname",
        frequency: Frequency::Always,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_users_groups",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &[],
    },
    Module {
        name: "cc_wireguard",
        frequency: Frequency::Instance,
        distros: &["ubuntu"],
        activate_by_schema_keys: &["wireguard"],
    },
    Module {
        name: "cc_write_files",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &["write_files"],
    },
    Module {
        name: "cc_write_files_deferred",
        frequency: Frequency::Instance,
        distros: &["all"],
        activate_by_schema_keys: &["write_files"],
    },
    Module {
        name: "cc_yum_add_repo",
        frequency: Frequency::Instance,
        distros: &[
            "almalinux",
            "azurelinux",
            "centos",
            "cloudlinux",
            "eurolinux",
            "fedora",
            "mariner",
            "openeuler",
            "OpenCloudOS",
            "openmandriva",
            "photon",
            "rhel",
            "rocky",
            "TencentOS",
            "virtuozzo",
        ],
        activate_by_schema_keys: &["yum_repos"],
    },
    Module {
        name: "cc_zypper_add_repo",
        frequency: Frequency::Always,
        distros: &[
            "opensuse",
            "opensuse-microos",
            "opensuse-tumbleweed",
            "opensuse-leap",
            "sle_hpc",
            "sle-micro",
            "sles",
        ],
        activate_by_schema_keys: &["zypper"],
    },
];

/// `importer.find_module`: the canonical name resolves, or it does not.
#[must_use]
pub fn find(name: &str) -> Option<&'static Module> {
    MODULES.iter().find(|module| module.name == name)
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
    fn table_is_sorted_and_prefixed() {
        let mut previous = "";
        for module in MODULES {
            assert!(module.name.starts_with("cc_"), "{}", module.name);
            assert!(module.name > previous, "{} after {previous}", module.name);
            previous = module.name;
        }
    }

    #[test]
    fn finds_by_canonical_name() {
        assert_eq!(
            find("cc_bootcmd").map(|m| m.frequency),
            Some(Frequency::Always)
        );
        assert_eq!(
            find("cc_apk_configure").map(|m| m.distros),
            Some(&["alpine"][..])
        );
        assert!(find("cc_nope").is_none());
        assert!(find("bootcmd").is_none());
    }
}
