//! Port of `cloudinit/distros`: the per-distribution facts a boot depends on.
//!
//! Upstream's `Distro` is one class per distribution, and most of what those
//! classes carry is *data* — where the hostname file lives, which network
//! renderer configuration to hand the renderer, whether the FQDN or the short
//! name is written. That half is a table here, generated from the packaged
//! Python classes rather than transcribed from their source, because the
//! inheritance graph is deep enough (`rocky` -> `rhel` -> `Distro`, with
//! `centos` and `almalinux` alongside) that reading it off by hand is how
//! mistakes get in.
//!
//! The behavioural half — installing packages, adding users, applying network
//! config — is not here. It needs `subp` against a live system and it arrives
//! with the modules and activators that call it. What *is* here is everything
//! those callers need to look up before they act, plus the few decisions that
//! are pure functions of that data.

mod table;

pub mod create;
pub mod hostname;
pub mod hosts;
pub mod locale;
pub mod mirrors;
pub mod packages;
pub mod probe;
pub mod service;
pub mod timezone;
pub mod ug;
pub mod user;

use ci_config::{option, Object, Value};

pub use table::{DISTROS, NAMES};

/// Which class in the MRO provides `_write_hostname`.
///
/// The variants are named after those classes, not after families: the two do
/// not line up (`azurelinux` names itself in `osfamily` but inherits `rhel`'s
/// writer). Resolved per distro in the generated table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostnameWriter {
    /// `alpine`, `arch`, `debian`: the `HostnameConf` round-trip.
    ConfFile,
    /// `aosc`: `hostnamectl`, and the file too for `previous-hostname`.
    Aosc,
    /// `gentoo`: [`ConfFile`](HostnameWriter::ConfFile), but `OpenRC` wants
    /// `hostname="..."`.
    Gentoo,
    /// `opensuse`: `hostnamectl` under systemd, the file otherwise.
    OpenSuse,
    /// `photon`: `hostnamectl`, warning rather than failing.
    Photon,
    /// `rhel`: `hostnamectl` under systemd, `/etc/sysconfig/network`
    /// otherwise.
    Rhel,
    /// `freebsd`, `netbsd`: `/etc/rc.conf`.
    Bsd,
    /// `openbsd`: `/etc/myname`.
    OpenBsd,
}

/// Which class in the MRO provides `Distro._read_hostname`.
///
/// Nearly the same partition as [`HostnameWriter`], but not quite: `gentoo`
/// writes `hostname="..."` and reads a plain name back, so it shares
/// `debian`'s reader while keeping its own writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostnameReader {
    /// `alpine`, `arch`, `debian`, `gentoo`: the first name in the file.
    ConfFile,
    /// `aosc`: the file for `previous-hostname`, `hostname` otherwise.
    Aosc,
    /// `opensuse`: like `rhel` under systemd, the file otherwise.
    OpenSuse,
    /// `photon`: the file for `previous-hostname`, `hostname -f` otherwise.
    Photon,
    /// `rhel`: the file for `previous-hostname`, `hostname` under systemd,
    /// `/etc/sysconfig/network` otherwise.
    Rhel,
    /// `freebsd`, `netbsd`: `/etc/rc.conf`'s `hostname=`.
    Bsd,
    /// `openbsd`: `/etc/myname`, whatever file it was asked for.
    OpenBsd,
}

/// Which file `Distro._read_system_hostname` reads the running name out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemHostnameReader {
    /// [`Distro::hostname_conf_fn`], always.
    ConfFile,
    /// [`Distro::systemd_hostname_conf_fn`] under systemd, else
    /// [`Distro::hostname_conf_fn`].
    SystemdOrConf,
    /// [`Distro::systemd_hostname_conf_fn`], always — `photon` does not check.
    Systemd,
}

