//! Port of `cloudinit/config/cc_apt_pipelining.py`: how many requests apt may
//! have outstanding on one connection.
//!
//! The module exists because HTTP pipelining is where a broken proxy turns
//! into a corrupted package. Apt's own default is 10; a cache that does not
//! linger on the connection properly will interleave the responses, and apt
//! will happily install whatever it got. `apt_pipelining: false` writes a
//! depth of `0`, which is the only setting that is safe against that, and is
//! what the datasources that know they sit behind such a proxy ask for.
//!
//! The whole module is one config key, one file and no commands, so unlike
//! most of the ported modules there is nothing to plan: [`handle`] decides and
//! writes in one pass.

use ci_config::Value;
use ci_core::pystr;

use super::Args;

const SOURCE: &str = "cc_apt_pipelining.py";

/// `DEFAULT_FILE`.
pub const DEFAULT_FILE: &str = "/etc/apt/apt.conf.d/90cloud-init-pipelining";

/// `APT_PIPE_TPL`, with `%s` still in it.
const APT_PIPE_TPL: &str =
    "//Written by cloud-init per 'apt_pipelining'\nAcquire::http::Pipeline-Depth \"{}\";\n";

/// What the module decided to do with the configured value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Write [`DEFAULT_FILE`] with this depth. Always one of `0` .. `5`, as a
    /// string, because that is what goes into the template.
    Write(String),
    /// `none`, `unchanged` and `os` all leave apt's own default alone.
    Leave,
    /// Anything else: a warning naming the value as the config spelled it,
    /// and no file. Note that the module still *succeeds* — a typo here is
    /// silently not applied.
    Invalid(String),
}

/// `handle`.
///
/// # Errors
/// Only a failure to write [`DEFAULT_FILE`].
pub fn handle(args: &mut Args<'_>) -> Result<(), String> {
    let configured = args.cfg.get("apt_pipelining");
    match decide(configured) {
        Action::Write(setting) => {
            let path = super::rooted(args.root, DEFAULT_FILE);
            super::write_file(&path, render(&setting).as_bytes())?;
            args.debug(
                SOURCE,
                &format!(
                    "Wrote {DEFAULT_FILE} with apt pipeline depth setting {setting}"
                ),
            );
            Ok(())
        }
        Action::Leave => Ok(()),
        Action::Invalid(shown) => {
            args.warning(
                SOURCE,
                &format!("Invalid option for apt_pipelining: {shown}"),
            );
            Ok(())
        }
    }
}

/// `str(cfg.get("apt_pipelining", "os")).lower().strip()` and the chain of
/// comparisons that follows it.
///
/// Note the order: upstream lowercases *before* stripping, and both operate on
/// the `str()` of whatever the key held. So `apt_pipelining: [1]` becomes the
/// string `[1]` and lands in [`Action::Invalid`] rather than raising, and a
/// bare `apt_pipelining:` becomes `none` — the same as writing it out — which
/// is why an empty value disables the module instead of enabling it.
#[must_use]
pub fn decide(configured: Option<&Value>) -> Action {
    let raw = configured.map_or_else(|| "os".to_owned(), super::py_str);
    let lowered = raw.to_lowercase();
    let setting = pystr::strip(&lowered);

    if setting == "false" {
        // Not `0` because `str(False).lower()` is `false`, which is not in
        // the numeric list below; upstream spells the depth out separately.
        return Action::Write("0".to_owned());
    }
    if matches!(setting, "none" | "unchanged" | "os") {
        return Action::Leave;
    }
    // `[str(b) for b in range(6)]`. Not a numeric range check: `apt_pipelining:
    // 05` and `apt_pipelining: " 3"` are both strings that do not match, and
    // `true` does not either.
    if matches!(setting, "0" | "1" | "2" | "3" | "4" | "5") {
        return Action::Write(setting.to_owned());
    }
    Action::Invalid(raw)
}

/// `APT_PIPE_TPL % setting`.
#[must_use]
pub fn render(setting: &str) -> String {
    APT_PIPE_TPL.replacen("{}", setting, 1)
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

    fn decided(value: &Value) -> Action {
        decide(Some(value))
    }

    #[test]
    fn absent_key_leaves_apt_alone() {
        assert_eq!(decide(None), Action::Leave);
    }

    #[test]
    fn os_none_and_unchanged_leave_apt_alone() {
        for word in ["os", "none", "unchanged", "OS", "  None  ", "UNCHANGED"] {
            assert_eq!(decided(&Value::from(word)), Action::Leave, "{word}");
        }
    }

    #[test]
    fn a_bare_key_reads_as_none() {
        // `apt_pipelining:` with nothing after it is YAML null, and
        // `str(None).lower()` is exactly the word that disables the module.
        assert_eq!(decided(&Value::Null), Action::Leave);
    }

    #[test]
    fn false_writes_depth_zero() {
        assert_eq!(decided(&Value::Bool(false)), Action::Write("0".to_owned()));
        assert_eq!(
            decided(&Value::from("false")),
            Action::Write("0".to_owned())
        );
        assert_eq!(
            decided(&Value::from("False")),
            Action::Write("0".to_owned())
        );
    }

    #[test]
    fn true_is_not_a_depth() {
        // Symmetry would suggest `true` means "apt's default"; it does not.
        assert_eq!(
            decided(&Value::Bool(true)),
            Action::Invalid("True".to_owned())
        );
    }

    #[test]
    fn zero_through_five_are_depths() {
        for depth in 0..=5u64 {
            assert_eq!(
                decided(&Value::from(depth)),
                Action::Write(depth.to_string()),
                "{depth}"
            );
        }
    }

    #[test]
    fn six_is_out_of_range() {
        assert_eq!(decided(&Value::from(6u64)), Action::Invalid("6".to_owned()));
    }

    #[test]
    fn the_warning_quotes_the_value_not_the_normalised_form() {
        // `LOG.warning(.., apt_pipe_value)` takes the original object, so the
        // operator sees what they wrote rather than what it was folded to.
        assert_eq!(
            decided(&Value::from("  Yes  ")),
            Action::Invalid("  Yes  ".to_owned())
        );
    }

    #[test]
    fn a_list_is_stringified_rather_than_rejected() {
        let value = Value::Array(vec![Value::from(1u64)]);
        assert_eq!(decided(&value), Action::Invalid("[1]".to_owned()));
    }

    #[test]
    fn a_padded_digit_is_not_a_depth_but_a_padded_word_is() {
        // `.strip()` runs after the comparison list is built from bare
        // digits, so whitespace is removed for both -- but `05` never was a
        // member.
        assert_eq!(
            decided(&Value::from("  3  ")),
            Action::Write("3".to_owned())
        );
        assert_eq!(
            decided(&Value::from("05")),
            Action::Invalid("05".to_owned())
        );
    }

    #[test]
    fn the_snippet_names_the_key_that_produced_it() {
        assert_eq!(
            render("0"),
            "//Written by cloud-init per 'apt_pipelining'\nAcquire::http::Pipeline-Depth \"0\";\n"
        );
    }
}
