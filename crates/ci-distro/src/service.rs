//! `Distro.manage_service`: which argv performs an action on a service.
//!
//! The command is built here and run by the caller, because the modules that
//! need it drive a machine behind a trait so that the differential can compare
//! what they asked for without a machine to ask.

use crate::Distro;

/// `Distro.manage_service`'s `cmds` table, in upstream's order.
///
/// The two halves are the systemd one and the `service`-style one; which is
/// used depends on [`systemd`](fn@systemd) and on whether `init_cmd` already
/// names `systemctl`.
const SYSTEMD: [(&str, &[&str]); 8] = [
    ("stop", &["stop"]),
    ("start", &["start"]),
    ("enable", &["enable"]),
    ("disable", &["disable"]),
    ("restart", &["restart"]),
    ("reload", &["reload-or-restart"]),
    ("try-reload", &["try-reload-or-restart"]),
    ("status", &["status"]),
];

/// The same actions for an init system driven by `service <name> <verb>`,
/// where the service name comes first.
const SYSV: [(&str, &str); 8] = [
    ("stop", "stop"),
    ("start", "start"),
    ("enable", "start"),
    ("disable", "stop"),
    ("restart", "restart"),
    ("reload", "restart"),
    ("try-reload", "restart"),
    ("status", "status"),
];

/// `Distro.manage_service(action, service, *extra_args)`, probing the live
/// host for its init system.
///
/// # Errors
/// The action name, which is the `KeyError` upstream raises for one that is
/// not in the table.
pub fn command(
    distro: &Distro,
    action: &str,
    service: &str,
    extra: &[&str],
) -> Result<Vec<String>, String> {
    command_with(
        distro,
        ci_core::status::uses_systemd(),
        action,
        service,
        extra,
    )
}

/// [`command`] for a caller that already knows whether the host runs systemd,
/// so a test or a differential can build the argv without probing.
///
/// # Errors
/// As [`command`].
pub fn command_with(
    distro: &Distro,
    systemd: bool,
    action: &str,
    service: &str,
    extra: &[&str],
) -> Result<Vec<String>, String> {
    let mut argv: Vec<String>;
    if systemd || distro.init_cmd.contains(&"systemctl") {
        // `init_cmd = ["systemctl"]` -- the distro's own prefix is discarded.
        let verb = lookup(&SYSTEMD, action)?;
        argv = vec!["systemctl".to_owned()];
        argv.extend(verb.iter().map(|word| (*word).to_owned()));
        argv.push(service.to_owned());
    } else {
        let verb = lookup_sysv(action)?;
        argv = distro
            .init_cmd
            .iter()
            .map(|word| (*word).to_owned())
            .collect();
        argv.push(service.to_owned());
        argv.push(verb.to_owned());
    }
    argv.extend(extra.iter().map(|word| (*word).to_owned()));
    Ok(argv)
}

fn lookup<'a>(
    table: &'a [(&str, &'static [&'static str])],
    action: &str,
) -> Result<&'a &'static [&'static str], String> {
    table
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, verb)| verb)
        .ok_or_else(|| action.to_owned())
}

fn lookup_sysv(action: &str) -> Result<&'static str, String> {
    SYSV.iter()
        .find(|(name, _)| *name == action)
        .map(|(_, verb)| *verb)
        .ok_or_else(|| action.to_owned())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn systemd_discards_the_distros_own_prefix() {
        let distro = crate::fetch("ubuntu").unwrap();
        assert_eq!(
            command_with(distro, true, "try-reload", "rsyslog", &[]).unwrap(),
            ["systemctl", "try-reload-or-restart", "rsyslog"]
        );
    }

    #[test]
    fn a_service_style_init_puts_the_name_before_the_verb() {
        let distro = crate::fetch("freebsd").unwrap();
        assert_eq!(
            command_with(distro, false, "try-reload", "rsyslogd", &[]).unwrap(),
            ["service", "rsyslogd", "restart"]
        );
    }

    #[test]
    fn an_init_cmd_naming_systemctl_wins_without_probing() {
        let distro = crate::fetch("photon").unwrap();
        assert!(distro.init_cmd.contains(&"systemctl"));
        assert_eq!(
            command_with(distro, false, "reload", "sshd", &[]).unwrap(),
            ["systemctl", "reload-or-restart", "sshd"]
        );
    }

    #[test]
    fn an_action_that_is_not_in_the_table_is_the_key_error() {
        let distro = crate::fetch("ubuntu").unwrap();
        assert_eq!(
            command_with(distro, true, "onestart", "syslogd", &[]),
            Err("onestart".to_owned())
        );
    }
}