/// Which class in the MRO provides `set_timezone`.
///
/// Every variant starts the same way, with `_find_tz_file` refusing a name
/// that has no file under [`TZ_ZONE_DIR`]; they differ in what they do with
/// the file afterwards. See [`timezone`] for the bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimezoneWriter {
    /// `alpine`, `arch`, `debian`, `gentoo`, `photon`:
    /// `distros.set_etc_timezone`, which writes `/etc/timezone` as well as
    /// pointing `/etc/localtime` at the zone file.
    EtcTimezone,
    /// `aosc`: the `/etc/localtime` symlink only, and unconditionally — no
    /// `/etc/timezone`, and no copy fallback for a real file.
    Aosc,
    /// `rhel`: the symlink under systemd, `/etc/sysconfig/clock`'s `ZONE`
    /// otherwise.
    Rhel,
    /// `opensuse`: the same shape as [`Rhel`](TimezoneWriter::Rhel), with
    /// `TIMEZONE` as the sysconfig key.
    OpenSuse,
    /// `freebsd`, `netbsd`, `openbsd`.
    Bsd,
}

/// Which class in the MRO provides `apply_locale`.
///
/// One variant per class, unlike the hostname enums: no two of the eleven
/// bodies agree, because each distro puts the locale in a different file and
/// regenerates it with a different command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocaleWriter {
    /// `alpine`: a `/etc/profile.d` shell snippet, plus `locale-gen` only if
    /// the musl locale package happens to be installed.
    Alpine,
    /// `aosc`: `/etc/locale.conf` through `update_sysconfig_file`.
    Aosc,
    /// `arch`: `locale-gen` then `localectl`.
    Arch,
    /// `debian`, `ubuntu`: `locale-gen` and `update-locale`, both skipped when
    /// the system is already there. The one this port implements.
    Debian,
    /// `freebsd`: `/etc/login.conf` and `cap_mkdb`.
    FreeBsd,
    /// `gentoo`: `locale-gen` against `/etc/locale.gen`.
    Gentoo,
    /// `netbsd`, `openbsd`: a no-op that logs.
    NetBsd,
    /// `opensuse`: `/etc/sysconfig/language`.
    OpenSuse,
    /// `photon`: `/etc/locale.conf` and `localectl`.
    Photon,
    /// `raspberry-pi-os`: `debian`'s, plus a `debconf` selection.
    RaspberryPiOs,
    /// `rhel`: `/etc/sysconfig/i18n` or `/etc/locale.conf`.
    Rhel,
}

/// Which class in the MRO provides `get_locale`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocaleReader {
    /// `alpine`: the `/etc/profile.d` snippet.
    Alpine,
    /// `debian`, `raspberry-pi-os`, `ubuntu`: `/etc/default/locale`, falling
    /// back to [`Distro::default_locale`].
    Debian,
    /// `rhel` and its fifteen relatives.
    Rhel,
    /// The abstract base: `raise NotImplementedError()`.
    ///
    /// Seventeen distros land here, which means `cc_locale` on them has no
    /// system default to fall back to and depends on the config naming one.
    Unsupported,
}

/// `distros.ALL_DISTROS`, the wildcard a module's `meta` uses to say it runs
/// everywhere.
pub const ALL_DISTROS: &str = "all";

/// Attributes every distro in 26.1 inherits unchanged. Kept out of the table
/// so a real override cannot hide in 36 identical rows; the differential dump
/// reports them per distro, so one appearing would show up as drift.
pub const HOSTS_FN: &str = "/etc/hosts";
/// `Distro.doas_fn`.
pub const DOAS_FN: &str = "/etc/doas.conf";
/// `Distro.shadow_extrausers_fn`.
pub const SHADOW_EXTRAUSERS_FN: &str = "/var/lib/extrausers/shadow";
/// `Distro.tz_zone_dir`.
pub const TZ_ZONE_DIR: &str = "/usr/share/zoneinfo";

/// `distros.PREFERRED_NTP_CLIENTS`.
pub const PREFERRED_NTP_CLIENTS: &[&str] =
    &["chrony", "systemd-timesyncd", "ntp", "ntpdate"];

