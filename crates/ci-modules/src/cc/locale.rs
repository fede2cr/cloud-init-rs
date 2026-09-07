//! Port of `cloudinit/config/cc_locale.py`.
//!
//! Like `cc_timezone` this is a handful of lines around one distro call, and
//! the behaviour lives in [`ci_distro::locale`]. What is decided *here* is
//! which locale to apply, and that has one turn worth spelling out: when the
//! config does not name one, the fallback is not a constant but the locale the
//! datasource reports, which for every datasource in 26.1 is the locale the
//! distro already has. So an unconfigured `cc_locale` asks the system to set
//! the locale it is already set to — which is usually, but not always, a
//! no-op. See [`ci_distro::locale::plan`] for the case where it is not.

use ci_config::Value;
use ci_distro::locale::{self, State, Step};

use super::Args;

const SOURCE: &str = "cc_locale.py";

/// `DataSource.default_locale`, the answer when the distro cannot supply one.
pub const DEFAULT_LOCALE: &str = "en_US.UTF-8";

/// `handle`.
///
/// # Errors
/// An empty locale, a distro whose `apply_locale` is not ported, an unreadable
/// or malformed conf file, or a failed command.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    // `cloud.get_locale()` is the *default argument* of the config lookup, so
    // it is evaluated whether or not the config names a locale, and it always
    // reads the hardcoded `/etc/default/locale` -- never `locale_configfile`.
    let system_locale = read_system_locale(args.root, locale::LOCALE_CONF_FN)?;
    let locale = configured(args.args, args.cfg, args.distro, system_locale.as_deref());

    if ci_config::option::is_false(&Value::from(locale.clone())) {
        let (name, shown) = (args.name.to_owned(), locale.clone());
        args.debug(
            SOURCE,
            &format!("Skipping module named {name}, disabled by config: {shown}"),
        );
        return Ok(());
    }
    args.debug(SOURCE, &format!("Setting locale to {locale}"));

    let out_fn = config_file(args.cfg);
    let out_fn = out_fn.as_deref();
    let conf_path = out_fn.unwrap_or(locale::LOCALE_CONF_FN);
    let state = State {
        system_locale,
        // `os.path.exists(out_fn)` -- a different file from the one the system
        // locale was read out of, whenever `locale_configfile` is set.
        conf_fn_exists: super::rooted(args.root, conf_path).exists(),
        has_locale_gen: ci_sys::subp::which("locale-gen").is_some(),
        has_update_locale: ci_sys::subp::which("update-locale").is_some(),
    };

    let steps = locale::plan(args.distro, &locale, out_fn, &state, args.logger)?;
    run(&steps, args)
}

/// `debian.read_system_locale`, which reports "unset" for a missing file.
fn read_system_locale(
    root: &std::path::Path,
    path: &str,
) -> Result<Option<String>, String> {
    let path = super::rooted(root, path);
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    locale::read_system_locale(&text, "LANG")
}

/// `util.get_cfg_option_str(cfg, "locale_configfile")`. An empty answer is
/// folded into "absent" here rather than in `apply_locale`, which spells it
/// `if not out_fn`.
#[must_use]
pub fn config_file(cfg: &ci_config::Object) -> Option<String> {
    cfg.get("locale_configfile")
        .map(super::py_str)
        .filter(|path| !path.is_empty())
}

