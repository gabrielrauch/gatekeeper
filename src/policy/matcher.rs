use crate::config::MatchRule;

/// Returns true if `path` matches `pattern`.
///
/// Rules:
/// - Split both by `/`, filter empty segments.
/// - Walk segments:
///   - A literal segment requires an exact match.
///   - `*` as the **last** pattern segment matches one or more remaining path segments.
///   - `*` as a **non-last** pattern segment matches exactly one path segment.
/// - Both arrays must be exhausted for a match (unless a trailing `*` already returned true).
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let pat_segs: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let path_segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    let mut pi = 0usize; // index into path_segs
    let pat_len = pat_segs.len();
    let path_len = path_segs.len();

    for (i, seg) in pat_segs.iter().enumerate() {
        let is_last_pat = i == pat_len - 1;

        if *seg == "*" {
            if is_last_pat {
                // Trailing wildcard: must match at least one remaining segment.
                return pi < path_len;
            } else {
                // Mid wildcard: consume exactly one path segment.
                if pi >= path_len {
                    return false;
                }
                pi += 1;
            }
        } else {
            // Literal: must match exactly.
            if pi >= path_len || path_segs[pi] != *seg {
                return false;
            }
            pi += 1;
        }
    }

    // All pattern segments consumed — path must also be exhausted.
    pi == path_len
}

/// Returns true if `rule` matches the given request path and method.
pub fn policy_matches(rule: &MatchRule, request_path: &str, request_method: &str) -> bool {
    if !path_matches(&rule.path, request_path) {
        return false;
    }
    if let Some(ref methods) = rule.methods {
        let method_upper = request_method.to_uppercase();
        return methods.iter().any(|m| m.to_uppercase() == method_upper);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MatchRule;

    // --- path_matches tests ---

    #[test]
    fn exact_match() {
        assert!(path_matches("/api/v1/users", "/api/v1/users"));
    }

    #[test]
    fn trailing_wildcard_one_segment() {
        assert!(path_matches("/api/*", "/api/users"));
    }

    #[test]
    fn trailing_wildcard_multiple_segments() {
        assert!(path_matches("/api/*", "/api/users/123/profile"));
    }

    #[test]
    fn trailing_wildcard_requires_at_least_one_segment() {
        // The trailing `*` must match at least one segment.
        assert!(!path_matches("/api/*", "/api"));
        assert!(!path_matches("/api/*", "/api/"));
    }

    #[test]
    fn mid_wildcard_matches_exactly_one_segment() {
        assert!(path_matches("/api/*/users", "/api/v1/users"));
        assert!(!path_matches("/api/*/users", "/api/v1/v2/users"));
    }

    #[test]
    fn different_prefix_no_match() {
        assert!(!path_matches("/api/*", "/v2/users"));
    }

    #[test]
    fn root_matches_root() {
        assert!(path_matches("/", "/"));
    }

    #[test]
    fn root_does_not_match_non_root() {
        assert!(!path_matches("/", "/api"));
    }

    #[test]
    fn healthz_exact() {
        assert!(path_matches("/healthz", "/healthz"));
        assert!(!path_matches("/healthz", "/healthz/extra"));
    }

    // --- policy_matches tests ---

    fn make_rule(path: &str, methods: Option<Vec<&str>>) -> MatchRule {
        MatchRule {
            path: path.to_string(),
            methods: methods.map(|ms| ms.iter().map(|m| m.to_string()).collect()),
        }
    }

    #[test]
    fn policy_matches_with_methods() {
        let rule = make_rule("/api/*", Some(vec!["GET", "POST"]));
        assert!(policy_matches(&rule, "/api/users", "GET"));
        assert!(policy_matches(&rule, "/api/users", "POST"));
        assert!(!policy_matches(&rule, "/api/users", "DELETE"));
    }

    #[test]
    fn policy_matches_any_method() {
        let rule = make_rule("/api/*", None);
        assert!(policy_matches(&rule, "/api/users", "GET"));
        assert!(policy_matches(&rule, "/api/users", "DELETE"));
        assert!(policy_matches(&rule, "/api/users", "PATCH"));
    }

    #[test]
    fn policy_no_match_wrong_path() {
        let rule = make_rule("/api/*", None);
        assert!(!policy_matches(&rule, "/v2/users", "GET"));
    }

    #[test]
    fn policy_matches_method_case_insensitive() {
        let rule = make_rule("/api/*", Some(vec!["get"]));
        assert!(policy_matches(&rule, "/api/users", "GET"));
    }
}
