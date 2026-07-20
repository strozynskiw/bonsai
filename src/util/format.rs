//! Small shared formatting helpers.

/// Render a token count compactly: `1_234` becomes `"1.2k"`, anything under a
/// thousand stays a plain integer string. Shared by every surface that shows a
/// token budget (compaction/episode status, model-switch warnings, `/smol`, and
/// the settings screen) so they all read consistently.
pub(crate) fn format_tokens_k(tokens: usize) -> String {
    if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}
