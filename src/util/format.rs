//! Small shared formatting helpers.

/// Render a token count compactly: `1_234` becomes `"1.2k"`, anything under a
/// thousand stays a plain integer string. Shared by every surface that shows a
/// token budget (compaction/episode status, model-switch warnings, `/smol`, and
/// the settings screen) so they all read consistently.
pub(crate) fn format_tokens_k(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        let millions = format!("{:.1}", tokens as f64 / 1_000_000.0);
        format!("{}M", millions.trim_end_matches(".0"))
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::format_tokens_k;

    #[test]
    fn million_scale_counts_use_m_suffix() {
        assert_eq!(format_tokens_k(999), "999");
        assert_eq!(format_tokens_k(8_200), "8.2k");
        assert_eq!(format_tokens_k(1_000_000), "1M");
        assert_eq!(format_tokens_k(1_250_000), "1.2M");
    }
}
