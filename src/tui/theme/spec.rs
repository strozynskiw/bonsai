//! TOML schema and parser for user-defined themes.
//!
//! A theme file names a color per semantic role (see [`Palette`]). The role set
//! is defined once by the [`for_each_role!`] X-macro so the [`ThemeSpec`] struct,
//! the resolver, and the `/theme export` template can never drift apart — a role
//! added to [`Palette`] but not the macro (or vice-versa) fails to compile.
//!
//! Colors are `"#rrggbb"` strings. A file may `extends = "<built-in>"` to inherit
//! every role it doesn't set; without `extends`, all roles are required and a
//! single error lists every one that is missing.

use std::path::{Path, PathBuf};

use ratatui::style::Color;
use serde::Deserialize;

use super::Palette;

/// The single source of truth for the semantic role set. Invokes `$cb!` with a
/// `role => "doc"` list; callbacks below generate the spec struct, the resolver,
/// and the export template from it.
macro_rules! for_each_role {
    ($cb:ident) => {
        $cb! {
            // Surfaces
            bg => "Base background behind every surface.",
            panel => "Primary raised panel surface (frames, modals, transcript).",
            panel_dark => "Recessed surface for chrome (input box, footers).",
            border => "Idle frame/border color.",
            border_active => "Focused frame/border color and default title accent.",
            // Transcript card backgrounds
            user_block => "Background of a user-message card.",
            assistant_block => "Background of an assistant-message card.",
            thinking_block => "Background of a thinking / reasoning card.",
            tool_block => "Background of a tool-call card.",
            result_block => "Background of a tool-result card.",
            edit_block => "Background of a file-edit / diff card.",
            todo_block => "Background of a todo / plan card.",
            error_block => "Background of an error card.",
            peer_block => "Background of inter-agent (peer) chat messages.",
            // Text
            text => "Primary foreground text.",
            muted => "Secondary text (labels, metadata).",
            dim => "Tertiary text (hints, disabled, comments).",
            // Semantic accents
            user => "Accent for user-authored content.",
            assistant => "Accent for assistant-authored content.",
            progress => "Accent for in-progress / streaming state.",
            tool => "Accent for tool activity.",
            edit => "Accent for file edits.",
            error => "Accent for errors and failures.",
            todo => "Accent for todos / plan items.",
            success => "Accent for success / completion.",
            peer => "Inter-agent (peer) message accent.",
            selection_bg => "Background fill for the selected row / picker entry.",
            // Diff view
            added => "Foreground of added diff lines.",
            added_bg => "Background tint of added diff lines.",
            removed => "Foreground of removed diff lines.",
            removed_bg => "Background tint of removed diff lines.",
            lineno => "Diff / code line-number gutter color.",
            // Semantic value colors inside tool cards
            path => "File-path values inside tool cards.",
            command => "Shell-command values inside tool cards.",
            // Syntax highlighting (fenced code blocks)
            syntax_comment => "Code comments.",
            syntax_string => "String and char literals.",
            syntax_number => "Numeric literals.",
            syntax_keyword => "Language keywords.",
            syntax_type => "Type names (capitalized identifiers).",
            syntax_function => "Function-call identifiers.",
            // Context inspector role colors
            context_system => "Context inspector: system-role content.",
            context_user => "Context inspector: user-role content.",
            context_assistant => "Context inspector: assistant-role content.",
            context_tool => "Context inspector: tool-role content.",
            context_tool_schema => "Context inspector: tool-schema content.",
            // Per-view identity accents
            agent_accent => "Agent-view focused accent (bright).",
            agent_border => "Agent-view idle border (dim).",
            plan_accent => "Plan-view focused accent (bright).",
            plan_border => "Plan-view idle border (dim).",
        }
    };
}
// Shared with `super::capability` so palette adaptation iterates the same roles.
pub(crate) use for_each_role;

