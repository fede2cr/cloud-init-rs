//! `cc_package_update_upgrade_install`: update, upgrade, and install packages.
//!
//! The module itself is short — it reads four booleans and a list, then makes
//! three calls into the distro object. Nearly all of the behaviour lives on
//! the other side of those calls, in `distros/__init__.py` and
//! `distros/package_management/`; see [`ci_distro::packages`] for that half.
//!
//! Like `cc_set_passwords`, this is split into a [`plan`] that decides and a
//! [`handle`] that acts. Everything the module does to a machine is a package
//! transaction or a reboot, neither of which a differential run can afford to
//! perform, so the decision is what gets compared.

use std::path::Path;

use ci_config::{Object, Value};
use ci_distro::packages::{self, AptConfig, InstallPlan, Manager, Step};
use ci_log::Logger;

use super::Args;

/// `REBOOT_FILES`. Checked in order; the first that exists wins.
pub const REBOOT_FILES: &[&str] = &["/var/run/reboot-required", "/run/reboot-needed"];

/// `REBOOT_CMD`.
pub const REBOOT_CMD: &[&str] = &["/sbin/reboot"];

/// `_multi_cfg_bool_get`: true if any of `keys` is a true-ish config value.
///
/// `get_cfg_option_bool` is `translate_bool`, which is two tests in sequence:
/// anything Python calls falsy is false, a real `bool` is itself, and
/// everything else is `str(val).lower().strip() in ("true", "1", "on",
/// "yes")`. So the *only* true values are `True`, a non-zero number, and one
/// of those four spellings — `["x"]` and `"maybe"` are both false.
fn multi_cfg_bool_get(cfg: &Object, keys: &[&str]) -> bool {
    keys.iter()
        .any(|key| ci_config::option::get_bool(cfg, key, false))
}

/// `util.get_cfg_option_list(cfg, "packages", [])`.
///
/// A missing key gives the default, an explicit `null` gives the empty list, a
/// list is taken as-is, and anything else is wrapped in a one-element list —
/// stringified first if it is not already a string.
fn option_list(cfg: &Object, key: &str) -> Vec<Value> {
    match cfg.get(key) {
        // A missing key and an explicit `null` land in the same place, by
        // different routes: `get_cfg_option` returns the default for the
        // first and `if not val` catches the second.
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(value @ Value::String(_)) => vec![value.clone()],
        Some(value) => vec![Value::String(ci_config::repr(value))],
    }
}

/// What the module decided to do, in the order upstream does it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Plan {
    /// `distro.update_package_sources()`.
    pub update: Vec<Step>,
    /// `distro.package_command("upgrade")`.
    pub upgrade: Vec<Step>,
    /// `distro.install_packages(pkglist)`, empty when `packages:` was.
    pub install: InstallPlan,
    /// The marker file that justifies a reboot, when one is called for.
    /// `None` covers all three ways out: no marker, no `*_reboot_if_required`,
    /// and nothing installed or upgraded to reboot *for*.
    pub reboot: Option<String>,
    /// Whether `distro.package_command` refused the `upgrade` command.
    ///
    /// Upstream's base `package_command` raises `NotImplementedError`; the
    /// debian one raises `RuntimeError` for anything but `upgrade`. Either
    /// way the module catches it and carries on, so it is a recorded outcome
    /// rather than a failure of the plan.
    pub upgrade_unsupported: bool,
}

/// The live-system answers [`plan`] needs, gathered before it decides.
#[derive(Debug, Default, Clone)]
pub struct State {
    /// Which package managers are installed, and what apt says exists.
    pub packages: packages::State,
    /// The first entry of [`REBOOT_FILES`] that is a regular file.
    pub reboot_marker: Option<String>,
}

impl State {
    /// Read the state from a filesystem root, consulting `PATH` for the
    /// package managers the distro registers.
    #[must_use]
    pub fn probe(root: &Path, managers: &[Manager]) -> Self {
        let reboot_marker = REBOOT_FILES
            .iter()
            .find(|marker| {
                let relative = marker.trim_start_matches('/');
                root.join(relative).is_file()
            })
            .map(|marker| (*marker).to_owned());
        let snap_available =
            managers.contains(&Manager::Snap) && ci_sys::subp::which("snap").is_some();
        Self {
            packages: packages::State {
                apt_available: managers.contains(&Manager::Apt)
                    && ci_sys::subp::which("apt-get").is_some(),
                snap_available,
                all_packages: None,
                snap_refresh_hold: snap_available.then(snap_refresh_hold).flatten(),
            },
            reboot_marker,
        }
    }
}

