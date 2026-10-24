//! Whether a Message addresses the Agent.
//!
//! The rule is deliberately small: the Agent's current Principal Name appears in the Message's
//! markdown as a whole word, case-insensitively, with or without a leading `@`. A name inside a
//! longer word (`Scouting` for `Scout`) is not a mention, and neither is an empty name.

/// Whether `markdown` mentions `name` as a whole word.
#[must_use]
pub fn mentions(markdown: &str, name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    let haystack = markdown.to_lowercase();
    let needle = name.to_lowercase();
    haystack.match_indices(&needle).any(|(start, found)| {
        let end = start + found.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// A character that continues a word; `@` and punctuation are boundaries.
fn is_word(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

#[cfg(test)]
mod tests {
    use super::mentions;

    #[test]
    fn whole_word_with_optional_at_sign_and_any_case() {
        assert!(mentions("Scout, can you help?", "Scout"));
        assert!(mentions("@Scout hello", "Scout"));
        assert!(mentions("hello @scout", "Scout"));
        assert!(mentions("HELLO SCOUT!", "scout"));
        assert!(mentions("(scout)", "Scout"));
        assert!(mentions("scout", "Scout"));
        assert!(mentions("ask Test Agent please", "Test Agent"));
        assert!(mentions("ask @Test Agent please", "Test Agent"));
        assert!(mentions("héllo Ünïcode-Scout", " Ünïcode-Scout "));
    }

    #[test]
    fn substrings_empty_names_and_other_names_do_not_match() {
        assert!(!mentions("Scouting ahead", "Scout"));
        assert!(!mentions("the boyscout", "Scout"));
        assert!(!mentions("scout_bot", "Scout"));
        assert!(!mentions("Scout2", "Scout"));
        assert!(!mentions("anything", ""));
        assert!(!mentions("anything", "   "));
        assert!(!mentions("@Pathfinder hello", "Scout"));
        assert!(!mentions("", "Scout"));
    }
}
