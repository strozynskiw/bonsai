//! Shared kebab-case slug helper, used for resource filenames and frontmatter
//! `name` fields (agent composer, memory entries).

/// Lower-case, dash-separated slug — reused for both the filename and the
/// frontmatter `name`. Same rules as the local-model-wizard provider slug.
pub(crate) fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash && !slug.is_empty() {
            slug.push('-');
            previous_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_normalizes() {
        assert_eq!(slugify("API Explorer!"), "api-explorer");
        assert_eq!(slugify("  Foo   Bar  "), "foo-bar");
        assert_eq!(slugify("already-good"), "already-good");
        assert_eq!(slugify("!!!"), "");
    }
}
