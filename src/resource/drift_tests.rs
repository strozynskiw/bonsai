//! Doc-vs-binary drift tests for public documentation and embedded product
//! skills.
//!
//! `customize` and `provider-setup` teach exact file formats and commands;
//! the topic documentation and website promise exact flags, settings, release
//! versions, and platforms. These tests parse those claims through the
//! registries and build surfaces that own them, so a product change fails CI
//! until its public documentation changes with it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::config::schema::ConfigFile;
use crate::resource::builtin::BUILTIN_SKILL_SOURCES;
use crate::resource::frontmatter::{AgentFrontmatter, SkillFrontmatter, parse_frontmatter};

const README: &str = include_str!("../../README.md");
const GETTING_STARTED: &str = include_str!("../../docs/getting-started.md");
const HEADLESS_DOCS: &str = include_str!("../../docs/headless.md");
const SLASH_COMMAND_DOCS: &str = include_str!("../../docs/slash-commands.md");
const SETTINGS_SOURCE: &str = include_str!("../tui/settings.rs");
const INSTALLER: &str = include_str!("../../install.sh");
const RELEASE_SCRIPT: &str = include_str!("../../scripts/release.sh");
const RELEASE_WORKFLOW: &str = include_str!("../../.github/workflows/release.yml");
const SITE: &str = include_str!("../../site/index.html");
const CONNECTIONS: &str = include_str!("../../models/builtin/connections.toml");
const PROVIDERS_DOCS: &str = include_str!("../../docs/providers.md");

/// A tagged fenced example: which skill it came from, its `bonsai:*` tag, and
/// the fence body.
struct TaggedExample {
    skill_path: &'static str,
    tag: String,
    body: String,
}

/// Extract every fenced block whose info string carries a `bonsai:<kind>` tag
/// from every embedded skill. Untagged fences are ignored.
fn tagged_examples() -> Vec<TaggedExample> {
    let mut examples = Vec::new();
    for source in BUILTIN_SKILL_SOURCES {
        let mut current: Option<(String, String)> = None;
        for line in source.content.lines() {
            if current.is_some() {
                if line.trim_end() == "```" {
                    let (info, body) = current.take().expect("fence state checked above");
                    if let Some(tag) = info
                        .split_whitespace()
                        .find(|token| token.starts_with("bonsai:"))
                    {
                        examples.push(TaggedExample {
                            skill_path: source.path,
                            tag: tag.to_string(),
                            body,
                        });
                    }
                } else if let Some((_, body)) = current.as_mut() {
                    body.push_str(line);
                    body.push('\n');
                }
            } else if let Some(info) = line.strip_prefix("```") {
                current = Some((info.trim().to_string(), String::new()));
            }
        }
        assert!(
            current.is_none(),
            "unterminated code fence in {}",
            source.path
        );
    }
    examples
}

/// The top-level `config.toml` keys bonsai understands — mirror of the known
/// set in `config/mod.rs`. `ConfigFile`'s derive ignores unknown keys, so
/// without this check a typo'd section name in an example would parse as an
/// empty (vacuously valid) config.
const KNOWN_CONFIG_KEYS: &[&str] = &[
    "schema_version",
    "agent_read_isolation",
    "sandbox",
    "mcp",
    "hooks",
    "verification",
    "update",
];

