//! Terminal color-capability detection and palette adaptation.
//!
//! Every built-in palette is authored in 24-bit truecolor. Terminals that can't
//! render that need the colors downsampled *once*, at registry-build time, so
//! the per-draw path stays a plain field read. [`adapt`] produces the palette
//! widgets actually draw; the registry keeps the un-adapted original for
//! `/theme export`.

#[cfg(not(test))]
use std::sync::OnceLock;

use ratatui::style::Color;

use super::Palette;
use super::spec::for_each_role;

/// How much color the terminal can render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorMode {
    /// 24-bit RGB — palettes are used verbatim.
    TrueColor,
    /// 256-color — RGB is quantized to the xterm-256 palette.
    Xterm256,
    /// No color — every role collapses to the terminal default (`NO_COLOR`).
    Monochrome,
}

/// Resolves the color mode from environment variables (first match wins):
/// `BONSAI_COLOR_MODE` override → `NO_COLOR` → `COLORTERM` → `TERM` → default.
/// The env getter is injected so the policy is testable without touching the
/// process environment.
pub(crate) fn detect_color_mode(get: impl Fn(&str) -> Option<String>) -> ColorMode {
    // Explicit override / escape hatch.
    if let Some(value) = get("BONSAI_COLOR_MODE") {
        match value.trim().to_ascii_lowercase().as_str() {
            "truecolor" | "24bit" => return ColorMode::TrueColor,
            "256" | "xterm256" => return ColorMode::Xterm256,
            "mono" | "monochrome" | "none" => return ColorMode::Monochrome,
            _ => {}
        }
    }
    // https://no-color.org — any non-empty value disables color.
    if get("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        return ColorMode::Monochrome;
    }
    if let Some(colorterm) = get("COLORTERM") {
        let colorterm = colorterm.to_ascii_lowercase();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return ColorMode::TrueColor;
        }
    }
    if get("TERM").is_some_and(|term| term.contains("256color")) {
        return ColorMode::Xterm256;
    }
    // Unknown terminals keep the status-quo behavior (emit truecolor).
    ColorMode::TrueColor
}

/// The process-wide color mode, detected once from the real environment.
#[cfg(not(test))]
static COLOR_MODE: OnceLock<ColorMode> = OnceLock::new();

/// The active color mode. Tests always see `TrueColor` (so value assertions on
/// `palette()` hold regardless of the dev terminal); detection is exercised
/// directly through [`detect_color_mode`], and adaptation through [`adapt`].
pub(crate) fn color_mode() -> ColorMode {
    #[cfg(test)]
    {
        ColorMode::TrueColor
    }
    #[cfg(not(test))]
    {
        *COLOR_MODE.get_or_init(|| detect_color_mode(|key| std::env::var(key).ok()))
    }
}

/// Downsamples one color for `mode`.
fn adapt_color(color: Color, mode: ColorMode) -> Color {
    match mode {
        ColorMode::TrueColor => color,
        ColorMode::Xterm256 => match color {
            Color::Rgb(r, g, b) => Color::Indexed(nearest_xterm256(r, g, b)),
            other => other,
        },
        // Modifiers (bold/italic/reverse) survive; only hues collapse.
        ColorMode::Monochrome => Color::Reset,
    }
}

/// Adapts a whole palette for `mode`. `TrueColor` is the identity. The role list
/// comes from the same X-macro as the spec, so a new role is adapted automatically.
macro_rules! adapt_palette_body {
    ($($role:ident => $doc:expr),* $(,)?) => {
        pub(crate) fn adapt(palette: &Palette, mode: ColorMode) -> Palette {
            Palette {
                name: palette.name,
                blurb: palette.blurb,
                $( $role: adapt_color(palette.$role, mode), )*
            }
        }
    };
}
for_each_role!(adapt_palette_body);

/// The six intensity levels of the xterm-256 color cube.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Index (0..6) of the cube level nearest to `value`.
fn nearest_cube_level(value: u8) -> usize {
    let mut best = 0;
    let mut best_dist = u32::MAX;
    for (index, &level) in CUBE_LEVELS.iter().enumerate() {
        let dist = (i32::from(level) - i32::from(value)).unsigned_abs();
        if dist < best_dist {
            best_dist = dist;
            best = index;
        }
    }
    best
}