/// `snap get system -d`, reduced to `refresh.hold`.
///
/// Upstream catches only `ProcessExecutionError` here, so a command that
/// succeeds and prints something that is not JSON takes the whole upgrade
/// down with a `JSONDecodeError`. Nothing observed does that, and a plan has
/// nowhere to put such a failure, so an unreadable answer is simply no hold —
/// which is also what upstream would decide if the command had failed.
fn snap_refresh_hold() -> Option<String> {
    let output = ci_sys::subp::Subp::new(vec![
        "snap".to_owned(),
        "get".to_owned(),
        "system".to_owned(),
        "-d".to_owned(),
    ])
    .inherit_env()
    .check()
    .ok()?;
    let parsed =
        ci_core::jsonfmt::json_loads(&String::from_utf8_lossy(&output.stdout))?;
    parsed
        .get("refresh")?
        .get("hold")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// Decide what `handle` would run, without running any of it.
///
/// # Errors
///
/// The `ValueError` from `_validate_entry` when a `packages:` entry is
/// neither a string nor a two-element list. Upstream raises this out of
/// `install_packages`, where the module catches it and re-raises at the end;
/// the caller reproduces that, so it is surfaced here rather than swallowed.
pub fn plan(
    cfg: &Object,
    system_info: &Object,
    managers: &[Manager],
    state: &State,
    log: &mut Logger,
) -> Result<Plan, String> {
    let update = multi_cfg_bool_get(cfg, &["apt_update", "package_update"]);
    let upgrade = multi_cfg_bool_get(cfg, &["package_upgrade", "apt_upgrade"]);
    let reboot_if_required = multi_cfg_bool_get(
        cfg,
        &["apt_reboot_if_required", "package_reboot_if_required"],
    );
    let pkglist = option_list(cfg, "packages");

    let config = AptConfig::from_config(system_info, &mut |name| {
        ci_sys::subp::which(name).is_some()
    })?;

    let mut plan = Plan::default();

    if update || upgrade {
        plan.update = packages::plan_update_sources(
            managers,
            &config,
            &state.packages,
            false,
            log,
        );
    }

    if upgrade {
        // Only the debian family implements `package_command`; every other
        // distro inherits the base `NotImplementedError`.
        match packages::plan_package_command(
            managers,
            &config,
            &state.packages,
            "upgrade",
        ) {
            Ok(steps) if managers.contains(&Manager::Apt) => plan.upgrade = steps,
            _ => plan.upgrade_unsupported = true,
        }
    }

    if !pkglist.is_empty() {
        plan.install =
            packages::plan_install(managers, &config, &state.packages, &pkglist, log)?;
    }

    if (upgrade || !pkglist.is_empty()) && reboot_if_required {
        plan.reboot.clone_from(&state.reboot_marker);
    }

    Ok(plan)
}

/// `handle`.
///
/// # Errors
///
/// The last exception of however many the three phases raised, matching
/// upstream's `raise errors[-1]`.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    const SOURCE: &str = "cc_package_update_upgrade_install.py";

    let managers = args.distro.package_managers;
    let state = State::probe(args.root, managers);
    let plan = plan(args.cfg, args.system_info, managers, &state, args.logger)?;
    let mut errors: Vec<String> = Vec::new();

    if let Err(error) = run_steps(&plan.update) {
        args.logger
            .warning(SOURCE, &format!("Package update failed\n{error}"));
        errors.push(error);
    }

    if plan.upgrade_unsupported {
        let error =
            "Unable to install packages. Debian family distros only.".to_owned();
        args.logger
            .warning(SOURCE, &format!("Package upgrade failed\n{error}"));
        errors.push(error);
    } else if let Err(error) = run_steps(&plan.upgrade) {
        args.logger
            .warning(SOURCE, &format!("Package upgrade failed\n{error}"));
        errors.push(error);
    }

    let install = run_steps(&plan.install.steps)
        .and_then(|()| plan.install.failed.clone().map_or(Ok(()), Err));
    if let Err(error) = install {
        args.logger.warning(
            SOURCE,
            &format!("Failure when attempting to install packages\n{error}"),
        );
        errors.push(error);
    }

    if let Some(marker) = &plan.reboot {
        args.logger.info(
            SOURCE,
            &format!("***WARNING*** Rebooting after upgrade or install per {marker}"),
        );
        if let Err(error) = fire_reboot() {
            args.logger.warning(
                SOURCE,
                &format!("Requested reboot did not happen!\n{error}"),
            );
            errors.push(error);
        }
    }

    match errors.pop() {
        None => Ok(()),
        Some(last) => {
            args.logger.warning(
                SOURCE,
                &format!(
                    "{} failed with exceptions, re-raising the last one",
                    errors.len() + 1
                ),
            );
            Err(last)
        }
    }
}

