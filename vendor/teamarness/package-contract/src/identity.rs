//! The Package Identity grammar.

/// Returns whether `value` is a well-formed lowercase reverse-DNS Package Identity.
///
/// The grammar matches `PACKAGE_IDENTITY_PATTERN` in `scripts/create-package.mjs`: at least two
/// dot-separated labels, each starting with an ASCII lowercase letter, containing only lowercase
/// letters, digits and `-`, not ending in `-`, at most 63 bytes per label and 3..=128 bytes
/// overall.
#[must_use]
pub fn is_valid_package_identity(value: &str) -> bool {
    if !(3..=128).contains(&value.len()) || !value.is_ascii() {
        return false;
    }
    let labels: Vec<_> = value.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    !labels.iter().any(|label| {
        label.is_empty()
            || label.len() > 63
            || !label.as_bytes()[0].is_ascii_lowercase()
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    })
}

#[cfg(test)]
mod tests {
    use super::is_valid_package_identity;

    #[test]
    fn accepts_reverse_dns_identities() {
        assert!(is_valid_package_identity("dev.teamarness.tutorial"));
        assert!(is_valid_package_identity("a.b1-c"));
    }

    #[test]
    fn rejects_malformed_identities() {
        for value in [
            "",
            "ab",
            "single",
            "dev..tutorial",
            "dev.Tutorial",
            "dev.1tutorial",
            "dev.tutorial-",
            "dev.tutorial_x",
            "dev.tutorialé",
        ] {
            assert!(!is_valid_package_identity(value), "accepted {value:?}");
        }
        assert!(!is_valid_package_identity(&format!(
            "dev.{}",
            "a".repeat(126)
        )));
    }
}
