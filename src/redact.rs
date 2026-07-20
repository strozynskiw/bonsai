//! Secret redaction.
//!
//! A single source of truth for detecting credential-shaped strings and masking
//! them before they reach a place they could leak from: the model context, the
//! TUI transcript, `/ctx`, logs, and the persisted session store. Post-tool hooks
//! receive only a redacted result excerpt, and the fully composed output is redacted
//! at the final run-loop fan-out so hook-added context is masked too; pasted text is
//! screened at the composer so a live credential is refused before it enters the buffer.
//!
//! All patterns are combined into one regex so redaction is a single linear pass
//! with no look-around (the `regex` crate's linear-time guarantee). When nothing
//! matches, [`redact`] returns the input borrowed — the zero-allocation common
//! case for the overwhelming majority of tool output.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::{Captures, Regex};

/// The class of credential a match represents. Carried out of [`first_secret`]
/// so the paste-refusal notice can name what it caught.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecretKind {
    GitHubToken,
    GitHubPat,
    GitLabToken,
    SlackToken,
    AwsAccessKey,
    AnthropicKey,
    OpenAiKey,
    GoogleApiKey,
    PrivateKey,
    Jwt,
    BearerToken,
    UrlCredentials,
}

impl SecretKind {
    /// Human-readable label shown in the `[REDACTED:…]` marker and the paste notice.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::GitHubToken => "GitHub token",
            Self::GitHubPat => "GitHub PAT",
            Self::GitLabToken => "GitLab token",
            Self::SlackToken => "Slack token",
            Self::AwsAccessKey => "AWS access key",
            Self::AnthropicKey => "Anthropic API key",
            Self::OpenAiKey => "OpenAI API key",
            Self::GoogleApiKey => "Google API key",
            Self::PrivateKey => "private key",
            Self::Jwt => "JWT",
            Self::BearerToken => "bearer token",
            Self::UrlCredentials => "URL credentials",
        }
    }
}

/// One credential pattern. `group` is the named capture used to recognise which
/// alternative of the combined regex matched. `refuse_paste` marks high-confidence
/// shapes that should block a paste outright — lower-confidence shapes (a bare
/// `Bearer …`, inline URL credentials) are still redacted but never refuse a paste,
/// to avoid blocking legitimate input on a loose match.
struct Spec {
    kind: SecretKind,
    group: &'static str,
    pattern: &'static str,
    refuse_paste: bool,
}

/// Order matters: alternatives are tried left-to-right, so a more specific shape
/// (`sk-ant-…`) must precede a broader one (`sk-…`) that could also match it.
static SPECS: &[Spec] = &[
    Spec {
        kind: SecretKind::GitHubToken,
        group: "github",
        pattern: r"gh[pousr]_[A-Za-z0-9]{36,}",
        refuse_paste: true,
    },
    Spec {
        kind: SecretKind::GitHubPat,
        group: "githubpat",
        pattern: r"github_pat_[A-Za-z0-9_]{22,}",
        refuse_paste: true,
    },
    Spec {
        kind: SecretKind::GitLabToken,
        group: "gitlab",
        pattern: r"glpat-[A-Za-z0-9_-]{20,}",
        refuse_paste: true,
    },
    Spec {
        kind: SecretKind::SlackToken,
        group: "slack",
        pattern: r"xox[baprs]-[A-Za-z0-9-]{10,}",
        refuse_paste: true,
    },
    Spec {
        kind: SecretKind::AwsAccessKey,
        group: "aws",
        pattern: r"(?:AKIA|ASIA)[0-9A-Z]{16}",
        refuse_paste: true,
    },
    Spec {
        kind: SecretKind::AnthropicKey,
        group: "anthropic",
        pattern: r"sk-ant-[A-Za-z0-9_-]{20,}",
        refuse_paste: true,
    },
    Spec {
        kind: SecretKind::OpenAiKey,
        group: "openai",
        pattern: r"sk-(?:proj-)?[A-Za-z0-9_-]{32,}",
        refuse_paste: true,
    },
    Spec {
        kind: SecretKind::GoogleApiKey,
        group: "google",
        pattern: r"AIza[0-9A-Za-z_-]{35}",
        refuse_paste: true,
    },
    Spec {
        kind: SecretKind::PrivateKey,
        group: "privatekey",
        pattern: r"-----BEGIN (?:[A-Z]+ )*PRIVATE KEY-----[\s\S]*?-----END (?:[A-Z]+ )*PRIVATE KEY-----",
        refuse_paste: true,
    },
    Spec {
        kind: SecretKind::Jwt,
        group: "jwt",
        pattern: r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
        refuse_paste: true,
    },
    Spec {
        kind: SecretKind::BearerToken,
        group: "bearer",
        pattern: r"(?i:bearer)\s+[A-Za-z0-9._~+/-]{12,}=*",
        refuse_paste: false,
    },
    Spec {
        kind: SecretKind::UrlCredentials,
        group: "urlcreds",
        pattern: r"://[A-Za-z0-9._%+-]+:[^/\s:@]{3,}@",
        refuse_paste: false,
    },
];

static COMBINED: LazyLock<Regex> = LazyLock::new(|| {
    let alternation = SPECS
        .iter()
        .map(|spec| format!("(?P<{}>{})", spec.group, spec.pattern))
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&alternation).expect("combined secret regex is valid")
});

