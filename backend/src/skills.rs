//! The Agent's own skills, compiled into the Backend.
//!
//! Each skill is a `SKILL.md` under `backend/skills/<name>/`, in the agentskills.io shape: a
//! `---` fenced frontmatter with single-line `name` and `description`, then a Markdown body. The
//! model sees each skill's name and description in its system message and reads the body on
//! request with the `skill_read` tool. The frontmatter reader is deliberately restricted: we
//! write these files, and the tests hold every bundled skill to the specification's rules.

use std::fmt;
use std::sync::OnceLock;

/// Every bundled skill as `(directory, SKILL.md source)`.
pub const SKILLS: &[(&str, &str)] = &[(
    "teamarness-packages",
    include_str!("../skills/teamarness-packages/SKILL.md"),
)];

/// The longest skill name the specification allows.
pub const MAX_NAME_CHARS: usize = 64;
/// The longest skill description the specification allows.
pub const MAX_DESCRIPTION_CHARS: usize = 1024;

/// One compiled-in skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The Markdown after the frontmatter, trimmed.
    pub body: String,
}

/// A `SKILL.md` that does not meet the restricted frontmatter or the specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillError(String);

impl fmt::Display for SkillError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "skill: {}", self.0)
    }
}

impl std::error::Error for SkillError {}

/// Reads the `SKILL.md` `source` found in the directory `directory`.
///
/// # Errors
/// The frontmatter is not a `---` fence holding exactly one single-line `name` and
/// `description`, the name breaks the specification or differs from `directory`, the
/// description is empty or too long, or the body is empty.
pub fn parse(directory: &str, source: &str) -> Result<Skill, SkillError> {
    let fail = |cause: &str| SkillError(format!("{directory}: {cause}"));
    let rest = source
        .strip_prefix("---\n")
        .ok_or_else(|| fail("SKILL.md does not open with a `---` fence"))?;
    let mut name = None;
    let mut description = None;
    let mut consumed = 0;
    let mut closed = false;
    for line in rest.split_inclusive('\n') {
        consumed += line.len();
        let line = line.strip_suffix('\n').unwrap_or(line);
        if line == "---" {
            closed = true;
            break;
        }
        // Keys start at the line start, so an indented continuation line is refused too.
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| fail("a frontmatter line is not `key: value`"))?;
        let value = value.trim();
        if value.starts_with(['>', '|', '"', '\'']) {
            return Err(fail("frontmatter values are plain single-line text"));
        }
        let slot = match key {
            "name" => &mut name,
            "description" => &mut description,
            _ => return Err(fail("frontmatter holds only `name` and `description`")),
        };
        if slot.replace(value.to_owned()).is_some() {
            return Err(fail("a frontmatter key is repeated"));
        }
    }
    if !closed {
        return Err(fail("the frontmatter fence is not closed"));
    }
    let name = name.ok_or_else(|| fail("frontmatter has no `name`"))?;
    let description = description.ok_or_else(|| fail("frontmatter has no `description`"))?;
    let body = rest[consumed..].trim();
    if !valid_name(&name) {
        return Err(fail(
            "the name is not 1–64 lowercase letters, digits and single hyphens",
        ));
    }
    if name != directory {
        return Err(fail("the name differs from its directory"));
    }
    if !(1..=MAX_DESCRIPTION_CHARS).contains(&description.chars().count()) {
        return Err(fail(
            "the description is empty or longer than 1024 characters",
        ));
    }
    if body.is_empty() {
        return Err(fail("the body is empty"));
    }
    Ok(Skill {
        name,
        description,
        body: body.to_owned(),
    })
}

