//! `Distro.apply_locale` and `Distro.get_locale`, for the debian family.
//!
//! Eleven classes define `apply_locale` upstream and no two bodies agree, so
//! unlike the hostname handling there is no shared implementation to port:
//! each distro would be its own transcription. [`LocaleWriter`] names all
//! eleven so the table records which one a distro would need, and this module
//! implements the debian one — which is what Ubuntu, and so the Azure images
//! this port is checked against, actually run.
//!
//! The debian body is worth reading carefully because it is mostly *not*
//! applying a locale. `locale-gen` is slow and `update-locale` triggers a
//! reconfigure, so upstream compares the requested locale against what
//! `/etc/default/locale` already says and does nothing when they agree. The
//! comparison has an asymmetry that survives here: an unset system locale
//! forces both halves to run even when the requested locale equals the
//! hardcoded default, because "unset" and "set to the default" are different
//! states to `update-locale`.

use crate::{Distro, LocaleReader, LocaleWriter};

const SOURCE: &str = "distros/debian.py";

/// `debian.LOCALE_CONF_FN`.
pub const LOCALE_CONF_FN: &str = "/etc/default/locale";

/// One effect of `apply_locale`.
///
/// All three shell out, so this is a plan in the same sense as
/// [`packages`](crate::packages)': the caller decides whether it is on a live
/// system before running any of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// `install_function(["locales"])`, reached when the command the next
    /// step needs is not on `PATH`.
    InstallPackages { packages: Vec<String> },
    /// `subp(["locale-gen", locale], capture=False)`.
    LocaleGen { locale: String },
    /// `subp(["update-locale", "--locale-file=..", "LANG=.."])`.
    UpdateLocale { argv: Vec<String> },
}

/// What the caller has to have looked up before [`plan`] can decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// `read_system_locale()`: `LANG` out of the conf file, or `None` when
    /// the file is missing or does not set it. Note that upstream's empty
    /// string and a missing file are the same state.
    pub system_locale: Option<String>,
    /// `os.path.exists(out_fn)`.
    pub conf_fn_exists: bool,
    /// `subp.which("locale-gen")`.
    pub has_locale_gen: bool,
    /// `subp.which("update-locale")`.
    pub has_update_locale: bool,
}

/// `read_system_locale(sys_path, keyname)`.
///
/// # Errors
/// A conf file that is not valid shell. Upstream lets `shlex`'s `ValueError`
/// out of the module.
pub fn read_system_locale(text: &str, keyname: &str) -> Result<Option<String>, String> {
    let data =
        ci_core::shlex::load_shell_content(text).map_err(|error| error.to_string())?;
    // `sys_defaults.get(keyname, "")` and then `if not self.system_locale`:
    // an empty value is indistinguishable from an absent one, which is why
    // this is an `Option` rather than a `String`.
    Ok(data.get(keyname).filter(|value| !value.is_empty()).cloned())
}

/// `Distro.get_locale`.
///
/// # Errors
/// The `NotImplementedError` seventeen distros raise. `DataSource.get_locale`
/// catches it and answers `en_US.UTF-8`; a caller that wants that behaviour
/// should do the same rather than treating this as fatal.
pub fn get_locale(
    distro: &Distro,
    system_locale: Option<&str>,
) -> Result<String, String> {
    match distro.locale_reader {
        LocaleReader::Debian => Ok(system_locale
            .filter(|value| !value.is_empty())
            .or(distro.default_locale)
            .unwrap_or_default()
            .to_owned()),
        LocaleReader::Alpine | LocaleReader::Rhel => Err(format!(
            "{:?}'s get_locale is not ported; this distro needs it",
            distro.locale_reader
        )),
        LocaleReader::Unsupported => Err("NotImplementedError".to_owned()),
    }
}

