//! `util.human2bytes`.
//!
//! Upstream's docstring is worth repeating: SI prefixes parse to IEC values
//! here (`1KB == 1024B`), which is wrong but long-established, and IEC spelling
//! was added later parsing to the same values. Both spellings are accepted and
//! both mean 1024.

/// The suffix table, in upstream's insertion order. Only one can ever match,
/// since they are distinct single characters.
const MPLIERS: [(char, u64); 5] = [
    ('B', 1),
    ('K', 1 << 10),
    ('M', 1 << 20),
    ('G', 1 << 30),
    ('T', 1 << 40),
];

/// `util.human2bytes`.
///
/// Upstream also documents an integer argument, but every caller guards with
/// `isinstance(size, str)` first, so that form is unreachable and is not
/// modelled.
///
/// # Errors
/// The two `ValueError`s upstream raises, and the two exceptions it does NOT
/// mean to raise — see COMPAT.md bug B82.
pub fn human2bytes(size: &str) -> Result<u64, String> {
    let stripped = size
        .strip_suffix("iB")
        .or_else(|| size.strip_suffix('B'))
        .unwrap_or(size);

    let mut num = stripped;
    let mut mplier = 1_u64;
    for (suffix, factor) in MPLIERS {
        if let Some(rest) = stripped.strip_suffix(suffix) {
            num = rest;
            mplier = factor;
        }
    }

    let value = py_float(num).ok_or_else(|| format!("'{size}' is not valid input."))?;
    if value < 0.0 {
        return Err(format!("'{size}': cannot be negative"));
    }

    // B82: `float()` accepts these two and `int()` then refuses them, so the
    // "is not valid input." guard above is bypassed. Reproduced, not fixed.
    #[expect(
        clippy::cast_precision_loss,
        reason = "every multiplier is a power of two below 2^41, exact in f64"
    )]
    let scaled = value * mplier as f64;
    if scaled.is_nan() {
        return Err("cannot convert float NaN to integer".to_owned());
    }
    if scaled.is_infinite() {
        return Err("cannot convert float infinity to integer".to_owned());
    }
    // Python would return a bignum; the cast saturates instead. Reaching it
    // needs a size no filesystem could hold. `int()` truncates toward zero,
    // which is what the cast does for the non-negative, finite value left here.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "value is checked non-negative and finite just above"
    )]
    Ok(scaled as u64)
}

/// Python's `float()` over a string. It tolerates surrounding whitespace and
/// underscores between digits; Rust's parser accepts neither.
fn py_float(text: &str) -> Option<f64> {
    let text = crate::pystr::strip(text);
    let cleaned = if text.contains('_') {
        strip_underscores(text)?
    } else {
        text.to_owned()
    };
    cleaned.parse::<f64>().ok()
}

/// Python allows `_` only with a digit on each side.
fn strip_underscores(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        if c != '_' {
            out.push(c);
            continue;
        }
        let before = i.checked_sub(1).and_then(|j| chars.get(j));
        let after = chars.get(i + 1);
        if !before.is_some_and(char::is_ascii_digit)
            || !after.is_some_and(char::is_ascii_digit)
        {
            return None;
        }
    }
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn si_and_iec_spellings_both_mean_1024() {
        assert_eq!(human2bytes("10M").unwrap(), 10_485_760);
        assert_eq!(human2bytes("10MB").unwrap(), 10_485_760);
        assert_eq!(human2bytes("10MiB").unwrap(), 10_485_760);
    }

    #[test]
    fn a_bare_number_is_bytes() {
        assert_eq!(human2bytes("10").unwrap(), 10);
        assert_eq!(human2bytes("10B").unwrap(), 10);
        assert_eq!(human2bytes("0").unwrap(), 0);
    }

    #[test]
    fn every_suffix_scales() {
        assert_eq!(human2bytes("1K").unwrap(), 1024);
        assert_eq!(human2bytes("1G").unwrap(), 1 << 30);
        assert_eq!(human2bytes("10T").unwrap(), 10_995_116_277_760);
    }

    #[test]
    fn the_number_is_a_float_so_fractions_and_exponents_work() {
        assert_eq!(human2bytes("10.5M").unwrap(), 11_010_048);
        assert_eq!(human2bytes("1e3").unwrap(), 1000);
    }

    #[test]
    fn unparseable_input_names_itself_unstripped() {
        // The message quotes the ORIGINAL argument, suffix and all.
        assert_eq!(human2bytes("10m").unwrap_err(), "'10m' is not valid input.");
        assert_eq!(human2bytes("B").unwrap_err(), "'B' is not valid input.");
        assert_eq!(human2bytes("iB").unwrap_err(), "'iB' is not valid input.");
        // Whitespace is stripped inside `float()`, which happens AFTER the
        // suffix match, so a padded suffix still fails.
        assert_eq!(
            human2bytes("  10M  ").unwrap_err(),
            "'  10M  ' is not valid input."
        );
    }

    #[test]
    fn whitespace_around_a_bare_number_is_accepted_as_python_does() {
        assert_eq!(human2bytes("  10  ").unwrap(), 10);
        assert_eq!(human2bytes(" 10M").unwrap(), 10_485_760);
    }

    #[test]
    fn underscores_are_allowed_only_between_digits() {
        assert_eq!(human2bytes("1_000").unwrap(), 1000);
        assert_eq!(human2bytes("1_0M").unwrap(), 10_485_760);
        assert_eq!(human2bytes("_1").unwrap_err(), "'_1' is not valid input.");
        assert_eq!(human2bytes("1_").unwrap_err(), "'1_' is not valid input.");
        assert_eq!(
            human2bytes("1._5").unwrap_err(),
            "'1._5' is not valid input."
        );
    }

    #[test]
    fn a_negative_size_has_its_own_message() {
        assert_eq!(human2bytes("-1M").unwrap_err(), "'-1M': cannot be negative");
        assert_eq!(
            human2bytes("-inf").unwrap_err(),
            "'-inf': cannot be negative"
        );
    }

    #[test]
    fn b82_infinity_and_nan_escape_the_not_valid_input_guard() {
        assert_eq!(
            human2bytes("inf").unwrap_err(),
            "cannot convert float infinity to integer"
        );
        assert_eq!(
            human2bytes("infinity").unwrap_err(),
            "cannot convert float infinity to integer"
        );
        assert_eq!(
            human2bytes("nan").unwrap_err(),
            "cannot convert float NaN to integer"
        );
    }
}