/// `^[a-z0-9]+(-[a-z0-9]+)*$`, at most [`MAX_NAME_CHARS`] long.
fn valid_name(name: &str) -> bool {
    name.len() <= MAX_NAME_CHARS
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

/// Every bundled skill, parsed once.
///
/// # Panics
/// A bundled skill is malformed. A unit test parses every one, so this is a build defect, not
/// a runtime condition.
#[must_use]
pub fn bundled() -> &'static [Skill] {
    static PARSED: OnceLock<Vec<Skill>> = OnceLock::new();
    PARSED.get_or_init(|| {
        SKILLS
            .iter()
            .map(|(directory, source)| parse(directory, source))
            .collect::<Result<_, _>>()
            .expect("the bundled skills parse")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}")
    }

    #[test]
    fn every_bundled_skill_meets_spec() {
        assert!(!SKILLS.is_empty());
        let skills = bundled();
        assert_eq!(skills.len(), SKILLS.len());
        for (skill, (directory, _)) in skills.iter().zip(SKILLS) {
            assert_eq!(skill.name, *directory);
            assert!((1..=MAX_NAME_CHARS).contains(&skill.name.chars().count()));
            assert!(
                skill.name.split('-').all(|part| !part.is_empty()
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())),
                "{}",
                skill.name
            );
            assert!(
                (1..=MAX_DESCRIPTION_CHARS).contains(&skill.description.chars().count()),
                "{}",
                skill.name
            );
            assert!(!skill.body.trim().is_empty());
            assert!(!skill.body.starts_with("---"));
        }
        let packages = skills
            .iter()
            .find(|skill| skill.name == "teamarness-packages")
            .expect("teamarness-packages ships");
        assert!(packages.body.contains("__runtime_guide"));
        assert!(packages.body.contains("agent.tool_outcome_unknown"));
    }

    #[test]
    fn frontmatter_reader_reads_name_description_and_body() {
        let skill = parse(
            "demo",
            &source("demo", "  Does a thing.  ", "\n# Demo\n\nBody text.\n"),
        )
        .unwrap();
        assert_eq!(
            skill,
            Skill {
                name: "demo".into(),
                description: "Does a thing.".into(),
                body: "# Demo\n\nBody text.".into(),
            }
        );
        // Keys may come in either order; a description may hold colons.
        let swapped = "---\ndescription: Use it: always.\nname: demo\n---\nBody";
        assert_eq!(
            parse("demo", swapped).unwrap().description,
            "Use it: always."
        );
        // A `---` inside the body is body text.
        let ruled = source("demo", "D.", "One\n---\nTwo");
        assert_eq!(parse("demo", &ruled).unwrap().body, "One\n---\nTwo");
    }

    #[test]
    fn frontmatter_reader_rejects_malformed() {
        let long_description = "d".repeat(MAX_DESCRIPTION_CHARS + 1);
        let long_name = "a".repeat(MAX_NAME_CHARS + 1);
        for (directory, text) in [
            // No fence, an unclosed fence, or a fence not on the first line.
            ("demo", "name: demo\ndescription: D.\n\nBody".to_owned()),
            ("demo", "---\nname: demo\ndescription: D.\nBody".to_owned()),
            ("demo", format!("\n{}", source("demo", "D.", "Body"))),
            // A missing, repeated, unknown or multi-line key.
            ("demo", "---\nname: demo\n---\nBody".to_owned()),
            ("demo", "---\ndescription: D.\n---\nBody".to_owned()),
            (
                "demo",
                "---\nname: demo\nname: demo\ndescription: D.\n---\nBody".to_owned(),
            ),
            (
                "demo",
                "---\nname: demo\ndescription: D.\nlicense: MIT\n---\nBody".to_owned(),
            ),
            (
                "demo",
                "---\nname: demo\ndescription: >\n  Folded.\n---\nBody".to_owned(),
            ),
            (
                "demo",
                "---\nname: demo\ndescription: D.\n  continued\n---\nBody".to_owned(),
            ),
            (
                "demo",
                "---\nname demo\ndescription: D.\n---\nBody".to_owned(),
            ),
            // Names the specification refuses.
            ("Demo", source("Demo", "D.", "Body")),
            ("-demo", source("-demo", "D.", "Body")),
            ("demo-", source("demo-", "D.", "Body")),
            ("de--mo", source("de--mo", "D.", "Body")),
            ("de_mo", source("de_mo", "D.", "Body")),
            ("", source("", "D.", "Body")),
            (long_name.as_str(), source(&long_name, "D.", "Body")),
            // An empty or overlong description, or an empty body.
            ("demo", source("demo", "", "Body")),
            ("demo", source("demo", "   ", "Body")),
            ("demo", source("demo", &long_description, "Body")),
            ("demo", source("demo", "D.", "")),
            ("demo", source("demo", "D.", " \n\n ")),
        ] {
            assert!(parse(directory, &text).is_err(), "{directory}: {text:.80}");
        }
        // The bounds themselves are accepted.
        let longest = "a".repeat(MAX_NAME_CHARS);
        let widest = "d".repeat(MAX_DESCRIPTION_CHARS);
        assert!(parse(&longest, &source(&longest, &widest, "Body")).is_ok());
        assert!(parse("a1-b2", &source("a1-b2", "D.", "Body")).is_ok());
    }

    #[test]
    fn name_must_match_directory() {
        assert!(parse("other", &source("demo", "D.", "Body")).is_err());
        assert!(parse("demo", &source("demo", "D.", "Body")).is_ok());
    }
}
