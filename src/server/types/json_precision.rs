//! Check numeric fidelity before promoting JSON text to structured values.

#[derive(PartialEq, Eq)]
struct NumericValue {
    negative: bool,
    digits: String,
    exponent: i64,
}

/// The caller parses JSON first; strings cannot contribute numeric tokens.
pub(super) fn preserves_numeric_values(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            index = skip_string(bytes, index);
        } else if bytes[index] == b'-' || bytes[index].is_ascii_digit() {
            let start = index;
            index += 1;
            while index < bytes.len() && is_number_byte(bytes[index]) {
                index += 1;
            }
            if !preserves_number(&text[start..index]) {
                return false;
            }
        } else {
            index += 1;
        }
    }
    true
}

fn skip_string(bytes: &[u8], mut index: usize) -> usize {
    index += 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    index
}

fn is_number_byte(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E')
}

fn preserves_number(text: &str) -> bool {
    let Ok(number) = serde_json::from_str::<serde_json::Number>(text) else {
        return false;
    };
    let original = normalize_number(text);
    original.is_some() && original == normalize_number(&number.to_string())
}

fn normalize_number(text: &str) -> Option<NumericValue> {
    let negative = text.starts_with('-');
    let unsigned = text.strip_prefix('-').unwrap_or(text);
    let (coefficient, exponent) = unsigned.split_once(['e', 'E']).unwrap_or((unsigned, "0"));
    let fraction_length = coefficient
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let coefficient = coefficient.replace('.', "");
    let digits = coefficient.trim_start_matches('0');
    if digits.is_empty() {
        return Some(NumericValue {
            negative: false,
            digits: "0".to_string(),
            exponent: 0,
        });
    }
    let significant = digits.trim_end_matches('0');
    let trailing_zeros = i64::try_from(digits.len() - significant.len()).ok()?;
    let fraction_length = i64::try_from(fraction_length).ok()?;
    let exponent = exponent
        .parse::<i64>()
        .ok()?
        .checked_sub(fraction_length)?
        .checked_add(trailing_zeros)?;
    Some(NumericValue {
        negative,
        digits: significant.to_string(),
        exponent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_numeric_fidelity_preserves_equivalent_forms_and_native_integers() {
        for number in [
            "0",
            "-0.0",
            "0.1000000",
            "10e-1",
            "1e20",
            "1e-200",
            "9007199254740993",
            "18446744073709551615",
            "-9223372036854775808",
        ] {
            assert!(preserves_number(number), "{number}");
        }
    }

    #[test]
    fn test_numeric_fidelity_rejects_rounding_underflow_and_overflow() {
        for number in [
            "18446744073709551617",
            "0.10000000000000001",
            "1e-400",
            "1e400",
        ] {
            assert!(!preserves_number(number), "{number}");
        }
    }

    #[test]
    fn test_numeric_fidelity_ignores_quoted_and_escaped_numeric_text() {
        let text = r#"{"18446744073709551617":"0.10000000000000001\\\"", "array":[1.00e2, {"value":0.1}]}"#;
        assert!(serde_json::from_str::<serde_json::Value>(text).is_ok());
        assert!(preserves_numeric_values(text));
        assert!(!preserves_numeric_values(
            r#"{"nested":[{"value":18446744073709551617}]}"#
        ));
    }
}