#[test]
fn product_skill_examples_parse_through_real_schemas() {
    let temp = tempfile::TempDir::new().unwrap();
    let provider_dir = temp.path().join("providers");
    let model_dir = temp.path().join("models");
    std::fs::create_dir_all(&provider_dir).unwrap();
    std::fs::create_dir_all(&model_dir).unwrap();

    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, example) in tagged_examples().into_iter().enumerate() {
        let TaggedExample {
            skill_path,
            tag,
            body,
        } = example;
        let kind = match tag.as_str() {
            "bonsai:providers-file" => {
                std::fs::write(provider_dir.join(format!("example-{index}.toml")), &body).unwrap();
                "bonsai:providers-file"
            }
            "bonsai:models-file" => {
                std::fs::write(model_dir.join(format!("example-{index}.toml")), &body).unwrap();
                "bonsai:models-file"
            }
            "bonsai:config-file" => {
                toml::from_str::<ConfigFile>(&body).unwrap_or_else(|err| {
                    panic!("config example in {skill_path} does not parse: {err}")
                });
                let table: toml::Table = toml::from_str(&body).unwrap();
                for key in table.keys() {
                    assert!(
                        KNOWN_CONFIG_KEYS.contains(&key.as_str()),
                        "config example in {skill_path} uses unknown top-level key `{key}`"
                    );
                }
                "bonsai:config-file"
            }
            "bonsai:theme-file" => {
                crate::tui::theme::spec::parse_theme(
                    "driftcheck",
                    Path::new("driftcheck.toml"),
                    &body,
                )
                .unwrap_or_else(|err| {
                    panic!("theme example in {skill_path} does not parse: {err}")
                });
                "bonsai:theme-file"
            }
            "bonsai:skill-file" => {
                parse_frontmatter::<SkillFrontmatter>(&body).unwrap_or_else(|err| {
                    panic!("skill-file example in {skill_path} does not parse: {err}")
                });
                "bonsai:skill-file"
            }
            "bonsai:agent-file" => {
                parse_frontmatter::<AgentFrontmatter>(&body).unwrap_or_else(|err| {
                    panic!("agent-file example in {skill_path} does not parse: {err}")
                });
                "bonsai:agent-file"
            }
            other => panic!("unknown example tag `{other}` in {skill_path}"),
        };
        *seen.entry(kind).or_insert(0) += 1;
    }

    // Every documented example kind must actually be present — an edit that
    // drops a tag must not silently skip its validation.
    for expected in [
        "bonsai:providers-file",
        "bonsai:models-file",
        "bonsai:config-file",
        "bonsai:theme-file",
        "bonsai:skill-file",
        "bonsai:agent-file",
    ] {
        assert!(
            seen.get(expected).copied().unwrap_or(0) > 0,
            "no `{expected}` example found in any built-in skill"
        );
    }

    // The catalog examples must load against the real built-in catalog — the
    // same merge a user's ~/.bonsai/providers + ~/.bonsai/models goes through.
    let spec = crate::model_catalog::load_catalog_with_user_dirs(&provider_dir, &model_dir)
        .unwrap_or_else(|err| panic!("catalog examples do not load: {err}"));
    assert!(
        spec.connections
            .iter()
            .any(|connection| connection.id.as_str() == "ollama"),
        "the documented custom connection must materialize"
    );
    // The "override a built-in model" example must both name a target that
    // still exists and demonstrably patch it.
    let patched = spec
        .targets
        .iter()
        .find(|target| {
            target.connection.as_str() == "anthropic"
                && target.model.as_str() == "anthropic/claude-sonnet-5"
        })
        .expect("the documented builtin-patch example must target an existing builtin model");
    assert_eq!(
        patched.context_window,
        Some(400_000),
        "user patch must override the builtin's context window"
    );
}

#[test]
fn product_skill_command_mentions_exist() {
    let mut mentions = 0usize;
    for source in BUILTIN_SKILL_SOURCES {
        // Inline code spans are the odd segments when splitting on backticks;
        // fenced bodies also land on odd segments but start with their info
        // string (`toml …`/`markdown …`), never with `/`, so the filter below
        // skips them. A lone `/` span (prose about separators) is skipped too.
        for (segment_index, span) in source.content.split('`').enumerate() {
            if segment_index % 2 == 0 {
                continue;
            }
            let Some(rest) = span.strip_prefix('/') else {
                continue;
            };
            if !rest.starts_with(|c: char| c.is_ascii_lowercase()) {
                continue;
            }
            let token = span
                .split_whitespace()
                .next()
                .expect("span starts with a non-space char");
            assert!(
                crate::commands::COMMANDS
                    .iter()
                    .any(|command| command.name == token),
                "{} mentions `{token}`, which is not a registered command",
                source.path
            );
            mentions += 1;
        }
    }
    assert!(
        mentions >= 10,
        "expected the product skills to mention many commands, found {mentions} — \
         did the extraction break?"
    );
}

fn prose_inline_code(markdown: &str) -> Vec<String> {
    let mut prose = String::new();
    let mut in_fence = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    prose
        .split('`')
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .map(|(_, span)| span.to_string())
        .collect()
}

fn target_triples(source: &str) -> BTreeSet<String> {
    source
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        })
        .map(|token| token.trim_start_matches('-'))
        // The leading hyphen is load-bearing: it requires an arch prefix, so a
        // bare os suffix is not mistaken for a triple. `install.sh` builds its
        // triple from separate `os_part`/`arch_part` assignments, and without
        // this those assignments register as phantom `apple-darwin` /
        // `unknown-linux-gnu` targets that defeat the equality check below.
        // Matching any arch (rather than an allowlist) keeps a newly added
        // architecture visible to the drift check instead of silently ignored.
        .filter(|token| token.ends_with("-unknown-linux-gnu") || token.ends_with("-apple-darwin"))
        .map(str::to_string)
        .collect()
}