/// Run a phase, stopping at the first command that fails the way upstream's
/// single `try` around each distro call does.
fn run_steps(steps: &[Step]) -> Result<(), String> {
    packages::run(steps, &mut |step| {
        let mut command = ci_sys::subp::Subp::new(step.argv.clone()).inherit_env();
        for (key, value) in &step.env {
            command = command.env(key, value);
        }
        command
            .check()
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
}

/// `_fire_reboot`: ask for a reboot and panic if it does not arrive.
///
/// The waits are 1, 2, 4, 8, 16 and 32 seconds — a little over a minute in
/// total — after which the process is still running and something is wrong.
fn fire_reboot() -> Result<(), String> {
    let argv: Vec<String> = REBOOT_CMD.iter().map(|s| (*s).to_owned()).collect();
    ci_sys::subp::Subp::new(argv)
        .inherit_env()
        .run()
        .map_err(|error| error.to_string())?;
    let mut elapsed = 0_u64;
    let mut wait = 1_u64;
    for _ in 0..6 {
        std::thread::sleep(std::time::Duration::from_secs(wait));
        elapsed += wait;
        wait *= 2;
    }
    Err(format!("Reboot did not happen after {elapsed} seconds!"))
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

    fn cfg(text: &str) -> Object {
        let value =
            ci_config::yaml::load_yaml(text, ci_config::yaml::Limits::default())
                .unwrap();
        match value {
            Value::Object(object) => object,
            other => panic!("not a mapping: {other:?}"),
        }
    }

    /// A machine with both managers present and nothing to reboot for.
    fn both() -> State {
        State {
            packages: packages::State {
                apt_available: true,
                snap_available: true,
                all_packages: None,
                snap_refresh_hold: None,
            },
            reboot_marker: None,
        }
    }

    fn ubuntu() -> &'static [Manager] {
        &[Manager::Apt, Manager::Snap]
    }

    fn plan_ubuntu(text: &str, state: &State) -> Plan {
        let mut log = Logger::silent();
        plan(&cfg(text), &Object::new(), ubuntu(), state, &mut log).unwrap()
    }

    /// `get_cfg_option_bool` is `translate_bool`, not `bool()`: a non-empty
    /// string is not enough, it has to be one of four spellings. So a
    /// `package_update: maybe` quietly means no.
    #[test]
    fn multi_cfg_bool_get_is_translate_bool() {
        let keys = &["a", "b"];
        for (text, expected) in [
            ("{}", false),
            ("a: true", true),
            ("b: true", true),
            ("a: false", false),
            ("a: 'false'", false),
            ("a: 'no'", false),
            ("a: 'off'", false),
            ("a: '0'", false),
            ("a: 'yes'", true),
            ("a: 'on'", true),
            ("a: '1'", true),
            ("a: 'TRUE'", true),
            ("a: maybe", false),
            ("a: ''", false),
            ("a: 0", false),
            ("a: 1", true),
            ("a: []", false),
            ("a: [x]", false),
            ("a: {}", false),
            ("a: null", false),
            ("a: false\nb: true", true),
        ] {
            assert_eq!(
                multi_cfg_bool_get(&cfg(text), keys),
                expected,
                "for {text:?}"
            );
        }
    }

    /// `util.get_cfg_option_list`: absent and explicit `null` both give the
    /// empty list, a list is taken whole, and anything else is wrapped —
    /// stringified first unless it is already a string.
    #[test]
    fn option_list_shapes() {
        let list = |text: &str| option_list(&cfg(text), "packages");
        assert!(list("{}").is_empty());
        assert!(list("packages: null").is_empty());
        assert!(list("packages: []").is_empty());
        assert_eq!(list("packages: git"), vec![Value::String("git".to_owned())]);
        assert_eq!(list("packages: 5"), vec![Value::String("5".to_owned())]);
        assert_eq!(
            list("packages: true"),
            vec![Value::String("True".to_owned())]
        );
        assert_eq!(list("packages: [git, curl]").len(), 2);
    }

    /// Neither marker present means no reboot even when everything else asks
    /// for one; the first of the two that exists is the one reported.
    #[test]
    fn probe_picks_the_first_reboot_marker() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let probe = || State::probe(root, &[]);
        assert_eq!(probe().reboot_marker, None);

        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::write(root.join("run/reboot-needed"), "").unwrap();
        assert_eq!(probe().reboot_marker.as_deref(), Some("/run/reboot-needed"));

        std::fs::create_dir_all(root.join("var/run")).unwrap();
        std::fs::write(root.join("var/run/reboot-required"), "").unwrap();
        assert_eq!(
            probe().reboot_marker.as_deref(),
            Some("/var/run/reboot-required")
        );
    }

    /// An empty `package_managers` is what every non-debian distro has, and
    /// it is why they get the "Debian family distros only." refusal.
    #[test]
    fn probe_only_looks_for_managers_the_distro_registers() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::probe(dir.path(), &[]);
        assert!(!state.packages.apt_available);
        assert!(!state.packages.snap_available);
    }

    /// Nothing configured is nothing done — no update, no upgrade, no
    /// install, and no reboot.
    #[test]
    fn empty_config_plans_nothing() {
        assert_eq!(plan_ubuntu("{}", &both()), Plan::default());
    }

    /// `update_package_sources` runs for an upgrade as well as an update, and
    /// once either has asked for it a second ask changes nothing.
    #[test]
    fn update_runs_for_update_or_upgrade() {
        let state = both();
        assert_eq!(plan_ubuntu("package_update: true", &state).update.len(), 1);
        assert_eq!(plan_ubuntu("apt_update: true", &state).update.len(), 1);
        assert_eq!(plan_ubuntu("package_upgrade: true", &state).update.len(), 1);
        assert!(plan_ubuntu("{}", &state).update.is_empty());
        assert_eq!(
            plan_ubuntu("package_update: true\npackage_upgrade: true", &state)
                .update
                .len(),
            1
        );
    }

    /// The upgrade subcommand is `dist-upgrade`, not `upgrade`, and on Ubuntu
    /// it is followed by the snap refresh.
    #[test]
    fn upgrade_is_dist_upgrade() {
        let plan = plan_ubuntu("package_upgrade: true", &both());
        assert!(!plan.upgrade_unsupported);
        assert_eq!(plan.upgrade.len(), 3);
        assert_eq!(
            plan.upgrade[0].argv.last().map(String::as_str),
            Some("dist-upgrade")
        );
        assert_eq!(
            plan.upgrade[0].env,
            vec![("DEBIAN_FRONTEND".to_owned(), "noninteractive".to_owned())]
        );
        assert_eq!(plan.upgrade[2].argv, ["snap", "refresh"]);
    }

    /// A distro that registers no managers cannot run `package_command`, so
    /// the upgrade is recorded as refused rather than as a failure of the
    /// plan — upstream catches the exception and carries on to the install.
    #[test]
    fn upgrade_is_unsupported_without_apt() {
        let mut log = Logger::silent();
        let plan = plan(
            &cfg("package_upgrade: true"),
            &Object::new(),
            &[],
            &both(),
            &mut log,
        )
        .unwrap();
        assert!(plan.upgrade_unsupported);
        assert!(plan.upgrade.is_empty());
    }

    /// `install_packages` runs its own `update_package_sources` first, so a
    /// bare `packages:` still produces an `apt-get update` — under the same
    /// once-per-instance semaphore, which is what stops it running twice.
    #[test]
    fn install_updates_sources_first() {
        let plan = plan_ubuntu("packages: [git]", &both());
        assert!(plan.update.is_empty());
        assert_eq!(plan.install.steps.len(), 2);
        assert_eq!(
            plan.install.steps[0].argv.last().map(String::as_str),
            Some("update")
        );
        assert!(plan.install.steps[0].semaphore.is_some());
        assert_eq!(
            plan.install.steps[1].argv.last().map(String::as_str),
            Some("git")
        );
        assert!(plan.install.failed.is_none());
    }

    /// A `packages:` entry that is neither a string nor a two-element list is
    /// the one way this module fails while still deciding.
    #[test]
    fn malformed_entry_fails_the_plan() {
        let mut log = Logger::silent();
        let error = plan(
            &cfg("packages: [5]"),
            &Object::new(),
            ubuntu(),
            &both(),
            &mut log,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Invalid 'packages' yaml specification. Check schema definition."
        );
    }

    /// The reboot needs three things at once: a marker on disk, the config
    /// key, and something actually installed or upgraded to reboot for.
    #[test]
    fn reboot_needs_a_marker_a_flag_and_a_reason() {
        let mut marked = both();
        marked.reboot_marker = Some("/run/reboot-needed".to_owned());

        // No flag.
        assert_eq!(plan_ubuntu("packages: [git]", &marked).reboot, None);
        // No reason.
        assert_eq!(
            plan_ubuntu("apt_reboot_if_required: true", &marked).reboot,
            None
        );
        // No marker.
        assert_eq!(
            plan_ubuntu("apt_reboot_if_required: true\npackages: [git]", &both())
                .reboot,
            None
        );
        // All three.
        assert_eq!(
            plan_ubuntu("apt_reboot_if_required: true\npackages: [git]", &marked)
                .reboot
                .as_deref(),
            Some("/run/reboot-needed")
        );
        // An upgrade is a reason too, and the other spelling of the key works.
        assert_eq!(
            plan_ubuntu(
                "package_reboot_if_required: true\npackage_upgrade: true",
                &marked
            )
            .reboot
            .as_deref(),
            Some("/run/reboot-needed")
        );
    }
}
