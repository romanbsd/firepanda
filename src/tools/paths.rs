//! Path-prefix filters for map results. They trim returned links only.

use super::origin;

/// Path of a normalized URL, with the query removed.
///
/// `https://example.com/a?b` yields `/a`. `https://example.com/` yields `/`.
pub fn url_path(normalized: &str) -> &str {
    let rest = &normalized[origin(normalized).len()..];
    let path = if rest.is_empty() { "/" } else { rest };
    path.split_once('?').map_or(path, |(before, _)| before)
}

/// A prefix matches on a segment boundary: the path is equal to it, continues
/// with `/`, or the prefix already ends in `/` and the path continues past it.
fn prefix_matches(path: &str, prefix: &str) -> bool {
    let Some(rest) = path.strip_prefix(prefix) else {
        return false;
    };
    rest.is_empty() || prefix.ends_with('/') || rest.starts_with('/')
}

/// Keep `path` when `include` is empty or a prefix matches, and no exclude matches.
pub fn path_allowed(path: &str, include: &[String], exclude: &[String]) -> bool {
    let included = include.is_empty() || include.iter().any(|prefix| prefix_matches(path, prefix));
    included && !exclude.iter().any(|prefix| prefix_matches(path, prefix))
}

#[cfg(test)]
mod tests {
    use super::{path_allowed, url_path};

    #[test]
    fn segment_boundary_and_exclude_wins() {
        let careers = vec!["/careers".to_string()];
        let draft = vec!["/careers/draft".to_string()];
        assert!(path_allowed("/careers", &careers, &[]));
        assert!(path_allowed("/careers/role", &careers, &[]));
        assert!(!path_allowed("/careers-old", &careers, &[]));
        assert!(path_allowed("/careers/", &["/careers/".into()], &[]));
        assert!(!path_allowed("/careers", &["/careers/".into()], &[]));
        // A prefix that already ends in `/` still matches its descendants.
        assert!(path_allowed("/careers/role", &["/careers/".into()], &[]));
        assert!(!path_allowed("/careers/draft", &careers, &draft));
        assert!(path_allowed("/careers/role", &careers, &draft));
        assert!(path_allowed("/about", &[], &[]));
        assert!(!path_allowed("/about", &[], &["/about".into()]));
        assert!(!path_allowed("/about", &careers, &[]));
    }

    #[test]
    fn url_path_drops_the_query() {
        assert_eq!(url_path("https://example.com/careers?x=1"), "/careers");
        assert_eq!(url_path("https://example.com/"), "/");
    }
}
