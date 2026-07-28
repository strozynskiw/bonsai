//! `${VAR}` expansion against the process environment. Shared by the MCP
//! client (stdio env / HTTP headers) and the hooks engine (HTTP action
//! headers), so config-authored secrets are referenced the same way
//! everywhere rather than each caller reinventing the syntax.

/// Expand `${VAR}` references against the process environment. Unset
/// variables expand to empty — the same "don't fail the whole config load"
/// posture as the rest of `src/config`; a caller that truly needs the secret
/// fails downstream instead (a connection handshake, an HTTP 401), which
/// surfaces as a normal `Failed` status rather than a load-time crash.
pub(crate) fn expand_env_vars(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next();
            let mut name = String::new();
            for next in chars.by_ref() {
                if next == '}' {
                    break;
                }
                name.push(next);
            }
            if let Ok(value) = std::env::var(&name) {
                result.push_str(&value);
            }
        } else {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_known_vars_and_blanks_unknown_ones() {
        crate::util::test_env::with_var("BONSAI_UTIL_ENV_TEST_VAR", Some("secret"), || {
            assert_eq!(
                expand_env_vars("Bearer ${BONSAI_UTIL_ENV_TEST_VAR}"),
                "Bearer secret"
            );
            assert_eq!(expand_env_vars("${BONSAI_UTIL_ENV_TEST_VAR_UNSET}"), "");
            assert_eq!(expand_env_vars("no vars here"), "no vars here");
        });
    }
}