/// `Distro.apply_locale`, as a plan.
///
/// # Errors
/// The `ValueError("Failed to provide locale value.")` an empty locale raises,
/// or a variant this port does not implement.
pub fn plan(
    distro: &Distro,
    locale: &str,
    out_fn: Option<&str>,
    state: &State,
    log: &mut ci_log::Logger,
) -> Result<Vec<Step>, String> {
    if !matches!(distro.locale_writer, LocaleWriter::Debian) {
        return Err(format!(
            "{:?}'s apply_locale is not ported; this distro needs it to set a locale",
            distro.locale_writer
        ));
    }
    let keyname = "LANG";
    let out_fn = out_fn
        .filter(|path| !path.is_empty())
        .unwrap_or(LOCALE_CONF_FN);
    if locale.is_empty() {
        return Err("Failed to provide locale value.".to_owned());
    }

    let system_locale = state.system_locale.as_deref();
    let distro_locale = get_locale(distro, system_locale)?;
    let sys_locale_unset = system_locale.is_none_or(str::is_empty);
    if sys_locale_unset {
        log.debug(
            SOURCE,
            &format!(
                "System locale not found in {LOCALE_CONF_FN}. Assuming system locale \
                 is {} based on hardcoded default",
                distro.default_locale.unwrap_or_default()
            ),
        );
    } else {
        log.debug(
            SOURCE,
            &format!(
                "System locale set to {} via {LOCALE_CONF_FN}",
                system_locale.unwrap_or_default()
            ),
        );
    }

    // Note that the two lines report `LOCALE_CONF_FN` even when `out_fn` named
    // a different file, and that the second half is redundant: `need_regen`
    // already covers both of the other disjuncts, so `need_conf` can never be
    // true when `need_regen` is false. Kept as written.
    let need_regen = !locale.eq_ignore_ascii_case(&distro_locale)
        || !state.conf_fn_exists
        || sys_locale_unset;
    let need_conf = !state.conf_fn_exists || need_regen || sys_locale_unset;

    let mut steps = Vec::new();
    if need_regen {
        steps.extend(regenerate_locale(locale, state, keyname, log));
    } else {
        log.debug(
            SOURCE,
            &format!(
                "System has '{keyname}={}' requested '{locale}', skipping regeneration.",
                system_locale.unwrap_or_default()
            ),
        );
    }
    if need_conf {
        log.debug(
            SOURCE,
            &format!("Updating {out_fn} with locale setting {keyname}={locale}"),
        );
        if !state.has_update_locale {
            steps.push(Step::InstallPackages {
                packages: vec!["locales".to_owned()],
            });
        }
        steps.push(Step::UpdateLocale {
            argv: vec![
                "update-locale".to_owned(),
                format!("--locale-file={out_fn}"),
                format!("{keyname}={locale}"),
            ],
        });
    }
    Ok(steps)
}