/// Carry out a plan. Every step shells out, so none of them run when the
/// module is pointed at a fixture root.
fn run(steps: &[Step], args: &mut Args<'_>) -> Result<(), String> {
    let live = args.root == std::path::Path::new("/");
    for step in steps {
        match step {
            Step::InstallPackages { packages } => {
                if live {
                    // `install_function` is `self.install_packages`, which for
                    // debian is apt with the update-then-install dance that
                    // `cc_package_update_upgrade_install` already drives.
                    return Err(format!(
                        "install_packages is not ported here; {} would have been \
                         installed to apply a locale",
                        packages.join(" ")
                    ));
                }
            }
            Step::LocaleGen { locale } if live => {
                // `capture=False`, so the output goes to cloud-init's own
                // stdout rather than into the log.
                ci_sys::subp::Subp::new(["locale-gen", locale.as_str()])
                    .passthrough()
                    .map_err(|error| error.to_string())
                    .and_then(status)?;
            }
            Step::UpdateLocale { argv } if live => {
                ci_sys::subp::Subp::new(argv.iter().map(String::as_str))
                    .passthrough()
                    .map_err(|error| error.to_string())
                    .and_then(status)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// `subp` raises `ProcessExecutionError` on a non-zero exit even when it is
/// not capturing, so a passthrough command still has to be checked.
fn status(status: std::process::ExitStatus) -> Result<(), String> {
    if status.success() {
        return Ok(());
    }
    Err(format!(
        "Unexpected error while running command.\nExit code: {status}"
    ))
}

/// `args[0]`, else `util.get_cfg_option_str(cfg, "locale", cloud.get_locale())`.
///
/// `cloud.get_locale()` reaches `DataSource.get_locale`, which asks the distro
/// and falls back to `en_US.UTF-8` for the seventeen that raise
/// `NotImplementedError`. `system_locale` is what `/etc/default/locale` says,
/// which for the debian family *is* the answer whenever it is set.
#[must_use]
pub fn configured(
    args: &Value,
    cfg: &ci_config::Object,
    distro: &ci_distro::Distro,
    system_locale: Option<&str>,
) -> String {
    if let Some(first) = args.as_array().and_then(|items| items.first()) {
        return super::py_str(first);
    }
    if let Some(value) = cfg.get("locale") {
        return super::py_str(value);
    }
    locale::get_locale(distro, system_locale)
        .unwrap_or_else(|_| DEFAULT_LOCALE.to_owned())
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
    use ci_config::Object;

    fn cfg(text: &str) -> Object {
        ci_config::yaml::load_yaml(text, ci_config::yaml::Limits::default())
            .unwrap()
            .as_object()
            .unwrap()
            .clone()
    }

    fn distro(name: &str) -> &'static ci_distro::Distro {
        ci_distro::fetch(name).unwrap()
    }

    fn from_cfg(name: &str, text: &str) -> String {
        configured(&Value::Null, &cfg(text), distro(name), None)
    }

    #[test]
    fn the_config_wins() {
        assert_eq!(from_cfg("ubuntu", "locale: fr_FR.UTF-8"), "fr_FR.UTF-8");
    }

    #[test]
    fn without_a_key_the_distros_hardcoded_default_is_used() {
        assert_eq!(from_cfg("ubuntu", "other: 1"), "C.UTF-8");
    }

    #[test]
    fn without_a_key_a_set_system_locale_beats_the_hardcoded_default() {
        // `cloud.get_locale()` reaches `debian.Distro.get_locale`, which reads
        // `/etc/default/locale` and only falls back to `default_locale` when
        // that file does not set `LANG`.
        assert_eq!(
            configured(
                &Value::Null,
                &cfg("other: 1"),
                distro("ubuntu"),
                Some("en_GB.UTF-8")
            ),
            "en_GB.UTF-8"
        );
    }

    #[test]
    fn a_distro_that_cannot_answer_ignores_the_system_locale_too() {
        // The seventeen raise before they can look, so the datasource's own
        // default is what comes back however the file reads.
        assert_eq!(
            configured(
                &Value::Null,
                &cfg("other: 1"),
                distro("photon"),
                Some("en_GB.UTF-8")
            ),
            "en_US.UTF-8"
        );
    }

    #[test]
    fn a_distro_that_cannot_answer_gets_the_datasources_default() {
        // Seventeen of them, including every suse and photon.
        assert_eq!(from_cfg("photon", "other: 1"), "en_US.UTF-8");
    }

    #[test]
    fn the_false_strings_disable_the_module() {
        for word in ["false", "0", "no", "off", "  OFF  "] {
            assert!(ci_config::option::is_false(&Value::from(word)), "{word}");
        }
    }

    #[test]
    fn an_empty_locale_is_not_a_false_string_and_so_reaches_the_distro() {
        // `is_false("")` is false, so the module proceeds and `apply_locale`
        // raises `Failed to provide locale value.` rather than skipping.
        assert!(!ci_config::option::is_false(&Value::from("")));
        assert_eq!(from_cfg("ubuntu", "locale: ''"), "");
    }

    #[test]
    fn a_module_argument_wins_over_the_config() {
        let args = Value::Array(vec![Value::from("fr_FR.UTF-8")]);
        assert_eq!(
            configured(&args, &cfg("locale: de_DE.UTF-8"), distro("ubuntu"), None),
            "fr_FR.UTF-8"
        );
    }

    #[test]
    fn a_non_string_locale_is_stringified() {
        assert_eq!(from_cfg("ubuntu", "locale: 1"), "1");
        assert_eq!(from_cfg("ubuntu", "locale:"), "None");
    }
}
