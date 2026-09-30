//! Small shared helpers.

/// Lowercase ASCII slug: runs of non-alphanumerics collapse to a single `-`,
/// trimmed at both ends and capped at 60 chars. `fallback` is used when the
/// result would otherwise be empty (for example a title of `"!!!"`).
pub fn slugify(title: &str, fallback: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
        if out.len() >= 60 {
            break;
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() {
        fallback.to_string()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugifies() {
        assert_eq!(
            slugify("Accept image files & URLs!", "x"),
            "accept-image-files-urls"
        );
        assert_eq!(slugify("Hello__World", "x"), "hello-world");
        assert_eq!(slugify("", "plan"), "plan");
        assert_eq!(slugify("!!!", "plan"), "plan");
    }
}