/// The parsed form of a theme TOML file: every role optional (so `extends` can
/// fill the gaps and so a missing-roles error can list *all* of them at once),
/// plus the two non-color keys. Unknown keys are rejected, naming the offender.
macro_rules! define_theme_spec {
    ($($role:ident => $doc:expr),* $(,)?) => {
        #[derive(Debug, Clone, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub(crate) struct ThemeSpec {
            /// One-line description shown by `/theme`; defaults to "custom theme".
            #[serde(default)]
            pub blurb: Option<String>,
            /// Name of a built-in theme to inherit unset roles from.
            #[serde(default)]
            pub extends: Option<String>,
            $(
                #[serde(default)]
                pub $role: Option<String>,
            )*
        }
    };
}
for_each_role!(define_theme_spec);

/// Resolves one role: parse the supplied hex, else fall back to the base
/// palette's value, else record it as missing.
fn resolve_role(
    missing: &mut Vec<&'static str>,
    invalid: &mut Vec<String>,
    role: &'static str,
    raw: Option<&str>,
    fallback: Option<Color>,
) -> Option<Color> {
    match raw {
        Some(value) => match parse_role_color(value) {
            Some(color) => Some(color),
            None => {
                invalid.push(format!(
                    "role '{role}': invalid color '{value}' (expected \"#rrggbb\")"
                ));
                None
            }
        },
        None => {
            if fallback.is_none() {
                missing.push(role);
            }
            fallback
        }
    }
}

/// Parses a `"#rrggbb"` role color. The `#` is required so error messages can
/// point at the exact expected format.
fn parse_role_color(value: &str) -> Option<Color> {
    let hex = value.trim().strip_prefix('#')?;
    super::parse_hex_color(hex)
}

/// Builds a palette from a spec, overlaying an optional `extends` base. Returns
/// every problem at once (missing roles collapsed into one line, invalid colors
/// listed individually). `name`/`blurb` are placeholders here — the caller sets
/// them only on success, so the error path leaks nothing.
macro_rules! resolve_spec_body {
    ($($role:ident => $doc:expr),* $(,)?) => {
        fn resolve_spec(base: Option<&Palette>, spec: &ThemeSpec) -> Result<Palette, Vec<String>> {
            let mut missing: Vec<&'static str> = Vec::new();
            let mut invalid: Vec<String> = Vec::new();
            $(
                let $role = resolve_role(
                    &mut missing,
                    &mut invalid,
                    stringify!($role),
                    spec.$role.as_deref(),
                    base.map(|b| b.$role),
                );
            )*
            let mut problems = Vec::new();
            if !missing.is_empty() {
                problems.push(format!("missing roles: {}", missing.join(", ")));
            }
            problems.extend(invalid);
            if !problems.is_empty() {
                return Err(problems);
            }
            Ok(Palette {
                name: "",
                blurb: "",
                $( $role: $role.expect("no problems means every role resolved"), )*
            })
        }
    };
}
for_each_role!(resolve_spec_body);

/// A theme file that could not be loaded, naming the theme, its path, and every
/// problem found (all missing roles, all invalid colors, or a parse error).
#[derive(Debug, Clone)]
pub(crate) struct ThemeFileError {
    name: String,
    path: PathBuf,
    problems: Vec<String>,
}

impl std::fmt::Display for ThemeFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "theme '{}' ({}): {}",
            self.name,
            self.path.display(),
            self.problems.join("; ")
        )
    }
}

impl std::error::Error for ThemeFileError {}

