//! Dump one distro's facts as JSON, for differential testing.
//!
//! Usage: `dump-distro <name>` or `dump-distro --names`.
//!
//! The table this reads is generated from the packaged cloud-init, so a
//! disagreement here means either the generator drifted from what `fetch`
//! actually returns, or upstream changed underneath the checked-in copy. Both
//! are worth failing over.

fn writer_name(distro: &ci_distro::Distro) -> &'static str {
    match distro.hostname_writer {
        ci_distro::HostnameWriter::Aosc => "Aosc",
        ci_distro::HostnameWriter::Bsd => "Bsd",
        ci_distro::HostnameWriter::ConfFile => "ConfFile",
        ci_distro::HostnameWriter::Gentoo => "Gentoo",
        ci_distro::HostnameWriter::OpenBsd => "OpenBsd",
        ci_distro::HostnameWriter::OpenSuse => "OpenSuse",
        ci_distro::HostnameWriter::Photon => "Photon",
        ci_distro::HostnameWriter::Rhel => "Rhel",
    }
}

fn reader_name(distro: &ci_distro::Distro) -> &'static str {
    match distro.hostname_reader {
        ci_distro::HostnameReader::Aosc => "Aosc",
        ci_distro::HostnameReader::Bsd => "Bsd",
        ci_distro::HostnameReader::ConfFile => "ConfFile",
        ci_distro::HostnameReader::OpenBsd => "OpenBsd",
        ci_distro::HostnameReader::OpenSuse => "OpenSuse",
        ci_distro::HostnameReader::Photon => "Photon",
        ci_distro::HostnameReader::Rhel => "Rhel",
    }
}

fn system_reader_name(distro: &ci_distro::Distro) -> &'static str {
    match distro.system_hostname_reader {
        ci_distro::SystemHostnameReader::ConfFile => "ConfFile",
        ci_distro::SystemHostnameReader::Systemd => "Systemd",
        ci_distro::SystemHostnameReader::SystemdOrConf => "SystemdOrConf",
    }
}

fn timezone_writer_name(distro: &ci_distro::Distro) -> &'static str {
    match distro.timezone_writer {
        ci_distro::TimezoneWriter::EtcTimezone => "EtcTimezone",
        ci_distro::TimezoneWriter::Aosc => "Aosc",
        ci_distro::TimezoneWriter::Rhel => "Rhel",
        ci_distro::TimezoneWriter::OpenSuse => "OpenSuse",
        ci_distro::TimezoneWriter::Bsd => "Bsd",
    }
}

fn locale_writer_name(distro: &ci_distro::Distro) -> &'static str {
    match distro.locale_writer {
        ci_distro::LocaleWriter::Alpine => "Alpine",
        ci_distro::LocaleWriter::Aosc => "Aosc",
        ci_distro::LocaleWriter::Arch => "Arch",
        ci_distro::LocaleWriter::Debian => "Debian",
        ci_distro::LocaleWriter::FreeBsd => "FreeBsd",
        ci_distro::LocaleWriter::Gentoo => "Gentoo",
        ci_distro::LocaleWriter::NetBsd => "NetBsd",
        ci_distro::LocaleWriter::OpenSuse => "OpenSuse",
        ci_distro::LocaleWriter::Photon => "Photon",
        ci_distro::LocaleWriter::RaspberryPiOs => "RaspberryPiOs",
        ci_distro::LocaleWriter::Rhel => "Rhel",
    }
}

fn locale_reader_name(distro: &ci_distro::Distro) -> &'static str {
    match distro.locale_reader {
        ci_distro::LocaleReader::Alpine => "Alpine",
        ci_distro::LocaleReader::Debian => "Debian",
        ci_distro::LocaleReader::Rhel => "Rhel",
        ci_distro::LocaleReader::Unsupported => "Unsupported",
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(first) = args.first() else {
        eprintln!("usage: dump-distro <name>|--names");
        std::process::exit(2);
    };

    if first == "--names" {
        let names: Vec<ci_config::Value> = ci_distro::NAMES
            .iter()
            .map(|name| ci_config::Value::String((*name).to_owned()))
            .collect();
        println!(
            "{}",
            ci_core::jsonfmt::dumps_indent(&ci_config::Value::Array(names), 1)
        );
        return;
    }

    let Some(distro) = ci_distro::fetch(first) else {
        // `distros.fetch` raises for a name in OSFAMILIES with no module, and
        // for one that is not there at all. The two look the same here.
        println!("null");
        return;
    };

    println!("{}", ci_core::jsonfmt::dumps_indent(&facts(distro), 1));
}