/// One distribution's facts: the class attributes of `distros.fetch(name)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Distro {
    /// The name as spelled in `OSFAMILIES`, which is what `-D`, `system_info.
    /// distro` and `distros.fetch` all take.
    pub name: &'static str,
    /// `Distro.osfamily`, as `__init__` assigns it.
    ///
    /// Not the same thing as the `OSFAMILIES` key — see [`osfamily_of`].
    /// `azurelinux`, `mariner` and `photon` are all listed under `redhat` but
    /// each names itself, which is why `cc_ssh`'s `osfamily == "redhat"` does
    /// not fire on them. The three BSDs assign `platform.system().lower()`,
    /// which on the only kernel this port runs on is `linux`.
    pub osfamily: &'static str,
    /// `Distro.default_owner`: the `owner` a `write_files` entry gets when it
    /// does not name one.
    pub default_owner: &'static str,
    /// `Distro.hostname_conf_fn`.
    pub hostname_conf_fn: &'static str,
    /// `Distro.systemd_hostname_conf_fn`, where the class defines one.
    pub systemd_hostname_conf_fn: Option<&'static str>,
    /// Which class in the MRO provides `_write_hostname`. See
    /// [`hostname`] for what each one does.
    pub hostname_writer: HostnameWriter,
    /// Which class in the MRO provides `_read_hostname`.
    pub hostname_reader: HostnameReader,
    /// Which class in the MRO provides `_read_system_hostname`.
    pub system_hostname_reader: SystemHostnameReader,
    /// Which class in the MRO provides `set_timezone`.
    pub timezone_writer: TimezoneWriter,
    /// `Distro.tz_local_fn`, where the class defines one.
    ///
    /// Absent for the majority, whose `set_timezone` goes through
    /// `distros.set_etc_timezone` — that function takes the path as an
    /// argument default rather than reading it off the object, so those
    /// classes never needed the attribute.
    pub tz_local_fn: Option<&'static str>,
    /// `Distro.clock_conf_fn`: the sysconfig file the two non-systemd
    /// branches write.
    pub clock_conf_fn: Option<&'static str>,
    /// Which class in the MRO provides `apply_locale`.
    pub locale_writer: LocaleWriter,
    /// Which class in the MRO provides `get_locale`.
    pub locale_reader: LocaleReader,
    /// `Distro.locale_conf_fn`, where the class defines one.
    pub locale_conf_fn: Option<&'static str>,
    /// `Distro.systemd_locale_conf_fn`, where the class defines one.
    pub systemd_locale_conf_fn: Option<&'static str>,
    /// `Distro.default_locale`: what `get_locale` answers when the system has
    /// not been told otherwise.
    pub default_locale: Option<&'static str>,
    /// `Distro._get_localhost_ip()`: the address `update_etc_hosts` manages.
    ///
    /// `127.0.1.1` on Debian and its family, which is what lets `hostname -f`
    /// answer with the FQDN while `127.0.0.1` stays `localhost`.
    pub localhost_ip: &'static str,
    /// `Distro.prefer_fqdn`: whether [`select_hostname`] writes the FQDN.
    pub prefer_fqdn: bool,
    /// `Distro.usr_lib_exec`.
    pub usr_lib_exec: &'static str,
    /// `Distro.shadow_fn`.
    pub shadow_fn: &'static str,
    /// `Distro.ci_sudoers_fn`.
    pub ci_sudoers_fn: &'static str,
    /// `Distro.resolve_conf_fn`.
    pub resolve_conf_fn: &'static str,
    /// `Distro.pip_package_name`.
    pub pip_package_name: &'static str,
    /// `Distro.init_cmd`, the argv prefix for service management.
    pub init_cmd: &'static [&'static str],
    /// `Distro.shutdown_options_map["halt"]`.
    pub halt_option: &'static str,
    /// `Distro.shutdown_options_map["poweroff"]`.
    pub poweroff_option: &'static str,
    /// `Distro.shutdown_options_map["reboot"]`.
    pub reboot_option: &'static str,
    /// `distro.get_option("ssh_svcname", "ssh")`.
    ///
    /// Not a class attribute: each distro's `__init__` writes it into the
    /// `system_info.distro` dict it is handed, so the default belongs to the
    /// reader rather than the table. openSUSE's value depends on whether the
    /// *generating* host used systemd, exactly as `init_cmd` already does.
    pub ssh_svcname: &'static str,
    /// `Distro.dhclient_lease_directory`.
    pub dhclient_lease_directory: Option<&'static str>,
    /// `Distro.dhclient_lease_file_regex`.
    pub dhclient_lease_file_regex: Option<&'static str>,
    /// `Distro.shadow_empty_locked_passwd_patterns`, with `{username}` still
    /// in them.
    pub shadow_empty_locked_passwd_patterns: &'static [&'static str],
    /// `Distro.renderer_configs`, as canonical JSON with sorted keys.
    ///
    /// A blob rather than a typed struct because it *is* an untyped dict
    /// upstream: each renderer reads the keys it knows and ignores the rest,
    /// and the keys differ per renderer. [`Distro::renderer_config`] parses
    /// one renderer's share out of it.
    pub renderer_configs: &'static str,
    /// Whether `Distro.wait_for_network` does anything.
    ///
    /// The base method is a documented no-op: most distros order cloud-init's
    /// network service after network-online and so have nothing left to wait
    /// for. Only `ubuntu` overrides it, and only because Ubuntu starts that
    /// service straight after the local one. Read off the MRO like
    /// [`Distro::hostname_writer`], for the same reason.
    pub waits_for_network: bool,
    /// `Distro.package_managers`, in the order `install_packages` drives them.
    ///
    /// Empty for all but the debian family: every other distro overrides
    /// `install_packages` outright and shells out to its own tool instead, so
    /// an empty list here means "not this mechanism", not "no packages".
    pub package_managers: &'static [packages::Manager],
}