/// `regenerate_locale`.
fn regenerate_locale(
    locale: &str,
    state: &State,
    keyname: &str,
    log: &mut ci_log::Logger,
) -> Vec<Step> {
    // The three locales glibc has without being asked. Compared lowercased,
    // so `C.UTF-8` matches, but not `en_US.UTF-8` — the common Ubuntu default
    // *is* regenerated every boot the system locale is unset.
    if matches!(locale.to_lowercase().as_str(), "c" | "c.utf-8" | "posix") {
        log.debug(
            SOURCE,
            &format!("{keyname}={locale} does not require rengeneration"),
        );
        return Vec::new();
    }
    let mut steps = Vec::new();
    if !state.has_locale_gen {
        steps.push(Step::InstallPackages {
            packages: vec!["locales".to_owned()],
        });
    }
    log.debug(SOURCE, &format!("Generating locales for {locale}"));
    steps.push(Step::LocaleGen {
        locale: locale.to_owned(),
    });
    steps
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
    use crate::fetch;

    fn ubuntu() -> &'static Distro {
        fetch("ubuntu").unwrap()
    }

    fn settled(locale: &str) -> State {
        State {
            system_locale: Some(locale.to_owned()),
            conf_fn_exists: true,
            has_locale_gen: true,
            has_update_locale: true,
        }
    }

    fn planned(locale: &str, state: &State) -> Vec<Step> {
        let mut log = ci_log::Logger::silent();
        plan(ubuntu(), locale, None, state, &mut log).unwrap()
    }

    #[test]
    fn a_locale_that_is_already_set_does_nothing_at_all() {
        assert_eq!(planned("en_GB.UTF-8", &settled("en_GB.UTF-8")), Vec::new());
    }

    #[test]
    fn the_comparison_ignores_case() {
        assert_eq!(planned("EN_GB.utf-8", &settled("en_GB.UTF-8")), Vec::new());
    }

    #[test]
    fn a_different_locale_is_generated_and_written() {
        assert_eq!(
            planned("fr_FR.UTF-8", &settled("en_GB.UTF-8")),
            vec![
                Step::LocaleGen {
                    locale: "fr_FR.UTF-8".to_owned(),
                },
                Step::UpdateLocale {
                    argv: vec![
                        "update-locale".to_owned(),
                        "--locale-file=/etc/default/locale".to_owned(),
                        "LANG=fr_FR.UTF-8".to_owned(),
                    ],
                },
            ]
        );
    }

    #[test]
    fn an_unset_system_locale_forces_the_work_even_for_the_default() {
        // `C.UTF-8` is `ubuntu`'s `default_locale`, so `distro_locale` equals
        // the request and the first disjunct is false -- but `sys_locale_unset`
        // is not, so `update-locale` runs anyway. Setting a locale to what the
        // system already reports is therefore not always a no-op.
        let state = State {
            system_locale: None,
            ..settled("")
        };
        assert_eq!(
            planned("C.UTF-8", &state),
            vec![Step::UpdateLocale {
                argv: vec![
                    "update-locale".to_owned(),
                    "--locale-file=/etc/default/locale".to_owned(),
                    "LANG=C.UTF-8".to_owned(),
                ],
            }]
        );
    }

    #[test]
    fn the_three_builtin_locales_skip_generation_but_not_the_conf_file() {
        for locale in ["C", "c.utf-8", "POSIX"] {
            let steps = planned(locale, &settled("en_GB.UTF-8"));
            assert_eq!(steps.len(), 1, "{locale}: {steps:?}");
            assert!(matches!(steps[0], Step::UpdateLocale { .. }), "{locale}");
        }
    }

    #[test]
    fn a_missing_tool_installs_the_locales_package_first() {
        let state = State {
            has_locale_gen: false,
            has_update_locale: false,
            ..settled("en_GB.UTF-8")
        };
        let steps = planned("fr_FR.UTF-8", &state);
        // Twice, because each helper checks and installs independently.
        assert_eq!(
            steps
                .iter()
                .filter(|s| matches!(s, Step::InstallPackages { .. }))
                .count(),
            2
        );
        assert!(matches!(steps[0], Step::InstallPackages { .. }));
        assert!(matches!(steps[1], Step::LocaleGen { .. }));
    }

    #[test]
    fn need_conf_can_never_outrun_need_regen() {
        // The `need_conf` expression has three disjuncts and `need_regen`
        // already implies all of them, so the file is written exactly when the
        // locale is generated. Pinned because it reads as though it might not.
        for system_locale in [None, Some(String::new()), Some("en_GB.UTF-8".to_owned())]
        {
            for conf_fn_exists in [true, false] {
                for locale in ["en_GB.UTF-8", "fr_FR.UTF-8"] {
                    let state = State {
                        system_locale: system_locale.clone(),
                        conf_fn_exists,
                        has_locale_gen: true,
                        has_update_locale: true,
                    };
                    let steps = planned(locale, &state);
                    let wrote =
                        steps.iter().any(|s| matches!(s, Step::UpdateLocale { .. }));
                    let quiet = steps.is_empty();
                    assert!(wrote || quiet, "{locale} {state:?} -> {steps:?}");
                }
            }
        }
    }

    #[test]
    fn an_explicit_config_file_is_used_but_the_log_still_names_the_default() {
        let steps = plan(
            ubuntu(),
            "fr_FR.UTF-8",
            Some("/etc/locale.conf"),
            &settled("en_GB.UTF-8"),
            &mut ci_log::Logger::silent(),
        )
        .unwrap();
        assert_eq!(
            steps[1],
            Step::UpdateLocale {
                argv: vec![
                    "update-locale".to_owned(),
                    "--locale-file=/etc/locale.conf".to_owned(),
                    "LANG=fr_FR.UTF-8".to_owned(),
                ],
            }
        );
    }

    #[test]
    fn an_empty_locale_raises() {
        let error = plan(
            ubuntu(),
            "",
            None,
            &settled("en_GB.UTF-8"),
            &mut ci_log::Logger::silent(),
        )
        .unwrap_err();
        assert_eq!(error, "Failed to provide locale value.");
    }

    #[test]
    fn the_other_ten_writers_are_not_ported() {
        for distro in crate::DISTROS {
            let result = plan(
                distro,
                "fr_FR.UTF-8",
                None,
                &settled("en_GB.UTF-8"),
                &mut ci_log::Logger::silent(),
            );
            assert_eq!(
                result.is_ok(),
                matches!(distro.locale_writer, LocaleWriter::Debian),
                "{}",
                distro.name
            );
        }
    }

    #[test]
    fn get_locale_falls_back_to_the_hardcoded_default() {
        assert_eq!(get_locale(ubuntu(), None).unwrap(), "C.UTF-8");
        assert_eq!(get_locale(ubuntu(), Some("")).unwrap(), "C.UTF-8");
        assert_eq!(
            get_locale(ubuntu(), Some("fr_FR.UTF-8")).unwrap(),
            "fr_FR.UTF-8"
        );
    }

    #[test]
    fn seventeen_distros_cannot_answer_get_locale_at_all() {
        let unsupported = crate::DISTROS
            .iter()
            .filter(|d| matches!(d.locale_reader, LocaleReader::Unsupported))
            .count();
        assert_eq!(unsupported, 17);
        assert!(get_locale(fetch("photon").unwrap(), None).is_err());
    }

    #[test]
    fn an_empty_lang_in_the_conf_file_reads_as_unset() {
        assert_eq!(read_system_locale("LANG=\n", "LANG").unwrap(), None);
        assert_eq!(read_system_locale("LC_ALL=C\n", "LANG").unwrap(), None);
        assert_eq!(
            read_system_locale("LANG=en_GB.UTF-8\n", "LANG").unwrap(),
            Some("en_GB.UTF-8".to_owned())
        );
    }
}
