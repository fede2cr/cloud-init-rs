//! Port of `cloudinit/config/cc_timezone.py`.
//!
//! Four lines of decision and one call into the distro, which is where all the
//! behaviour is — see [`ci_distro::timezone`].
//!
//! The one thing worth knowing at this level is that the module's default is
//! `False` rather than the empty string, and the check is `if not timezone`.
//! So a `timezone` key holding `0`, `false` or an empty list all reach
//! `str()` first and become `"0"`, `"False"` and `"[]"` — non-empty strings,
//! which are truthy, which means the module proceeds and fails on a zone file
//! that does not exist. Only a genuinely absent key skips quietly.

use ci_config::Value;
use ci_distro::timezone::{self, LocalTime};

use super::Args;

const SOURCE: &str = "cc_timezone.py";

/// `handle`.
///
/// # Errors
/// `_find_tz_file`'s `IOError` for an unknown zone, a distro whose
/// `set_timezone` is not ported, or a failed write.
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let Some(tz) = configured(args.args, args.cfg) else {
        let name = args.name.to_owned();
        args.debug(
            SOURCE,
            &format!("Skipping module named {name}, no 'timezone' specified"),
        );
        return Ok(());
    };

    // Everything the plan depends on, read through the root so that a fixture
    // directory can stand in for `/usr/share/zoneinfo` and `/etc`.
    let tz_file = timezone::zone_file(&tz);
    let exists = super::rooted(args.root, &tz_file).is_file();
    let tz_local = args.distro.tz_local_fn.unwrap_or("/etc/localtime");
    let localtime = LocalTime::of(&super::rooted(args.root, tz_local));
    let systemd = ci_core::status::uses_systemd();

    let steps = timezone::plan(args.distro, &tz, exists, localtime, systemd)?;
    timezone::run(&steps, args.root, args.logger)
}

/// `args[0]` if the module section carried one, else
/// `util.get_cfg_option_str(cfg, "timezone", False)`, else nothing.
#[must_use]
pub fn configured(args: &Value, cfg: &ci_config::Object) -> Option<String> {
    if let Some(first) = args.as_array().and_then(|items| items.first()) {
        // No `str()` here: upstream takes `args[0]` as it comes, so a
        // non-string reaches `os.path.join` and raises there instead.
        return Some(super::py_str(first));
    }
    let value = cfg.get("timezone")?;
    let text = super::py_str(value);
    (!text.is_empty()).then_some(text)
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

    fn from_cfg(text: &str) -> Option<String> {
        configured(&Value::Null, &cfg(text))
    }

    #[test]
    fn an_absent_key_skips() {
        assert_eq!(from_cfg("other: 1"), None);
    }

    #[test]
    fn a_plain_name_is_taken_as_written() {
        assert_eq!(
            from_cfg("timezone: Europe/Madrid"),
            Some("Europe/Madrid".to_owned())
        );
    }

    #[test]
    fn an_empty_string_skips_but_a_falsy_scalar_does_not() {
        // `if not timezone` runs on the *string*, and `str(0)` is not empty.
        assert_eq!(from_cfg("timezone: ''"), None);
        assert_eq!(from_cfg("timezone: 0"), Some("0".to_owned()));
        assert_eq!(from_cfg("timezone: false"), Some("False".to_owned()));
        assert_eq!(from_cfg("timezone: []"), Some("[]".to_owned()));
    }

    #[test]
    fn a_null_value_becomes_the_word_none() {
        // Which is then looked for under /usr/share/zoneinfo and is not there.
        assert_eq!(from_cfg("timezone:"), Some("None".to_owned()));
    }

    #[test]
    fn a_module_argument_wins_over_the_config() {
        let args = Value::Array(vec![Value::from("UTC")]);
        assert_eq!(
            configured(&args, &cfg("timezone: Europe/Madrid")),
            Some("UTC".to_owned())
        );
    }

    #[test]
    fn an_empty_argument_list_falls_through_to_the_config() {
        let args = Value::Array(Vec::new());
        assert_eq!(
            configured(&args, &cfg("timezone: UTC")),
            Some("UTC".to_owned())
        );
    }
}