fn dist2(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let dr = i32::from(a.0) - i32::from(b.0);
    let dg = i32::from(a.1) - i32::from(b.1);
    let db = i32::from(a.2) - i32::from(b.2);
    (dr * dr + dg * dg + db * db) as u32
}

/// Nearest xterm-256 palette index (16..255) for an RGB color, choosing between
/// the 6×6×6 color cube and the 24-step grayscale ramp by squared distance.
pub(crate) fn nearest_xterm256(r: u8, g: u8, b: u8) -> u8 {
    // Candidate 1: the color cube (indices 16..231).
    let (ri, gi, bi) = (
        nearest_cube_level(r),
        nearest_cube_level(g),
        nearest_cube_level(b),
    );
    let cube = (CUBE_LEVELS[ri], CUBE_LEVELS[gi], CUBE_LEVELS[bi]);
    let cube_index = (16 + 36 * ri + 6 * gi + bi) as u8;
    let cube_dist = dist2((r, g, b), cube);

    // Candidate 2: the grayscale ramp (indices 232..255, values 8 + 10*j).
    let avg = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
    let j = if avg < 8 {
        0
    } else {
        (((avg - 8 + 5) / 10) as u8).min(23)
    };
    let gray_val = 8 + 10 * j;
    let gray_index = 232 + j;
    let gray_dist = dist2((r, g, b), (gray_val, gray_val, gray_val));

    if gray_dist < cube_dist {
        gray_index
    } else {
        cube_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detection_priority_and_defaults() {
        // Override beats everything.
        assert_eq!(
            detect_color_mode(env_of(&[
                ("BONSAI_COLOR_MODE", "truecolor"),
                ("NO_COLOR", "1"),
                ("TERM", "xterm-256color"),
            ])),
            ColorMode::TrueColor
        );
        // NO_COLOR beats COLORTERM.
        assert_eq!(
            detect_color_mode(env_of(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")])),
            ColorMode::Monochrome
        );
        // Empty NO_COLOR is ignored (per the spec).
        assert_eq!(
            detect_color_mode(env_of(&[("NO_COLOR", ""), ("COLORTERM", "truecolor")])),
            ColorMode::TrueColor
        );
        // COLORTERM=truecolor wins over a 256-color TERM.
        assert_eq!(
            detect_color_mode(env_of(&[
                ("COLORTERM", "truecolor"),
                ("TERM", "xterm-256color")
            ])),
            ColorMode::TrueColor
        );
        // Bare 256-color terminal downsamples.
        assert_eq!(
            detect_color_mode(env_of(&[("TERM", "xterm-256color")])),
            ColorMode::Xterm256
        );
        // Unknown terminal keeps truecolor (status quo).
        assert_eq!(detect_color_mode(env_of(&[])), ColorMode::TrueColor);
    }

    #[test]
    fn quantizer_hits_known_indices() {
        assert_eq!(nearest_xterm256(0, 0, 0), 16, "black → cube 16");
        assert_eq!(nearest_xterm256(255, 255, 255), 231, "white → cube 231");
        // Exact cube point (95, 135, 175) resolves to its exact index.
        assert_eq!(nearest_xterm256(95, 135, 175), 67);
        // Mid-gray prefers the finer grayscale ramp over the coarse cube.
        assert!((232..=255).contains(&nearest_xterm256(130, 130, 130)));
    }

    #[test]
    fn adapt_truecolor_is_identity() {
        let forest = super::super::builtin_by_name("forest").unwrap();
        assert_eq!(&adapt(forest, ColorMode::TrueColor), forest);
    }

    #[test]
    fn adapt_xterm256_yields_only_indexed() {
        let forest = super::super::builtin_by_name("forest").unwrap();
        let adapted = adapt(forest, ColorMode::Xterm256);
        assert!(matches!(adapted.bg, Color::Indexed(_)));
        assert!(matches!(adapted.text, Color::Indexed(_)));
        assert!(matches!(adapted.syntax_keyword, Color::Indexed(_)));
        assert_eq!(adapted.name, "forest", "metadata is preserved");
    }

    #[test]
    fn adapt_monochrome_resets_every_role() {
        let forest = super::super::builtin_by_name("forest").unwrap();
        let adapted = adapt(forest, ColorMode::Monochrome);
        assert_eq!(adapted.bg, Color::Reset);
        assert_eq!(adapted.text, Color::Reset);
        assert_eq!(adapted.error, Color::Reset);
    }
}