#[test]
fn public_install_surfaces_track_the_package_version() {
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    for (name, source) in [("install.sh", INSTALLER), ("site/index.html", SITE)] {
        assert!(
            source.contains(&tag),
            "{name} must reference the current package tag {tag}"
        );
    }

    assert!(
        GETTING_STARTED.contains("cargo install --git https://github.com/strozynskiw/bonsai.git")
    );
    // The repository is public; the private-repo SSH install form must stay out
    // of the README (HTTPS is the canonical clone URL for everyone).
    assert!(!README.contains("ssh://git@github.com"));
    assert!(!README.contains("Alpha install"));
    assert!(!README.contains("Release binaries (alpha)"));
    assert!(!SITE.contains("latest alpha"));
    assert!(!SITE.contains("alpha-tag"));

    for filename in ["site/index.html", "install.sh"] {
        assert!(
            RELEASE_SCRIPT.contains(filename),
            "release script must update {filename} when it bumps the version"
        );
    }
}

#[test]
fn headless_docs_options_cover_generated_help() {
    let help = crate::cli::help_text();
    let option_block = help
        .split_once("\n  Options:\n")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split_once("\n\n").map(|(block, _)| block))
        .expect("generated help has a headless options block");
    let help_options: BTreeSet<&str> = option_block
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|token| token.starts_with("--"))
        .collect();
    let documented_options: BTreeSet<String> = prose_inline_code(HEADLESS_DOCS)
        .into_iter()
        .filter_map(|span| {
            span.split_whitespace()
                .next()
                .filter(|token| token.starts_with("--"))
                .map(str::to_string)
        })
        .collect();

    assert!(help_options.len() >= 10, "headless option extraction broke");
    for option in help_options {
        assert!(
            documented_options.contains(option),
            "docs/headless.md does not document generated headless option `{option}`"
        );
    }
}

#[test]
fn settings_labels_match_the_settings_source() {
    let documented: BTreeSet<String> = SETTINGS_SOURCE
        .lines()
        .filter_map(|line| line.trim().strip_prefix("key: \"")?.strip_suffix("\","))
        .map(str::to_string)
        .collect();

    let app = crate::tui::test_utils::app();
    let profile = crate::smol::SmolProfile::resolve(crate::smol::SmolPreference::Off, 128_000);
    let rows = crate::tui::settings::seed_settings_rows(&app, profile);
    let actual: BTreeSet<String> = rows
        .iter()
        .filter_map(|row| match row {
            crate::tui::event::SettingsRow::Choice { key, .. }
            | crate::tui::event::SettingsRow::Action { key, .. } => Some((*key).to_string()),
            crate::tui::event::SettingsRow::Header(_) => None,
        })
        .collect();

    assert_eq!(documented, actual);
}

#[test]
fn slash_command_docs_mentions_resolve_to_registered_commands() {
    let mut mentions = 0usize;
    for span in prose_inline_code(SLASH_COMMAND_DOCS) {
        let Some(token) = span.split_whitespace().next() else {
            continue;
        };
        let Some(rest) = token.strip_prefix('/') else {
            continue;
        };
        // Paths have another slash. Single-character model shortcuts such as
        // `/f` are runtime bindings rather than registered command names.
        // `/review-pr` is the custom-command example in the "Custom commands"
        // section, not a built-in.
        if token == "/command"
            || token == "/review-pr"
            || rest.len() < 2
            || rest.contains('/')
            || !rest
                .chars()
                .all(|character| character.is_ascii_lowercase() || character == '-')
        {
            continue;
        }
        let canonical = crate::commands::metadata::canonical_command_name(token);
        assert!(
            crate::commands::COMMANDS
                .iter()
                .any(|command| command.name == canonical),
            "docs/slash-commands.md mentions `{token}`, which is not a registered command"
        );
        mentions += 1;
    }
    assert!(
        mentions >= 30,
        "slash-command extraction found only {mentions}"
    );
}

#[test]
fn release_platform_claims_match_installer_and_workflow() {
    let installer_targets = target_triples(INSTALLER);
    let workflow_targets = target_triples(RELEASE_WORKFLOW);

    // The installer constructs triples dynamically from os/arch parts, so the
    // concrete list lives in a comment there purely for this check. Equality is
    // deliberately bidirectional: an installer target the workflow never builds
    // means `install.sh` downloads a release asset that was never uploaded.
    assert_eq!(installer_targets.len(), 4);
    assert_eq!(workflow_targets, installer_targets);
    // The getting-started guide documents supported platforms in prose rather
    // than exposing target triples; the installer and release workflow remain
    // the machine-readable source of truth.
    assert!(target_triples(GETTING_STARTED).is_empty());
    for label in [
        "macOS · Apple Silicon",
        "macOS · Intel",
        "Linux · x86_64",
        "Linux · arm64",
    ] {
        assert!(
            SITE.contains(label),
            "website omits supported target `{label}`"
        );
    }
    for unsupported in ["Windows", "musl Linux", "BSD", "32-bit systems"] {
        assert!(
            GETTING_STARTED.contains(unsupported),
            "getting-started guide omits unsupported platform `{unsupported}`"
        );
    }
}