impl Distro {
    /// `distro.renderer_configs.get(name)`, or an empty map.
    ///
    /// Parsed on each call: the table holds text so that it can be `static`,
    /// and the callers are one-per-boot.
    #[must_use]
    pub fn renderer_config(&self, renderer: &str) -> Object {
        match self.all_renderer_configs().get(renderer) {
            Some(Value::Object(map)) => map.clone(),
            _ => Object::new(),
        }
    }

    /// Whether this distro has a `renderer_configs` entry for `renderer`.
    ///
    /// Upstream's `net-convert` reads `config["netplan_path"][1:]` off the
    /// `{}` default when there is none, so "absent" is the difference between
    /// working and a `KeyError`.
    #[must_use]
    pub fn has_renderer_config(&self, renderer: &str) -> bool {
        self.all_renderer_configs().contains_key(renderer)
    }

    /// `Distro.renderer_configs` as a whole.
    #[must_use]
    pub fn all_renderer_configs(&self) -> Object {
        match serde_json::from_str(self.renderer_configs) {
            Ok(Value::Object(all)) => all,
            _ => Object::new(),
        }
    }

    /// `Distro._select_hostname`: which of the two names reaches
    /// `hostname_conf_fn`.
    ///
    /// `cfg` is the system config, whose `prefer_fqdn_over_hostname` overrides
    /// the distro's own default in either direction.
    #[must_use]
    pub fn select_hostname<'a>(
        &self,
        cfg: &Object,
        hostname: Option<&'a str>,
        fqdn: Option<&'a str>,
    ) -> Option<&'a str> {
        let prefer =
            option::get_bool(cfg, "prefer_fqdn_over_hostname", self.prefer_fqdn);
        if prefer && fqdn.is_some() {
            fqdn
        } else if hostname.is_some() {
            hostname
        } else {
            fqdn
        }
    }

    /// `Distro.shutdown_command`.
    ///
    /// `delay` is `"now"` or a whole number of minutes; anything else is the
    /// `TypeError` upstream raises, reported here as the message it carries.
    pub fn shutdown_command(
        &self,
        mode: &str,
        delay: &str,
        message: &str,
    ) -> Result<Vec<String>, String> {
        let flag = match mode {
            "halt" => self.halt_option,
            "poweroff" => self.poweroff_option,
            "reboot" => self.reboot_option,
            other => return Err(format!("'{other}'")),
        };
        let delay = if delay == "now" {
            "now".to_owned()
        } else {
            let minutes: i64 = delay.trim().parse().map_err(|_| {
                format!(
                    "power_state[delay] must be 'now' or '+m' (minutes). \
                     found '{delay}'."
                )
            })?;
            format!("+{minutes}")
        };
        let mut argv = vec!["shutdown".to_owned(), flag.to_owned(), delay];
        if !message.is_empty() {
            argv.push(message.to_owned());
        }
        Ok(argv)
    }
}

/// `distros.fetch`: the class behind a `system_info.distro` name.
///
/// `None` covers both of upstream's failures — a name that is not in
/// `OSFAMILIES` at all, and one that is but has no module behind it, which in
/// 26.1 is `dragonfly` alone.
#[must_use]
pub fn fetch(name: &str) -> Option<&'static Distro> {
    DISTROS.iter().find(|distro| distro.name == name)
}

