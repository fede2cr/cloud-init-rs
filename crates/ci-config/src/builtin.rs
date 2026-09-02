//! Port of `cloudinit/settings.py::CFG_BUILTIN`.
//!
//! These are the values cloud-init falls back to when nothing on disk says
//! otherwise. They must stay byte-for-byte equivalent to upstream, because the
//! datasource search order in particular decides what an instance becomes.

use crate::Object;

/// `settings.CLOUD_CONFIG`.
pub const CLOUD_CONFIG: &str = "/etc/cloud/cloud.cfg";
/// `settings.CLEAN_RUNPARTS_DIR`.
pub const CLEAN_RUNPARTS_DIR: &str = "/etc/cloud/clean.d";
/// `settings.DEFAULT_RUN_DIR`.
pub const DEFAULT_RUN_DIR: &str = "/run/cloud-init";
/// `settings.CFG_ENV_NAME`.
pub const CFG_ENV_NAME: &str = "CLOUD_CFG";
/// Runtime config written by the generator/datasources.
pub const RUN_CLOUD_CONFIG: &str = "/run/cloud-init/cloud.cfg";
/// `settings.HOTPLUG_ENABLED_FILE`.
pub const HOTPLUG_ENABLED_FILE: &str = "/var/lib/cloud/hotplug.enabled";

/// Valid module frequencies (`settings.FREQUENCIES`).
pub const PER_INSTANCE: &str = "once-per-instance";
pub const PER_ALWAYS: &str = "always";
pub const PER_ONCE: &str = "once";

/// `settings.CFG_BUILTIN`.
pub fn cfg_builtin() -> Object {
    let value = serde_json::json!({
        "datasource_list": [
            "NoCloud",
            "ConfigDrive",
            "LXD",
            "OpenNebula",
            "DigitalOcean",
            "Azure",
            "AltCloud",
            "VMware",
            "OVF",
            "MAAS",
            "GCE",
            "OpenStack",
            "AliYun",
            "Vultr",
            "Ec2",
            "CloudSigma",
            "CloudStack",
            "SmartOS",
            "Bigstep",
            "Scaleway",
            "Hetzner",
            "IBMCloud",
            "Oracle",
            "Exoscale",
            "RbxCloud",
            "UpCloud",
            "NWCS",
            "Akamai",
            "WSL",
            "CloudCIX",
            // At the end to act as a 'catch' when none of the above work.
            "None"
        ],
        "def_log_file": "/var/log/cloud-init.log",
        "log_cfgs": [],
        "syslog_fix_perms": ["syslog:adm", "root:adm", "root:wheel", "root:root"],
        "system_info": {
            "paths": {
                "cloud_dir": "/var/lib/cloud",
                "docs_dir": "/usr/share/doc/cloud-init/",
                "templates_dir": "/etc/cloud/templates/"
            },
            "distro": "ubuntu",
            "network": {"renderers": null}
        },
        "vendor_data": {"enabled": true, "prefix": []},
        "vendor_data2": {"enabled": true, "prefix": []}
    });
    match value {
        serde_json::Value::Object(map) => map,
        _ => Object::new(),
    }
}