const NUMBER_ONES: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const NUMBER_TENS: [&str; 8] = [
    "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

/// The English spelling of a count under 100, so a page that says "Twenty-three
/// connections" is checked exactly like one that says "23 connections".
fn number_word(count: usize) -> Option<String> {
    if let Some(word) = NUMBER_ONES.get(count) {
        return Some((*word).to_string());
    }
    let tens = count / 10;
    let ones = count % 10;
    let tens_word = NUMBER_TENS.get(tens.checked_sub(2)?)?;
    Some(if ones == 0 {
        (*tens_word).to_string()
    } else {
        format!("{tens_word}-{}", NUMBER_ONES.get(ones)?)
    })
}

/// Every number word the claim scanner recognizes (0-99); tokens outside this
/// vocabulary ("active", "new") are prose, not counts.
fn number_vocabulary() -> BTreeSet<String> {
    (0..100).filter_map(number_word).collect()
}

/// Every count written directly before `noun` in `source`, lowercased, both
/// digit and spelled-out forms. Scans all occurrences, so a page carrying one
/// correct and one stale claim fails instead of passing on the first hit.
fn count_claims(source: &str, noun: &str, vocabulary: &BTreeSet<String>) -> BTreeSet<String> {
    let mut claims = BTreeSet::new();
    let mut rest = source;
    while let Some(index) = rest.find(noun) {
        let token = rest[..index]
            .trim_end()
            .rsplit(|character: char| !(character.is_ascii_alphanumeric() || character == '-'))
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !token.is_empty()
            && (token.chars().all(|character| character.is_ascii_digit())
                || vocabulary.contains(&token))
        {
            claims.insert(token);
        }
        rest = &rest[index + noun.len()..];
    }
    claims
}

fn assert_count_claims(
    name: &str,
    source: &str,
    noun: &str,
    expected: usize,
    vocabulary: &BTreeSet<String>,
) {
    let claims = count_claims(source, noun, vocabulary);
    assert!(
        !claims.is_empty(),
        "{name} states no `{noun}` count, so the drift guard went blind"
    );
    let mut accepted: BTreeSet<String> = BTreeSet::from([expected.to_string()]);
    if let Some(word) = number_word(expected) {
        accepted.insert(word);
    }
    for claim in claims {
        assert!(
            accepted.contains(&claim),
            "{name} claims `{claim} {noun}` but the binary ships {expected} \
             (accepted spellings: {accepted:?})"
        );
    }
}

#[test]
fn count_claims_reads_digits_words_and_ignores_prose() {
    let vocabulary = number_vocabulary();
    let claims = count_claims(
        "<span>23 connections</span> Twenty-three connections. Active connections. 18 themes",
        "connection",
        &vocabulary,
    );
    assert_eq!(
        claims,
        BTreeSet::from(["23".to_string(), "twenty-three".to_string()]),
        "the scanner must see both spellings and skip prose that is not a count"
    );
    assert_eq!(number_word(18).as_deref(), Some("eighteen"));
    assert_eq!(number_word(23).as_deref(), Some("twenty-three"));
    assert_eq!(number_word(120), None);
}

#[test]
fn website_feature_counts_follow_builtin_registries() {
    let connection_count = CONNECTIONS.matches("[[connections]]").count();
    let theme_count = crate::tui::theme::builtin_names().len();

    let app = crate::tui::test_utils::app();
    let profile = crate::smol::SmolProfile::resolve(crate::smol::SmolPreference::Off, 128_000);
    let autonomy_count = crate::tui::settings::seed_settings_rows(&app, profile)
        .into_iter()
        .find_map(|row| match row {
            crate::tui::event::SettingsRow::Choice {
                id: crate::tui::event::SettingId::Autonomy,
                values,
                ..
            } => Some(values.len()),
            _ => None,
        })
        .expect("settings screen has an autonomy row");

    let vocabulary = number_vocabulary();
    for (name, source) in [
        ("site/index.html", SITE),
        ("docs/providers.md", PROVIDERS_DOCS),
    ] {
        assert_count_claims(name, source, "connection", connection_count, &vocabulary);
    }
    assert_count_claims("site/index.html", SITE, "theme", theme_count, &vocabulary);
    assert_count_claims(
        "site/index.html",
        SITE,
        "level",
        autonomy_count,
        &vocabulary,
    );
}