/// `Distro.expand_osfamily`: replace each family name with its members.
///
/// An unknown family is upstream's `ValueError`, reported here as the offending
/// name, because the caller is a module's `meta` and a typo there would
/// otherwise silently narrow where the module runs.
pub fn expand_osfamily(families: &[&str]) -> Result<Vec<&'static str>, String> {
    let mut out = Vec::new();
    for family in families {
        let before = out.len();
        for name in NAMES {
            if osfamily_of(name) == Some(*family) {
                out.push(*name);
            }
        }
        if out.len() == before {
            return Err((*family).to_owned());
        }
    }
    Ok(out)
}

/// The `OSFAMILIES` key a name appears under, including for the names
/// [`fetch`] cannot resolve.
#[must_use]
pub fn osfamily_of(name: &str) -> Option<&'static str> {
    table::FAMILY_OF
        .iter()
        .find(|(member, _)| *member == name)
        .map(|(_, family)| *family)
}

/// `distros.uses_systemd`.
///
/// This is upstream's only copy of the check, and four callers import it from
/// `cloudinit.distros`. Three of ours are below `ci-distro` in the stack, so
/// the body lives in `ci-core` and this is the name a reader of
/// `cloudinit/distros/__init__.py` would come looking for.
pub use ci_core::status::uses_systemd;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn every_name_but_dragonfly_resolves() {
        for name in NAMES {
            assert_eq!(fetch(name).is_some(), *name != "dragonfly", "{name}");
        }
        assert!(fetch("nosuchdistro").is_none());
    }

    #[test]
    fn the_table_is_in_osfamilies_order() {
        let resolvable: Vec<&str> = NAMES
            .iter()
            .copied()
            .filter(|n| *n != "dragonfly")
            .collect();
        let table: Vec<&str> = DISTROS.iter().map(|d| d.name).collect();
        assert_eq!(table, resolvable);
    }

    #[test]
    fn the_redhat_family_inherits_rhels_libexec() {
        for name in ["centos", "rocky", "almalinux", "rhel"] {
            let distro = fetch(name).unwrap();
            assert_eq!(osfamily_of(name), Some("redhat"), "{name}");
            assert_eq!(distro.usr_lib_exec, "/usr/libexec", "{name}");
        }
        // …but not every member of it: `photon` and `azurelinux` are in the
        // family without descending from `rhel`.
        assert_eq!(fetch("photon").unwrap().usr_lib_exec, "/usr/lib");
    }

    #[test]
    fn three_distros_disown_the_family_that_lists_them() {
        for name in ["azurelinux", "mariner", "photon"] {
            assert_eq!(osfamily_of(name), Some("redhat"), "{name}");
            assert_eq!(fetch(name).unwrap().osfamily, name, "{name}");
        }
        // Everyone else agrees with the table they are listed in.
        for distro in DISTROS {
            if ["azurelinux", "mariner", "photon"].contains(&distro.name) {
                continue;
            }
            let expected = if distro.osfamily == "linux" {
                // The BSDs read `platform.system()`, not a constant.
                continue;
            } else {
                osfamily_of(distro.name)
            };
            assert_eq!(Some(distro.osfamily), expected, "{}", distro.name);
        }
    }

    #[test]
    fn the_bsds_report_the_kernel_they_are_running_on() {
        for name in ["freebsd", "netbsd", "openbsd"] {
            assert_eq!(fetch(name).unwrap().osfamily, "linux", "{name}");
        }
    }

    #[test]
    fn only_some_distros_have_a_netplan_config() {
        let with: Vec<&str> = DISTROS
            .iter()
            .filter(|d| d.has_renderer_config("netplan"))
            .map(|d| d.name)
            .collect();
        assert_eq!(
            with,
            [
                "arch",
                "debian",
                "ubuntu",
                "raspberry-pi-os",
                "azurelinux",
                "mariner"
            ]
        );
    }

    #[test]
    fn azurelinux_swaps_its_whole_renderer_config_in_init() {
        // The class attribute it inherits from `rhel` says `sysconfig`; the
        // object a boot builds says netplan and networkd. Reading the class
        // would make `-O netplan` fail for a distro on which it works.
        let azurelinux = fetch("azurelinux").unwrap();
        let configs = azurelinux.all_renderer_configs();
        let keys: Vec<&String> = configs.keys().collect();
        assert_eq!(keys, ["netplan", "networkd"]);
    }

    #[test]
    fn mariners_postcmds_is_the_string_true() {
        // Upstream bug B62: every other distro writes the bool. Both are
        // truthy, so nothing downstream notices, but the value is carried
        // through verbatim and would differ in any dump of it.
        let mariner = fetch("mariner").unwrap();
        assert_eq!(
            mariner.renderer_config("netplan").get("postcmds"),
            Some(&Value::String("True".to_owned()))
        );
        assert_eq!(
            fetch("ubuntu")
                .unwrap()
                .renderer_config("netplan")
                .get("postcmds"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn a_renderer_config_parses_back_to_its_keys() {
        let ubuntu = fetch("ubuntu").unwrap();
        let netplan = ubuntu.renderer_config("netplan");
        assert_eq!(
            netplan.get("netplan_path").and_then(Value::as_str),
            Some("/etc/netplan/50-cloud-init.yaml")
        );
        assert!(ubuntu.renderer_config("sysconfig").is_empty());
    }

    #[test]
    fn prefer_fqdn_picks_the_fqdn_only_when_there_is_one() {
        let empty = Object::new();
        let rhel = fetch("rhel").unwrap();
        assert!(rhel.prefer_fqdn);
        assert_eq!(
            rhel.select_hostname(&empty, Some("short"), Some("f.q.dn")),
            Some("f.q.dn")
        );
        assert_eq!(
            rhel.select_hostname(&empty, Some("short"), None),
            Some("short")
        );
        let ubuntu = fetch("ubuntu").unwrap();
        assert!(!ubuntu.prefer_fqdn);
        assert_eq!(
            ubuntu.select_hostname(&empty, Some("short"), Some("f.q.dn")),
            Some("short")
        );
        assert_eq!(
            ubuntu.select_hostname(&empty, None, Some("f.q.dn")),
            Some("f.q.dn")
        );
        assert_eq!(ubuntu.select_hostname(&empty, None, None), None);
    }

    #[test]
    fn the_config_overrides_the_distro_default_both_ways() {
        let mut cfg = Object::new();
        cfg.insert("prefer_fqdn_over_hostname".to_owned(), Value::Bool(true));
        let ubuntu = fetch("ubuntu").unwrap();
        assert_eq!(
            ubuntu.select_hostname(&cfg, Some("short"), Some("f.q.dn")),
            Some("f.q.dn")
        );
        cfg.insert("prefer_fqdn_over_hostname".to_owned(), Value::Bool(false));
        let rhel = fetch("rhel").unwrap();
        assert_eq!(
            rhel.select_hostname(&cfg, Some("short"), Some("f.q.dn")),
            Some("short")
        );
    }

    #[test]
    fn expanding_a_family_lists_its_members() {
        assert_eq!(
            expand_osfamily(&["debian"]),
            Ok(vec!["debian", "ubuntu", "raspberry-pi-os"])
        );
        assert_eq!(expand_osfamily(&["ubuntu"]), Err("ubuntu".to_owned()));
        assert_eq!(
            expand_osfamily(&["netbsd", "openbsd"]),
            Ok(vec!["netbsd", "openbsd"])
        );
    }

    #[test]
    fn an_unfetchable_name_still_has_a_family() {
        assert_eq!(osfamily_of("dragonfly"), Some("freebsd"));
        assert_eq!(
            expand_osfamily(&["freebsd"]),
            Ok(vec!["freebsd", "dragonfly"])
        );
        assert!(fetch("dragonfly").is_none());
    }

    #[test]
    fn a_shutdown_message_is_only_appended_when_there_is_one() {
        let ubuntu = fetch("ubuntu").unwrap();
        assert_eq!(
            ubuntu.shutdown_command("poweroff", "5", "bye"),
            Ok(vec![
                "shutdown".to_owned(),
                "-P".to_owned(),
                "+5".to_owned(),
                "bye".to_owned()
            ])
        );
        assert_eq!(
            ubuntu.shutdown_command("reboot", "now", ""),
            Ok(vec![
                "shutdown".to_owned(),
                "-r".to_owned(),
                "now".to_owned()
            ])
        );
        assert!(ubuntu.shutdown_command("sleep", "now", "").is_err());
        assert!(ubuntu.shutdown_command("halt", "soon", "").is_err());
    }
}