/// Parses a theme file into a `'static` palette (its `name`/`blurb` are leaked).
/// `name` is the file stem; it is normalized to lowercase and validated. The
/// returned palette is owned; the caller leaks it into the registry.
pub(crate) fn parse_theme(
    name: &str,
    path: &Path,
    content: &str,
) -> Result<Palette, ThemeFileError> {
    let Some(name) = normalize_theme_name(name) else {
        let raw = name.trim().to_string();
        return Err(ThemeFileError {
            name: raw.clone(),
            path: path.to_path_buf(),
            problems: vec![format!(
                "invalid theme name '{raw}' (allowed: a-z, 0-9, '_', '-')"
            )],
        });
    };
    let err = |problems| ThemeFileError {
        name: name.clone(),
        path: path.to_path_buf(),
        problems,
    };

    let spec: ThemeSpec = toml::from_str(content).map_err(|error| err(vec![error.to_string()]))?;

    let base = match spec.extends.as_deref() {
        Some(base_name) => match super::builtin_by_name(base_name) {
            Some(base) => Some(base),
            None => {
                return Err(err(vec![format!(
                    "extends unknown theme '{base_name}' (must be a built-in: {})",
                    super::builtin_names().join(", ")
                )]));
            }
        },
        None => None,
    };

    let mut palette = resolve_spec(base, &spec).map_err(err)?;
    let blurb = spec.blurb.unwrap_or_else(|| "custom theme".to_string());
    // Intern rather than leak so re-parsing an unchanged file reuses these
    // strings instead of leaking a fresh pair on every rescan.
    palette.name = super::intern_str(&name);
    palette.blurb = super::intern_str(&blurb);
    Ok(palette)
}

/// Normalizes a theme name (trim + lowercase) and validates it against the
/// allowed charset `[a-z0-9_-]`. Shared by the loader and `/theme export`; the
/// tight charset also makes path traversal impossible by construction.
pub(crate) fn normalize_theme_name(name: &str) -> Option<String> {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return None;
    }
    Some(name)
}

/// Formats a palette color as `#rrggbb`. Every palette passed to the exporter is
/// a pre-adaptation *original*, so the color is always `Color::Rgb`; the fallback
/// only guards against a future non-rgb role.
fn hex_of(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => "#000000".to_string(),
    }
}