/// The spec whose named group participated in this match. Exactly one top-level
/// alternative matches per match, so exactly one group is present.
fn matched_spec(caps: &Captures<'_>) -> &'static Spec {
    SPECS
        .iter()
        .find(|spec| caps.name(spec.group).is_some())
        .expect("a named alternative matched")
}

/// The replacement text for a given match. Most kinds collapse to a `[REDACTED:…]`
/// marker; URL credentials and bearer tokens keep their structural surroundings so
/// the redacted form stays readable (`scheme://[REDACTED]@host`, `Bearer [REDACTED]`).
fn replacement(spec: &Spec) -> Cow<'static, str> {
    match spec.kind {
        SecretKind::UrlCredentials => Cow::Borrowed("://[REDACTED]@"),
        SecretKind::BearerToken => Cow::Borrowed("Bearer [REDACTED]"),
        kind => Cow::Owned(format!("[REDACTED:{}]", kind.label())),
    }
}

/// Mask every credential-shaped substring in `input`. Returns the input borrowed
/// (no allocation) when nothing matches.
pub(crate) fn redact(input: &str) -> Cow<'_, str> {
    COMBINED.replace_all(input, |caps: &Captures<'_>| {
        replacement(matched_spec(caps)).into_owned()
    })
}

/// Redact in place, only reallocating the `String` when something was masked.
pub(crate) fn redact_in_place(text: &mut String) {
    if let Cow::Owned(redacted) = redact(text) {
        *text = redacted;
    }
}

/// The kind of the first high-confidence credential in `input`, if any. Used to
/// refuse a paste; deliberately ignores low-confidence shapes so legitimate input
/// is never blocked on a loose match.
pub(crate) fn first_secret(input: &str) -> Option<SecretKind> {
    COMBINED
        .captures_iter(input)
        .map(|caps| matched_spec(&caps))
        .find(|spec| spec.refuse_paste)
        .map(|spec| spec.kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_clean_text_borrowed() {
        let input = "just some ordinary tool output with no secrets in it";
        let out = redact(input);
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out, input);
    }

    #[test]
    fn redacts_github_token() {
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4)); // 4 + 40 chars
        let line = format!("token={token} done");
        assert_eq!(redact(&line), "token=[REDACTED:GitHub token] done");
    }

    #[test]
    fn redacts_each_provider_key_distinctly() {
        let cases = [
            (
                "github_pat_abcdefghijklmnopqrstuvwxyz0123",
                "[REDACTED:GitHub PAT]",
            ),
            ("glpat-abcdefghijklmnopqrstuvwx", "[REDACTED:GitLab token]"),
            ("xoxb-1234567890-abcdefghij", "[REDACTED:Slack token]"),
            ("AKIAIOSFODNN7EXAMPLE", "[REDACTED:AWS access key]"),
            (
                "AIzaSyA1234567890abcdefghijklmnopqrstuv",
                "[REDACTED:Google API key]",
            ),
        ];
        for (secret, expected) in cases {
            assert_eq!(redact(secret), expected, "failed for {secret}");
        }
    }

    #[test]
    fn anthropic_key_is_not_misread_as_openai() {
        let key = format!("sk-ant-{}", "ABCdef0123".repeat(3)); // > 20 after prefix
        assert_eq!(redact(&key), "[REDACTED:Anthropic API key]");
    }

    #[test]
    fn redacts_openai_key() {
        let key = format!("sk-{}", "ABCdef0123".repeat(4)); // 40 chars after sk-
        assert_eq!(redact(&key), "[REDACTED:OpenAI API key]");
    }

    #[test]
    fn redacts_bearer_token_keeping_scheme() {
        let out = redact("Authorization: Bearer abcDEF123456ghiJKL789");
        assert_eq!(out, "Authorization: Bearer [REDACTED]");
    }

    #[test]
    fn redacts_url_credentials_keeping_host() {
        let out = redact("clone https://alice:s3cr3tpw@example.com/repo.git now");
        assert_eq!(out, "clone https://[REDACTED]@example.com/repo.git now");
    }

    #[test]
    fn redacts_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N";
        assert_eq!(redact(jwt), "[REDACTED:JWT]");
    }

    #[test]
    fn redacts_private_key_block() {
        let block = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\nabcd\n-----END RSA PRIVATE KEY-----";
        assert_eq!(redact(block), "[REDACTED:private key]");
    }

    #[test]
    fn redaction_is_idempotent() {
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        let once = redact(&token).into_owned();
        let twice = redact(&once);
        assert_eq!(twice, once);
        assert!(matches!(twice, Cow::Borrowed(_)));
    }

    #[test]
    fn redact_in_place_only_allocates_on_match() {
        let mut clean = "nothing to see here".to_string();
        let ptr_before = clean.as_ptr();
        redact_in_place(&mut clean);
        assert_eq!(clean.as_ptr(), ptr_before, "clean text must not reallocate");

        let mut dirty = format!("key=ghp_{}", "a1B2c3D4e5".repeat(4));
        redact_in_place(&mut dirty);
        assert_eq!(dirty, "key=[REDACTED:GitHub token]");
    }

    #[test]
    fn first_secret_flags_high_confidence_only() {
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        assert_eq!(first_secret(&token), Some(SecretKind::GitHubToken));

        // A bare bearer token is redacted but must not refuse a paste.
        assert_eq!(first_secret("Bearer abcDEF123456ghiJKL789"), None);
        // Inline URL credentials likewise.
        assert_eq!(first_secret("https://a:bcd@h.com"), None);
        // Ordinary prose is clean.
        assert_eq!(first_secret("please read the config file"), None);
    }

    #[test]
    fn redacts_multiple_secrets_in_one_string() {
        let gh = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        let line = format!("first {gh} then AKIAIOSFODNN7EXAMPLE end");
        assert_eq!(
            redact(&line),
            "first [REDACTED:GitHub token] then [REDACTED:AWS access key] end"
        );
    }
}
