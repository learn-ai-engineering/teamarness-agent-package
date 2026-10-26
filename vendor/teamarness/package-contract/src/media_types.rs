//! The shared content rules of `teamarness.content-views` 1.0: the extension → media type
//! allowlist, the content path rules and the view bounds.
//!
//! The Package Host applies these rules when a Package opens a content view and when it serves a
//! view's bytes. A Package that stores content for a view (the Exhibits Package does) applies the
//! same code at upload time, so it refuses a file with the verdict the Host would give.
//!
//! The media type of a served file comes from this table only, never from the Package. An
//! extension matches **lowercase only**: `image.png` is served as `image/png`, and `image.PNG`
//! is refused as an unsupported media type.

/// The most files one content view may list.
pub const MAX_CONTENT_FILES: usize = 1_000;
/// The largest declared total size of one content view's files, in bytes (64 MiB).
pub const MAX_CONTENT_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// The largest size of one content file, in bytes (16 MiB).
pub const MAX_CONTENT_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// The longest content path, in UTF-8 bytes.
pub const MAX_CONTENT_PATH_BYTES: usize = 128;

/// Every allowed extension and the fixed media type it is served with, in a stable order.
pub const MEDIA_TYPES: &[(&str, &str)] = &[
    ("html", "text/html; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("js", "text/javascript; charset=utf-8"),
    ("mjs", "text/javascript; charset=utf-8"),
    ("json", "application/json"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("ico", "image/x-icon"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("wasm", "application/wasm"),
    ("txt", "text/plain; charset=utf-8"),
    ("md", "text/markdown; charset=utf-8"),
    ("csv", "text/csv; charset=utf-8"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("mp3", "audio/mpeg"),
];

/// The media type of one allowed extension (without the dot), or `None` when the extension is
/// not on the allowlist. Matching is exact and case-sensitive: only lowercase extensions match.
#[must_use]
pub fn media_type_for_extension(extension: &str) -> Option<&'static str> {
    MEDIA_TYPES
        .iter()
        .find(|(allowed, _)| *allowed == extension)
        .map(|(_, media_type)| *media_type)
}

/// The media type of a content path, from the extension of its last segment.
///
/// A path whose last segment has no extension (`LICENSE`, `.hidden`), or whose extension is not
/// on the allowlist, has no media type.
#[must_use]
pub fn media_type_for_path(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    media_type_for_extension(extension)
}

/// Whether a value is a normalized relative path: non-empty `/`-separated segments, none of
/// them `.` or `..`, and no `\`, NUL, `%`, `?` or `#`.
///
/// These are the segment rules the Host already applies to admitted UI resource paths.
#[must_use]
pub fn is_normalized_relative_path(value: &str) -> bool {
    !(value.is_empty()
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains("//")
        || value.contains('\\')
        || value.contains('\0')
        || value.contains('%')
        || value.contains('?')
        || value.contains('#')
        || value
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == ".."))
}

/// Whether a value is a valid content path: a normalized relative path of at most
/// [`MAX_CONTENT_PATH_BYTES`] bytes with no control characters.
///
/// A content path appears in a `teamarness-content:` URL, so control characters are refused as
/// well. The extension is checked separately by [`media_type_for_path`].
#[must_use]
pub fn is_valid_content_path(value: &str) -> bool {
    value.len() <= MAX_CONTENT_PATH_BYTES
        && is_normalized_relative_path(value)
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_allowlisted_extension_maps_to_its_fixed_media_type() {
        let expected = [
            ("html", "text/html; charset=utf-8"),
            ("css", "text/css; charset=utf-8"),
            ("js", "text/javascript; charset=utf-8"),
            ("mjs", "text/javascript; charset=utf-8"),
            ("json", "application/json"),
            ("svg", "image/svg+xml"),
            ("png", "image/png"),
            ("jpg", "image/jpeg"),
            ("gif", "image/gif"),
            ("webp", "image/webp"),
            ("avif", "image/avif"),
            ("ico", "image/x-icon"),
            ("woff", "font/woff"),
            ("woff2", "font/woff2"),
            ("ttf", "font/ttf"),
            ("otf", "font/otf"),
            ("wasm", "application/wasm"),
            ("txt", "text/plain; charset=utf-8"),
            ("md", "text/markdown; charset=utf-8"),
            ("csv", "text/csv; charset=utf-8"),
            ("mp4", "video/mp4"),
            ("webm", "video/webm"),
            ("mp3", "audio/mpeg"),
        ];
        assert_eq!(MEDIA_TYPES.len(), expected.len());
        for (extension, media_type) in expected {
            assert_eq!(
                media_type_for_extension(extension),
                Some(media_type),
                "{extension}"
            );
            assert_eq!(
                media_type_for_path(&format!("assets/file.{extension}")),
                Some(media_type)
            );
            mime_parses(media_type);
        }
    }

    fn mime_parses(media_type: &str) {
        assert!(
            media_type.parse::<mime::Mime>().is_ok(),
            "{media_type} is a MIME type"
        );
    }

    #[test]
    fn extensions_match_lowercase_only() {
        for refused in ["PNG", "Png", "HTML", "Js", "WASM", "jpeg", "htm", "exe", ""] {
            assert_eq!(media_type_for_extension(refused), None, "{refused}");
        }
        assert_eq!(media_type_for_path("image.PNG"), None);
        assert_eq!(media_type_for_path("index.Html"), None);
        assert_eq!(media_type_for_path("image.png"), Some("image/png"));
    }

    #[test]
    fn paths_without_an_allowed_extension_have_no_media_type() {
        for path in [
            "LICENSE",
            ".png",
            "assets/.png",
            "assets.png/readme",
            "archive.tar.gz",
            "page.html.",
        ] {
            assert_eq!(media_type_for_path(path), None, "{path}");
        }
        assert_eq!(
            media_type_for_path("a.b.css"),
            Some("text/css; charset=utf-8")
        );
        assert_eq!(
            media_type_for_path("pages/page2.html"),
            Some("text/html; charset=utf-8")
        );
    }

    #[test]
    fn content_paths_follow_the_resource_segment_rules_and_bound() {
        for valid in [
            "index.html",
            "pages/page2.html",
            "assets/fonts/a b.woff2",
            "ünïcode/ok.txt",
        ] {
            assert!(is_valid_content_path(valid), "{valid}");
        }
        for invalid in [
            "",
            "/index.html",
            "pages/",
            "pages//a.html",
            "./a.html",
            "a/../b.html",
            "..",
            "a\\b.html",
            "a%2fb.html",
            "a?.html",
            "a#.html",
            "a\0.html",
            "a\n.html",
        ] {
            assert!(!is_valid_content_path(invalid), "{invalid:?}");
        }
        let longest = format!("{}.txt", "a".repeat(MAX_CONTENT_PATH_BYTES - 4));
        assert!(is_valid_content_path(&longest));
        let too_long = format!("{}.txt", "a".repeat(MAX_CONTENT_PATH_BYTES - 3));
        assert!(!is_valid_content_path(&too_long));
    }

    #[test]
    fn bounds_are_the_published_contract_values() {
        assert_eq!(MAX_CONTENT_FILES, 1000);
        assert_eq!(MAX_CONTENT_TOTAL_BYTES, 67_108_864);
        assert_eq!(MAX_CONTENT_FILE_BYTES, 16_777_216);
        assert_eq!(MAX_CONTENT_PATH_BYTES, 128);
    }
}