/// Escapes a string for a TOML basic (double-quoted) string. Custom theme
/// blurbs are arbitrary user content, so a raw quote/backslash/newline would
/// otherwise produce an invalid — or key-injecting — exported file.
fn escape_toml_basic(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            // Remaining control chars must be escaped (TOML disallows them raw).
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// The comment header of an exported theme file.
fn theme_file_header(palette: &Palette) -> String {
    format!(
        "# bonsai theme — a complete, standalone palette (every role set).\n\
         #\n\
         # Load order (first wins): .bonsai/themes/, .claude/themes/,\n\
         # .agents/themes/ (project), then $BONSAI_HOME/themes/ (global).\n\
         # Select it with:  /theme <name>   (name = this file's stem, lowercased).\n\
         #\n\
         # Colors are \"#rrggbb\". To instead inherit a built-in and override only\n\
         # a few roles, replace this file's body with:  extends = \"forest\"\n\
         # plus just the roles you want to change.\n\
         \n\
         blurb = \"{}\"\n\n",
        escape_toml_basic(palette.blurb)
    )
}

/// Renders a palette to a self-documenting theme TOML file — the starter file
/// `/theme export` writes. Each role is preceded by its one-line doc comment.
macro_rules! render_theme_toml_body {
    ($($role:ident => $doc:expr),* $(,)?) => {
        pub(crate) fn render_theme_toml(palette: &Palette) -> String {
            let mut out = theme_file_header(palette);
            $(
                out.push_str(&format!(
                    "# {}\n{} = \"{}\"\n\n",
                    $doc,
                    stringify!($role),
                    hex_of(palette.$role),
                ));
            )*
            out
        }
    };
}
for_each_role!(render_theme_toml_body);

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = include_str!("testdata/full.toml");

    fn parse(name: &str, content: &str) -> Result<Palette, ThemeFileError> {
        parse_theme(name, Path::new("mem.toml"), content)
    }

    #[test]
    fn full_spec_parses_every_role() {
        let palette = parse("mytheme", FULL).expect("full theme should parse");
        assert_eq!(palette.name, "mytheme");
        assert_eq!(palette.bg, Color::Rgb(0x10, 0x10, 0x10));
        assert_eq!(palette.syntax_keyword, Color::Rgb(0xff, 0x00, 0xaa));
    }

    #[test]
    fn missing_roles_are_all_named() {
        let err = parse("t", "bg = \"#101010\"\n").expect_err("incomplete theme must fail");
        let text = err.to_string();
        assert!(text.contains("missing roles:"), "{text}");
        // A role early and late in the set both appear — not just the first.
        assert!(text.contains("panel"), "{text}");
        assert!(text.contains("plan_border"), "{text}");
        // The one role that WAS provided is not reported missing.
        assert!(
            !text.contains(" bg,") && !text.contains("missing roles: bg"),
            "{text}"
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = parse("t", "pannel = \"#101010\"\n").expect_err("typo key must fail");
        assert!(err.to_string().contains("pannel"), "{err}");
    }

    #[test]
    fn invalid_color_names_the_role() {
        let toml = "extends = \"forest\"\nbg = \"blue\"\n";
        let err = parse("t", toml).expect_err("non-hex color must fail");
        let text = err.to_string();
        assert!(text.contains("role 'bg'"), "{text}");
        assert!(text.contains("invalid color 'blue'"), "{text}");
    }

    #[test]
    fn extends_overlays_only_named_roles() {
        let base = super::super::builtin_by_name("forest").unwrap();
        let toml = "extends = \"forest\"\nbg = \"#010203\"\n";
        let palette = parse("tweaked", toml).expect("extends theme should parse");
        assert_eq!(palette.bg, Color::Rgb(1, 2, 3), "overridden role");
        assert_eq!(palette.panel, base.panel, "inherited role");
        assert_eq!(palette.blurb, "custom theme", "default blurb");
    }

    #[test]
    fn extends_unknown_base_is_rejected() {
        let err = parse("t", "extends = \"neon\"\n").expect_err("bad base must fail");
        assert!(err.to_string().contains("unknown theme 'neon'"), "{err}");
    }

    #[test]
    fn bad_name_is_rejected() {
        let err = parse("My Theme", "extends = \"forest\"\n").expect_err("bad name must fail");
        assert!(err.to_string().contains("invalid theme name"), "{err}");
    }

    #[test]
    fn export_round_trips_through_parse() {
        // Exporting a built-in and parsing the result must reproduce every color
        // role — this is the drift guard between the template and the parser.
        for name in ["forest", "ocean", "paper"] {
            let source = super::super::builtin_by_name(name).unwrap();
            let toml = render_theme_toml(source);
            let mut parsed = parse("roundtrip", &toml)
                .unwrap_or_else(|err| panic!("{name} export must parse: {err}"));
            // name/blurb are metadata set from the file stem, not color roles.
            parsed.name = source.name;
            parsed.blurb = source.blurb;
            assert_eq!(&parsed, source, "{name}: exported palette must round-trip");
        }
    }

    #[test]
    fn export_escapes_special_chars_in_blurb() {
        // A custom blurb with quotes/backslash/newline must survive export as
        // valid TOML that reloads to the same string.
        let forest = super::super::builtin_by_name("forest").unwrap();
        let mut nasty = *forest;
        nasty.blurb = "a \"quoted\" \\ theme\nwith newline";
        let toml = render_theme_toml(&nasty);
        let parsed = parse("t", &toml).expect("escaped blurb must produce valid TOML");
        assert_eq!(
            parsed.blurb, nasty.blurb,
            "blurb round-trips through export"
        );
    }

    #[test]
    fn normalize_theme_name_enforces_charset() {
        assert_eq!(normalize_theme_name("  Nord "), Some("nord".to_string()));
        assert_eq!(
            normalize_theme_name("my_theme-2"),
            Some("my_theme-2".to_string())
        );
        assert_eq!(normalize_theme_name("../evil"), None);
        assert_eq!(normalize_theme_name(""), None);
    }
}