fn facts(distro: &ci_distro::Distro) -> ci_config::Value {
    let mut out = ci_config::Object::new();
    let mut put = |key: &str, value: ci_config::Value| {
        out.insert(key.to_owned(), value);
    };
    let s = |text: &str| ci_config::Value::String(text.to_owned());
    let opt = |text: Option<&str>| {
        text.map_or(ci_config::Value::Null, |t| {
            ci_config::Value::String(t.to_owned())
        })
    };
    let list = |items: &[&str]| {
        ci_config::Value::Array(
            items
                .iter()
                .map(|i| ci_config::Value::String((*i).to_owned()))
                .collect(),
        )
    };

    put("ci_sudoers_fn", s(distro.ci_sudoers_fn));
    put("default_owner", s(distro.default_owner));
    put(
        "dhclient_lease_directory",
        opt(distro.dhclient_lease_directory),
    );
    put(
        "dhclient_lease_file_regex",
        opt(distro.dhclient_lease_file_regex),
    );
    put("doas_fn", s(ci_distro::DOAS_FN));
    put("family_key", opt(ci_distro::osfamily_of(distro.name)));
    put("hostname_conf_fn", s(distro.hostname_conf_fn));
    put(
        "systemd_hostname_conf_fn",
        opt(distro.systemd_hostname_conf_fn),
    );
    put("localhost_ip", s(distro.localhost_ip));
    put("hostname_reader", s(reader_name(distro)));
    put("system_hostname_reader", s(system_reader_name(distro)));
    put("hostname_writer", s(writer_name(distro)));
    put("hosts_fn", s(ci_distro::HOSTS_FN));
    put("init_cmd", list(distro.init_cmd));
    put("osfamily", s(distro.osfamily));
    put("pip_package_name", s(distro.pip_package_name));
    put("prefer_fqdn", ci_config::Value::Bool(distro.prefer_fqdn));
    put(
        "renderer_configs",
        ci_config::Value::Object(distro.all_renderer_configs()),
    );
    put("resolve_conf_fn", s(distro.resolve_conf_fn));
    put(
        "shadow_empty_locked_passwd_patterns",
        list(distro.shadow_empty_locked_passwd_patterns),
    );
    put("shadow_extrausers_fn", s(ci_distro::SHADOW_EXTRAUSERS_FN));
    put("shadow_fn", s(distro.shadow_fn));
    let mut shutdown = ci_config::Object::new();
    shutdown.insert("halt".to_owned(), s(distro.halt_option));
    shutdown.insert("poweroff".to_owned(), s(distro.poweroff_option));
    shutdown.insert("reboot".to_owned(), s(distro.reboot_option));
    put("shutdown_options_map", ci_config::Value::Object(shutdown));
    put("timezone_writer", s(timezone_writer_name(distro)));
    put("tz_local_fn", opt(distro.tz_local_fn));
    put("clock_conf_fn", opt(distro.clock_conf_fn));
    put("locale_writer", s(locale_writer_name(distro)));
    put("locale_reader", s(locale_reader_name(distro)));
    put("locale_conf_fn", opt(distro.locale_conf_fn));
    put("systemd_locale_conf_fn", opt(distro.systemd_locale_conf_fn));
    put("default_locale", opt(distro.default_locale));
    put("tz_zone_dir", s(ci_distro::TZ_ZONE_DIR));
    put("usr_lib_exec", s(distro.usr_lib_exec));
    put(
        "waits_for_network",
        ci_config::Value::Bool(distro.waits_for_network),
    );
    put(
        "package_managers",
        ci_config::Value::Array(
            distro
                .package_managers
                .iter()
                .map(|manager| s(manager.name()))
                .collect(),
        ),
    );

    ci_core::jsonfmt::sort_keys(&ci_config::Value::Object(out))
}
