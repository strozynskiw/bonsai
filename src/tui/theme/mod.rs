//! Theme system. All colors live in a [`Palette`]; widgets read the active
//! palette through [`palette()`] so themes can switch at runtime (`/theme`).
//!
//! Background rule: text styles carry a foreground only — the surface that
//! renders them (card, panel, input box) owns the background. Explicit span
//! backgrounds are reserved for deliberate chips (pills, selection, diff
//! rows). This is what keeps theme switches and nested surfaces leak-free.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock, PoisonError, RwLock};

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{
    Block as UiBlock, BorderType, Borders, Padding, Scrollbar, ScrollbarOrientation,
};

pub(crate) mod capability;
pub(crate) mod spec;

/// The complete set of semantic color roles a theme defines. Every field is a
/// role the UI draws through, so a custom theme that sets all of them recolors
/// the whole interface. The doc comment on each field is the role contract; it
/// is mirrored by the commented starter file `/theme export` writes and by
/// `docs/theming.md`. See [`crate::tui::theme::spec`] for the TOML schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Palette {
    /// Theme name (lowercase). Not a color; set from the file stem for custom
    /// themes and never appears in a theme TOML file.
    pub name: &'static str,
    /// One-line description shown by `/theme` and the picker. Not a color.
    pub blurb: &'static str,

    // Surfaces
    /// Base background behind every surface — the terminal canvas color.
    pub bg: Color,
    /// Primary raised panel surface (frames, modals, transcript body).
    pub panel: Color,
    /// Recessed panel surface for chrome (input box, footers, helper bars).
    pub panel_dark: Color,
    /// Idle frame/border color.
    pub border: Color,
    /// Focused frame/border color, also the default title accent.
    pub border_active: Color,

    // Transcript card backgrounds
    /// Background of a user-message card.
    pub user_block: Color,
    /// Background of an assistant-message card.
    pub assistant_block: Color,
    /// Background of a thinking / reasoning card.
    pub thinking_block: Color,
    /// Background of a tool-call card.
    pub tool_block: Color,
    /// Background of a tool-result card.
    pub result_block: Color,
    /// Background of a file-edit / diff card.
    pub edit_block: Color,
    /// Background of a todo / plan card.
    pub todo_block: Color,
    /// Background of an error card.
    pub error_block: Color,
    /// Background of inter-agent (peer) chat messages — the blue conversation lane.
    pub peer_block: Color,

    // Text
    /// Primary foreground text.
    pub text: Color,
    /// Secondary text (labels, metadata) — one step down from `text`.
    pub muted: Color,
    /// Tertiary text (hints, disabled, comments) — the faintest readable tier.
    pub dim: Color,

    // Semantic accents
    /// Accent for user-authored content.
    pub user: Color,
    /// Accent for assistant-authored content.
    pub assistant: Color,
    /// Accent for in-progress / spinner / streaming state.
    pub progress: Color,
    /// Accent for tool activity.
    pub tool: Color,
    /// Accent for file edits.
    pub edit: Color,
    /// Accent for errors and failures.
    pub error: Color,
    /// Accent for todos / plan items.
    pub todo: Color,
    /// Accent for success / completion.
    pub success: Color,
    /// Inter-agent (peer) message accent — blue in every theme so agent↔agent
    /// conversation is instantly distinguishable from user/assistant text.
    pub peer: Color,
    /// Background fill for the selected row / active picker entry.
    pub selection_bg: Color,

    // Diff view
    /// Foreground of added diff lines.
    pub added: Color,
    /// Background tint of added diff lines.
    pub added_bg: Color,
    /// Foreground of removed diff lines.
    pub removed: Color,
    /// Background tint of removed diff lines.
    pub removed_bg: Color,
    /// Diff / code line-number gutter color.
    pub lineno: Color,

    // Semantic value colors inside tool cards
    /// File-path values inside tool cards.
    pub path: Color,
    /// Shell-command values inside tool cards.
    pub command: Color,

    // Syntax highlighting (fenced code blocks in rendered markdown)
    /// Code comments.
    pub syntax_comment: Color,
    /// String and char literals.
    pub syntax_string: Color,
    /// Numeric literals.
    pub syntax_number: Color,
    /// Language keywords.
    pub syntax_keyword: Color,
    /// Type names (capitalized identifiers).
    pub syntax_type: Color,
    /// Function-call identifiers.
    pub syntax_function: Color,

    // Context inspector role colors
    /// Context inspector: system-role content.
    pub context_system: Color,
    /// Context inspector: user-role content.
    pub context_user: Color,
    /// Context inspector: assistant-role content.
    pub context_assistant: Color,
    /// Context inspector: tool-role content.
    pub context_tool: Color,
    /// Context inspector: tool-schema content.
    pub context_tool_schema: Color,

    // Per-view identity accents (bright / dim border pairs)
    /// Agent-view focused accent (bright).
    pub agent_accent: Color,
    /// Agent-view idle border (dim).
    pub agent_border: Color,
    /// Plan-view focused accent (bright).
    pub plan_accent: Color,
    /// Plan-view idle border (dim).
    pub plan_border: Color,
}

/// Warm, mossy dark theme — the bonsai default.
const FOREST: Palette = Palette {
    name: "forest",
    blurb: "warm mossy dark (default)",
    bg: Color::Rgb(12, 15, 12),
    panel: Color::Rgb(23, 30, 24),
    panel_dark: Color::Rgb(18, 24, 18),
    border: Color::Rgb(72, 84, 66),
    border_active: Color::Rgb(128, 182, 108),
    user_block: Color::Rgb(40, 55, 34),
    assistant_block: Color::Rgb(34, 38, 31),
    thinking_block: Color::Rgb(46, 40, 27),
    tool_block: Color::Rgb(30, 50, 36),
    result_block: Color::Rgb(32, 45, 36),
    edit_block: Color::Rgb(58, 42, 29),
    todo_block: Color::Rgb(55, 50, 27),
    error_block: Color::Rgb(55, 30, 24),
    peer_block: Color::Rgb(26, 36, 52),
    text: Color::Rgb(235, 230, 216),
    muted: Color::Rgb(186, 192, 168),
    dim: Color::Rgb(142, 154, 124),
    user: Color::Rgb(194, 222, 132),
    assistant: Color::Rgb(236, 233, 216),
    progress: Color::Rgb(198, 176, 133),
    tool: Color::Rgb(128, 188, 130),
    edit: Color::Rgb(206, 154, 106),
    error: Color::Rgb(236, 140, 123),
    todo: Color::Rgb(204, 177, 112),
    success: Color::Rgb(128, 172, 132),
    peer: Color::Rgb(124, 168, 216),
    selection_bg: Color::Rgb(58, 78, 47),
    added: Color::Rgb(152, 204, 145),
    added_bg: Color::Rgb(30, 58, 38),
    removed: Color::Rgb(236, 150, 128),
    removed_bg: Color::Rgb(64, 36, 30),
    lineno: Color::Rgb(122, 134, 108),
    path: Color::Rgb(141, 184, 196),
    command: Color::Rgb(212, 183, 124),
    syntax_comment: Color::Rgb(142, 154, 124),
    syntax_string: Color::Rgb(152, 204, 145),
    syntax_number: Color::Rgb(198, 176, 133),
    syntax_keyword: Color::Rgb(206, 154, 106),
    syntax_type: Color::Rgb(141, 184, 196),
    syntax_function: Color::Rgb(212, 183, 124),
    context_system: Color::Rgb(124, 168, 216),
    context_user: Color::Rgb(194, 222, 132),
    context_assistant: Color::Rgb(235, 230, 216),
    context_tool: Color::Rgb(206, 154, 106),
    context_tool_schema: Color::Rgb(178, 145, 218),
    agent_accent: Color::Rgb(124, 168, 216),
    agent_border: Color::Rgb(74, 102, 132),
    plan_accent: Color::Rgb(220, 188, 119),
    plan_border: Color::Rgb(120, 103, 62),
};

/// Cool blue-slate dark theme.
const OCEAN: Palette = Palette {
    name: "ocean",
    blurb: "cool blue-slate dark",
    bg: Color::Rgb(13, 17, 23),
    panel: Color::Rgb(22, 28, 37),
    panel_dark: Color::Rgb(17, 22, 30),
    border: Color::Rgb(72, 90, 112),
    border_active: Color::Rgb(108, 178, 235),
    user_block: Color::Rgb(26, 46, 62),
    assistant_block: Color::Rgb(30, 36, 46),
    thinking_block: Color::Rgb(42, 39, 30),
    tool_block: Color::Rgb(24, 48, 48),
    result_block: Color::Rgb(26, 45, 44),
    edit_block: Color::Rgb(54, 40, 28),
    todo_block: Color::Rgb(50, 45, 26),
    error_block: Color::Rgb(58, 28, 28),
    peer_block: Color::Rgb(28, 34, 64),
    text: Color::Rgb(232, 238, 244),
    muted: Color::Rgb(176, 190, 204),
    dim: Color::Rgb(124, 140, 158),
    user: Color::Rgb(140, 216, 255),
    assistant: Color::Rgb(238, 243, 248),
    progress: Color::Rgb(212, 196, 152),
    tool: Color::Rgb(120, 220, 188),
    edit: Color::Rgb(232, 166, 106),
    error: Color::Rgb(250, 128, 114),
    todo: Color::Rgb(236, 204, 108),
    success: Color::Rgb(126, 222, 146),
    peer: Color::Rgb(150, 170, 250),
    selection_bg: Color::Rgb(44, 72, 102),
    added: Color::Rgb(134, 230, 144),
    added_bg: Color::Rgb(22, 58, 36),
    removed: Color::Rgb(255, 142, 130),
    removed_bg: Color::Rgb(70, 30, 32),
    lineno: Color::Rgb(112, 128, 144),
    path: Color::Rgb(132, 202, 232),
    command: Color::Rgb(230, 202, 132),
    syntax_comment: Color::Rgb(124, 140, 158),
    syntax_string: Color::Rgb(134, 230, 144),
    syntax_number: Color::Rgb(212, 196, 152),
    syntax_keyword: Color::Rgb(232, 166, 106),
    syntax_type: Color::Rgb(132, 202, 232),
    syntax_function: Color::Rgb(230, 202, 132),
    context_system: Color::Rgb(122, 186, 245),
    context_user: Color::Rgb(126, 222, 146),
    context_assistant: Color::Rgb(232, 238, 244),
    context_tool: Color::Rgb(232, 166, 106),
    context_tool_schema: Color::Rgb(190, 150, 255),
    agent_accent: Color::Rgb(122, 186, 245),
    agent_border: Color::Rgb(64, 98, 132),
    plan_accent: Color::Rgb(240, 207, 107),
    plan_border: Color::Rgb(124, 106, 56),
};

/// Light theme on warm paper tones.
const PAPER: Palette = Palette {
    name: "paper",
    blurb: "warm paper light",
    bg: Color::Rgb(236, 233, 222),
    panel: Color::Rgb(248, 246, 238),
    panel_dark: Color::Rgb(240, 236, 226),
    border: Color::Rgb(168, 164, 142),
    border_active: Color::Rgb(86, 134, 56),
    user_block: Color::Rgb(224, 236, 198),
    assistant_block: Color::Rgb(244, 242, 232),
    thinking_block: Color::Rgb(242, 233, 209),
    tool_block: Color::Rgb(219, 238, 222),
    result_block: Color::Rgb(226, 240, 228),
    edit_block: Color::Rgb(246, 228, 206),
    todo_block: Color::Rgb(247, 239, 205),
    error_block: Color::Rgb(250, 221, 214),
    peer_block: Color::Rgb(219, 230, 246),
    text: Color::Rgb(43, 47, 38),
    muted: Color::Rgb(92, 100, 84),
    dim: Color::Rgb(132, 138, 118),
    user: Color::Rgb(86, 116, 26),
    assistant: Color::Rgb(56, 58, 48),
    progress: Color::Rgb(146, 116, 60),
    tool: Color::Rgb(38, 128, 88),
    edit: Color::Rgb(178, 108, 38),
    error: Color::Rgb(196, 64, 52),
    todo: Color::Rgb(164, 128, 22),
    success: Color::Rgb(58, 138, 66),
    peer: Color::Rgb(52, 102, 172),
    selection_bg: Color::Rgb(206, 222, 166),
    added: Color::Rgb(38, 118, 50),
    added_bg: Color::Rgb(214, 238, 212),
    removed: Color::Rgb(176, 52, 42),
    removed_bg: Color::Rgb(248, 217, 211),
    lineno: Color::Rgb(150, 148, 126),
    path: Color::Rgb(28, 112, 142),
    command: Color::Rgb(142, 102, 22),
    syntax_comment: Color::Rgb(132, 138, 118),
    syntax_string: Color::Rgb(38, 118, 50),
    syntax_number: Color::Rgb(146, 116, 60),
    syntax_keyword: Color::Rgb(178, 108, 38),
    syntax_type: Color::Rgb(28, 112, 142),
    syntax_function: Color::Rgb(142, 102, 22),
    context_system: Color::Rgb(52, 102, 172),
    context_user: Color::Rgb(86, 116, 26),
    context_assistant: Color::Rgb(56, 58, 48),
    context_tool: Color::Rgb(178, 108, 38),
    context_tool_schema: Color::Rgb(118, 82, 170),
    agent_accent: Color::Rgb(52, 102, 172),
    agent_border: Color::Rgb(150, 170, 198),
    plan_accent: Color::Rgb(168, 126, 20),
    plan_border: Color::Rgb(204, 182, 126),
};

/// Charcoal dark theme with fire-amber accents.
const EMBER: Palette = Palette {
    name: "ember",
    blurb: "charcoal warmed by fire",
    bg: Color::Rgb(16, 13, 11),
    panel: Color::Rgb(28, 23, 19),
    panel_dark: Color::Rgb(22, 18, 15),
    border: Color::Rgb(92, 74, 58),
    border_active: Color::Rgb(230, 146, 60),
    user_block: Color::Rgb(56, 40, 22),
    assistant_block: Color::Rgb(36, 30, 26),
    thinking_block: Color::Rgb(46, 38, 20),
    tool_block: Color::Rgb(26, 44, 38),
    result_block: Color::Rgb(30, 42, 34),
    edit_block: Color::Rgb(58, 38, 24),
    todo_block: Color::Rgb(54, 46, 22),
    error_block: Color::Rgb(60, 26, 20),
    peer_block: Color::Rgb(30, 34, 54),
    text: Color::Rgb(240, 230, 216),
    muted: Color::Rgb(196, 180, 160),
    dim: Color::Rgb(150, 134, 116),
    user: Color::Rgb(244, 178, 102),
    assistant: Color::Rgb(242, 234, 222),
    progress: Color::Rgb(210, 170, 120),
    tool: Color::Rgb(108, 190, 160),
    edit: Color::Rgb(240, 164, 96),
    error: Color::Rgb(250, 124, 100),
    todo: Color::Rgb(226, 190, 98),
    success: Color::Rgb(150, 200, 130),
    peer: Color::Rgb(136, 168, 230),
    selection_bg: Color::Rgb(74, 54, 34),
    added: Color::Rgb(150, 210, 140),
    added_bg: Color::Rgb(32, 54, 34),
    removed: Color::Rgb(246, 140, 116),
    removed_bg: Color::Rgb(66, 32, 24),
    lineno: Color::Rgb(140, 122, 102),
    path: Color::Rgb(150, 190, 214),
    command: Color::Rgb(226, 186, 120),
    syntax_comment: Color::Rgb(150, 134, 116),
    syntax_string: Color::Rgb(150, 210, 140),
    syntax_number: Color::Rgb(210, 170, 120),
    syntax_keyword: Color::Rgb(240, 164, 96),
    syntax_type: Color::Rgb(150, 190, 214),
    syntax_function: Color::Rgb(226, 186, 120),
    context_system: Color::Rgb(136, 168, 230),
    context_user: Color::Rgb(244, 178, 102),
    context_assistant: Color::Rgb(240, 230, 216),
    context_tool: Color::Rgb(108, 190, 160),
    context_tool_schema: Color::Rgb(186, 150, 230),
    agent_accent: Color::Rgb(136, 168, 230),
    agent_border: Color::Rgb(84, 96, 132),
    plan_accent: Color::Rgb(236, 184, 100),
    plan_border: Color::Rgb(128, 100, 58),
};

/// Dark plum theme with cherry-blossom pink accents.
const SAKURA: Palette = Palette {
    name: "sakura",
    blurb: "dusk plum & blossom pink",
    bg: Color::Rgb(20, 14, 20),
    panel: Color::Rgb(32, 24, 33),
    panel_dark: Color::Rgb(26, 19, 27),
    border: Color::Rgb(96, 74, 96),
    border_active: Color::Rgb(232, 140, 176),
    user_block: Color::Rgb(58, 34, 50),
    assistant_block: Color::Rgb(38, 30, 38),
    thinking_block: Color::Rgb(46, 36, 30),
    tool_block: Color::Rgb(30, 44, 42),
    result_block: Color::Rgb(32, 44, 40),
    edit_block: Color::Rgb(56, 38, 30),
    todo_block: Color::Rgb(52, 44, 28),
    error_block: Color::Rgb(58, 28, 30),
    peer_block: Color::Rgb(28, 34, 58),
    text: Color::Rgb(238, 228, 236),
    muted: Color::Rgb(192, 176, 190),
    dim: Color::Rgb(148, 132, 148),
    user: Color::Rgb(244, 154, 190),
    assistant: Color::Rgb(240, 232, 240),
    progress: Color::Rgb(206, 178, 150),
    tool: Color::Rgb(126, 208, 182),
    edit: Color::Rgb(230, 158, 110),
    error: Color::Rgb(248, 126, 120),
    todo: Color::Rgb(224, 192, 110),
    success: Color::Rgb(140, 204, 150),
    peer: Color::Rgb(140, 170, 240),
    selection_bg: Color::Rgb(76, 48, 68),
    added: Color::Rgb(146, 214, 150),
    added_bg: Color::Rgb(28, 54, 38),
    removed: Color::Rgb(248, 138, 130),
    removed_bg: Color::Rgb(66, 32, 36),
    lineno: Color::Rgb(136, 120, 136),
    path: Color::Rgb(150, 192, 224),
    command: Color::Rgb(222, 186, 128),
    syntax_comment: Color::Rgb(148, 132, 148),
    syntax_string: Color::Rgb(146, 214, 150),
    syntax_number: Color::Rgb(206, 178, 150),
    syntax_keyword: Color::Rgb(230, 158, 110),
    syntax_type: Color::Rgb(150, 192, 224),
    syntax_function: Color::Rgb(222, 186, 128),
    context_system: Color::Rgb(140, 170, 240),
    context_user: Color::Rgb(244, 154, 190),
    context_assistant: Color::Rgb(238, 228, 236),
    context_tool: Color::Rgb(230, 158, 110),
    context_tool_schema: Color::Rgb(184, 152, 238),
    agent_accent: Color::Rgb(140, 170, 240),
    agent_border: Color::Rgb(82, 96, 138),
    plan_accent: Color::Rgb(228, 192, 110),
    plan_border: Color::Rgb(122, 102, 60),
};

/// Soft arctic theme — desaturated blue-grays with icy frost accents.
/// Lighter surfaces than `ocean`, which stays saturated and deep.
const GLACIER: Palette = Palette {
    name: "glacier",
    blurb: "soft arctic blue-gray",
    bg: Color::Rgb(24, 28, 36),
    panel: Color::Rgb(36, 42, 52),
    panel_dark: Color::Rgb(30, 35, 44),
    border: Color::Rgb(88, 100, 118),
    border_active: Color::Rgb(136, 192, 208),
    user_block: Color::Rgb(42, 54, 66),
    assistant_block: Color::Rgb(40, 46, 56),
    thinking_block: Color::Rgb(52, 48, 40),
    tool_block: Color::Rgb(38, 54, 54),
    result_block: Color::Rgb(40, 52, 50),
    edit_block: Color::Rgb(62, 50, 38),
    todo_block: Color::Rgb(58, 54, 38),
    error_block: Color::Rgb(64, 38, 40),
    peer_block: Color::Rgb(40, 44, 70),
    text: Color::Rgb(226, 233, 240),
    muted: Color::Rgb(178, 188, 200),
    dim: Color::Rgb(130, 142, 156),
    user: Color::Rgb(143, 188, 187),
    assistant: Color::Rgb(232, 238, 244),
    progress: Color::Rgb(208, 190, 154),
    tool: Color::Rgb(163, 190, 140),
    edit: Color::Rgb(208, 135, 112),
    error: Color::Rgb(224, 120, 128),
    todo: Color::Rgb(235, 203, 139),
    success: Color::Rgb(151, 200, 144),
    peer: Color::Rgb(129, 161, 193),
    selection_bg: Color::Rgb(58, 74, 94),
    added: Color::Rgb(163, 190, 140),
    added_bg: Color::Rgb(42, 58, 44),
    removed: Color::Rgb(224, 120, 128),
    removed_bg: Color::Rgb(70, 40, 44),
    lineno: Color::Rgb(120, 132, 148),
    path: Color::Rgb(136, 192, 208),
    command: Color::Rgb(235, 203, 139),
    syntax_comment: Color::Rgb(130, 142, 156),
    syntax_string: Color::Rgb(163, 190, 140),
    syntax_number: Color::Rgb(208, 190, 154),
    syntax_keyword: Color::Rgb(208, 135, 112),
    syntax_type: Color::Rgb(136, 192, 208),
    syntax_function: Color::Rgb(235, 203, 139),
    context_system: Color::Rgb(129, 161, 193),
    context_user: Color::Rgb(143, 188, 187),
    context_assistant: Color::Rgb(226, 233, 240),
    context_tool: Color::Rgb(208, 135, 112),
    context_tool_schema: Color::Rgb(180, 142, 173),
    agent_accent: Color::Rgb(136, 192, 208),
    agent_border: Color::Rgb(76, 96, 112),
    plan_accent: Color::Rgb(235, 203, 139),
    plan_border: Color::Rgb(122, 108, 72),
};

/// Light theme on rose-tinted paper — the cool morning counterpart to `paper`.
const DAWN: Palette = Palette {
    name: "dawn",
    blurb: "rosy morning light",
    bg: Color::Rgb(244, 237, 233),
    panel: Color::Rgb(250, 246, 242),
    panel_dark: Color::Rgb(242, 235, 231),
    border: Color::Rgb(176, 160, 158),
    border_active: Color::Rgb(180, 86, 120),
    user_block: Color::Rgb(244, 222, 230),
    assistant_block: Color::Rgb(246, 242, 238),
    thinking_block: Color::Rgb(244, 234, 216),
    tool_block: Color::Rgb(222, 238, 230),
    result_block: Color::Rgb(228, 240, 232),
    edit_block: Color::Rgb(248, 230, 210),
    todo_block: Color::Rgb(248, 240, 208),
    error_block: Color::Rgb(250, 222, 218),
    peer_block: Color::Rgb(222, 232, 246),
    text: Color::Rgb(50, 44, 48),
    muted: Color::Rgb(104, 92, 100),
    dim: Color::Rgb(142, 130, 138),
    user: Color::Rgb(170, 60, 110),
    assistant: Color::Rgb(62, 54, 60),
    progress: Color::Rgb(150, 116, 66),
    tool: Color::Rgb(40, 130, 100),
    edit: Color::Rgb(184, 106, 40),
    error: Color::Rgb(200, 62, 56),
    todo: Color::Rgb(168, 130, 26),
    success: Color::Rgb(64, 140, 72),
    peer: Color::Rgb(66, 100, 180),
    selection_bg: Color::Rgb(238, 214, 224),
    added: Color::Rgb(44, 122, 56),
    added_bg: Color::Rgb(218, 238, 216),
    removed: Color::Rgb(182, 54, 46),
    removed_bg: Color::Rgb(248, 218, 214),
    lineno: Color::Rgb(158, 146, 148),
    path: Color::Rgb(36, 110, 150),
    command: Color::Rgb(148, 104, 28),
    syntax_comment: Color::Rgb(142, 130, 138),
    syntax_string: Color::Rgb(44, 122, 56),
    syntax_number: Color::Rgb(150, 116, 66),
    syntax_keyword: Color::Rgb(184, 106, 40),
    syntax_type: Color::Rgb(36, 110, 150),
    syntax_function: Color::Rgb(148, 104, 28),
    context_system: Color::Rgb(66, 100, 180),
    context_user: Color::Rgb(170, 60, 110),
    context_assistant: Color::Rgb(62, 54, 60),
    context_tool: Color::Rgb(184, 106, 40),
    context_tool_schema: Color::Rgb(122, 84, 172),
    agent_accent: Color::Rgb(66, 100, 180),
    agent_border: Color::Rgb(158, 174, 204),
    plan_accent: Color::Rgb(172, 128, 24),
    plan_border: Color::Rgb(208, 186, 132),
};

/// Catppuccin Mocha — cosy pastel dark.
const CATPPUCCIN: Palette = Palette {
    name: "catppuccin",
    blurb: "Catppuccin Mocha — cosy pastel dark",
    bg: Color::Rgb(30, 30, 46),
    panel: Color::Rgb(49, 50, 68),
    panel_dark: Color::Rgb(24, 24, 37),
    border: Color::Rgb(69, 71, 90),
    border_active: Color::Rgb(137, 180, 250),
    user_block: Color::Rgb(30, 34, 52),
    assistant_block: Color::Rgb(34, 34, 44),
    thinking_block: Color::Rgb(44, 40, 32),
    tool_block: Color::Rgb(28, 42, 42),
    result_block: Color::Rgb(30, 44, 40),
    edit_block: Color::Rgb(48, 38, 30),
    todo_block: Color::Rgb(46, 44, 30),
    error_block: Color::Rgb(50, 32, 38),
    peer_block: Color::Rgb(28, 38, 56),
    text: Color::Rgb(205, 214, 244),
    muted: Color::Rgb(186, 194, 222),
    dim: Color::Rgb(108, 112, 134),
    user: Color::Rgb(180, 190, 254),
    assistant: Color::Rgb(205, 214, 244),
    progress: Color::Rgb(249, 226, 175),
    tool: Color::Rgb(148, 226, 213),
    edit: Color::Rgb(250, 179, 135),
    error: Color::Rgb(243, 139, 168),
    todo: Color::Rgb(249, 226, 175),
    success: Color::Rgb(166, 227, 161),
    peer: Color::Rgb(137, 180, 250),
    selection_bg: Color::Rgb(57, 58, 82),
    added: Color::Rgb(166, 227, 161),
    added_bg: Color::Rgb(30, 50, 38),
    removed: Color::Rgb(243, 139, 168),
    removed_bg: Color::Rgb(54, 32, 38),
    lineno: Color::Rgb(108, 112, 134),
    path: Color::Rgb(116, 199, 236),
    command: Color::Rgb(249, 226, 175),
    syntax_comment: Color::Rgb(108, 112, 134),
    syntax_string: Color::Rgb(166, 227, 161),
    syntax_number: Color::Rgb(250, 179, 135),
    syntax_keyword: Color::Rgb(203, 166, 247),
    syntax_type: Color::Rgb(249, 226, 175),
    syntax_function: Color::Rgb(137, 180, 250),
    context_system: Color::Rgb(137, 180, 250),
    context_user: Color::Rgb(180, 190, 254),
    context_assistant: Color::Rgb(205, 214, 244),
    context_tool: Color::Rgb(250, 179, 135),
    context_tool_schema: Color::Rgb(203, 166, 247),
    agent_accent: Color::Rgb(137, 180, 250),
    agent_border: Color::Rgb(74, 102, 142),
    plan_accent: Color::Rgb(249, 226, 175),
    plan_border: Color::Rgb(140, 120, 80),
};

/// Gruvbox — retro warm dark.
const GRUVBOX: Palette = Palette {
    name: "gruvbox",
    blurb: "Gruvbox — retro warm dark",
    bg: Color::Rgb(40, 40, 40),
    panel: Color::Rgb(60, 56, 54),
    panel_dark: Color::Rgb(29, 32, 33),
    border: Color::Rgb(80, 73, 69),
    border_active: Color::Rgb(142, 192, 124),
    user_block: Color::Rgb(34, 42, 44),
    assistant_block: Color::Rgb(44, 42, 40),
    thinking_block: Color::Rgb(52, 44, 30),
    tool_block: Color::Rgb(36, 48, 36),
    result_block: Color::Rgb(38, 46, 38),
    edit_block: Color::Rgb(56, 42, 28),
    todo_block: Color::Rgb(54, 48, 28),
    error_block: Color::Rgb(56, 34, 30),
    peer_block: Color::Rgb(30, 40, 50),
    text: Color::Rgb(235, 219, 178),
    muted: Color::Rgb(168, 153, 132),
    dim: Color::Rgb(146, 131, 116),
    user: Color::Rgb(211, 134, 155),
    assistant: Color::Rgb(235, 219, 178),
    progress: Color::Rgb(250, 189, 47),
    tool: Color::Rgb(142, 192, 124),
    edit: Color::Rgb(254, 128, 25),
    error: Color::Rgb(251, 73, 52),
    todo: Color::Rgb(250, 189, 47),
    success: Color::Rgb(184, 187, 38),
    peer: Color::Rgb(131, 165, 152),
    selection_bg: Color::Rgb(74, 66, 58),
    added: Color::Rgb(184, 187, 38),
    added_bg: Color::Rgb(44, 50, 26),
    removed: Color::Rgb(251, 73, 52),
    removed_bg: Color::Rgb(58, 34, 30),
    lineno: Color::Rgb(146, 131, 116),
    path: Color::Rgb(131, 165, 152),
    command: Color::Rgb(250, 189, 47),
    syntax_comment: Color::Rgb(146, 131, 116),
    syntax_string: Color::Rgb(184, 187, 38),
    syntax_number: Color::Rgb(211, 134, 155),
    syntax_keyword: Color::Rgb(251, 73, 52),
    syntax_type: Color::Rgb(250, 189, 47),
    syntax_function: Color::Rgb(142, 192, 124),
    context_system: Color::Rgb(131, 165, 152),
    context_user: Color::Rgb(211, 134, 155),
    context_assistant: Color::Rgb(235, 219, 178),
    context_tool: Color::Rgb(254, 128, 25),
    context_tool_schema: Color::Rgb(177, 98, 134),
    agent_accent: Color::Rgb(131, 165, 152),
    agent_border: Color::Rgb(56, 82, 80),
    plan_accent: Color::Rgb(250, 189, 47),
    plan_border: Color::Rgb(120, 96, 40),
};

/// Nord — arctic frost dark.
const NORD: Palette = Palette {
    name: "nord",
    blurb: "Nord — arctic frost dark",
    bg: Color::Rgb(46, 52, 64),
    panel: Color::Rgb(59, 66, 82),
    panel_dark: Color::Rgb(40, 45, 56),
    border: Color::Rgb(67, 76, 94),
    border_active: Color::Rgb(136, 192, 208),
    user_block: Color::Rgb(44, 54, 72),
    assistant_block: Color::Rgb(52, 56, 66),
    thinking_block: Color::Rgb(58, 54, 42),
    tool_block: Color::Rgb(42, 58, 58),
    result_block: Color::Rgb(44, 58, 54),
    edit_block: Color::Rgb(62, 52, 44),
    todo_block: Color::Rgb(60, 58, 44),
    error_block: Color::Rgb(64, 46, 50),
    peer_block: Color::Rgb(40, 50, 74),
    text: Color::Rgb(236, 239, 244),
    muted: Color::Rgb(216, 222, 233),
    dim: Color::Rgb(76, 86, 106),
    user: Color::Rgb(129, 161, 193),
    assistant: Color::Rgb(236, 239, 244),
    progress: Color::Rgb(235, 203, 139),
    tool: Color::Rgb(143, 188, 187),
    edit: Color::Rgb(208, 135, 112),
    error: Color::Rgb(191, 97, 106),
    todo: Color::Rgb(235, 203, 139),
    success: Color::Rgb(163, 190, 140),
    peer: Color::Rgb(94, 129, 172),
    selection_bg: Color::Rgb(58, 70, 92),
    added: Color::Rgb(163, 190, 140),
    added_bg: Color::Rgb(46, 62, 50),
    removed: Color::Rgb(191, 97, 106),
    removed_bg: Color::Rgb(66, 44, 48),
    lineno: Color::Rgb(76, 86, 106),
    path: Color::Rgb(136, 192, 208),
    command: Color::Rgb(235, 203, 139),
    syntax_comment: Color::Rgb(76, 86, 106),
    syntax_string: Color::Rgb(163, 190, 140),
    syntax_number: Color::Rgb(180, 142, 173),
    syntax_keyword: Color::Rgb(129, 161, 193),
    syntax_type: Color::Rgb(143, 188, 187),
    syntax_function: Color::Rgb(136, 192, 208),
    context_system: Color::Rgb(94, 129, 172),
    context_user: Color::Rgb(129, 161, 193),
    context_assistant: Color::Rgb(236, 239, 244),
    context_tool: Color::Rgb(208, 135, 112),
    context_tool_schema: Color::Rgb(180, 142, 173),
    agent_accent: Color::Rgb(129, 161, 193),
    agent_border: Color::Rgb(74, 100, 140),
    plan_accent: Color::Rgb(235, 203, 139),
    plan_border: Color::Rgb(130, 112, 72),
};

/// Tokyo Night — neon city dark.
const TOKYONIGHT: Palette = Palette {
    name: "tokyonight",
    blurb: "Tokyo Night — neon city dark",
    bg: Color::Rgb(26, 27, 38),
    panel: Color::Rgb(41, 46, 66),
    panel_dark: Color::Rgb(22, 22, 30),
    border: Color::Rgb(59, 66, 97),
    border_active: Color::Rgb(122, 162, 247),
    user_block: Color::Rgb(28, 34, 54),
    assistant_block: Color::Rgb(32, 34, 46),
    thinking_block: Color::Rgb(44, 38, 28),
    tool_block: Color::Rgb(26, 44, 42),
    result_block: Color::Rgb(30, 44, 38),
    edit_block: Color::Rgb(48, 38, 28),
    todo_block: Color::Rgb(46, 42, 28),
    error_block: Color::Rgb(50, 30, 38),
    peer_block: Color::Rgb(26, 36, 58),
    text: Color::Rgb(192, 202, 245),
    muted: Color::Rgb(154, 164, 204),
    dim: Color::Rgb(86, 95, 137),
    user: Color::Rgb(187, 154, 247),
    assistant: Color::Rgb(192, 202, 245),
    progress: Color::Rgb(224, 175, 104),
    tool: Color::Rgb(26, 188, 156),
    edit: Color::Rgb(255, 158, 100),
    error: Color::Rgb(247, 118, 142),
    todo: Color::Rgb(224, 175, 104),
    success: Color::Rgb(158, 206, 106),
    peer: Color::Rgb(122, 162, 247),
    selection_bg: Color::Rgb(44, 50, 74),
    added: Color::Rgb(158, 206, 106),
    added_bg: Color::Rgb(28, 48, 36),
    removed: Color::Rgb(247, 118, 142),
    removed_bg: Color::Rgb(54, 32, 42),
    lineno: Color::Rgb(86, 95, 137),
    path: Color::Rgb(125, 207, 255),
    command: Color::Rgb(224, 175, 104),
    syntax_comment: Color::Rgb(86, 95, 137),
    syntax_string: Color::Rgb(158, 206, 106),
    syntax_number: Color::Rgb(255, 158, 100),
    syntax_keyword: Color::Rgb(187, 154, 247),
    syntax_type: Color::Rgb(125, 207, 255),
    syntax_function: Color::Rgb(122, 162, 247),
    context_system: Color::Rgb(122, 162, 247),
    context_user: Color::Rgb(187, 154, 247),
    context_assistant: Color::Rgb(192, 202, 245),
    context_tool: Color::Rgb(255, 158, 100),
    context_tool_schema: Color::Rgb(157, 124, 216),
    agent_accent: Color::Rgb(122, 162, 247),
    agent_border: Color::Rgb(70, 96, 150),
    plan_accent: Color::Rgb(224, 175, 104),
    plan_border: Color::Rgb(128, 100, 60),
};

/// Solarized — balanced dark.
const SOLARIZED: Palette = Palette {
    name: "solarized",
    blurb: "Solarized — balanced dark",
    bg: Color::Rgb(0, 43, 54),
    panel: Color::Rgb(7, 54, 66),
    panel_dark: Color::Rgb(0, 34, 43),
    border: Color::Rgb(25, 70, 84),
    border_active: Color::Rgb(38, 139, 210),
    user_block: Color::Rgb(4, 48, 66),
    assistant_block: Color::Rgb(8, 48, 58),
    thinking_block: Color::Rgb(24, 50, 46),
    tool_block: Color::Rgb(0, 54, 54),
    result_block: Color::Rgb(6, 54, 48),
    edit_block: Color::Rgb(30, 48, 44),
    todo_block: Color::Rgb(26, 52, 44),
    error_block: Color::Rgb(34, 42, 44),
    peer_block: Color::Rgb(2, 44, 70),
    text: Color::Rgb(147, 161, 161),
    muted: Color::Rgb(131, 148, 150),
    dim: Color::Rgb(88, 110, 117),
    user: Color::Rgb(108, 113, 196),
    assistant: Color::Rgb(147, 161, 161),
    progress: Color::Rgb(181, 137, 0),
    tool: Color::Rgb(42, 161, 152),
    edit: Color::Rgb(203, 75, 22),
    error: Color::Rgb(220, 50, 47),
    todo: Color::Rgb(181, 137, 0),
    success: Color::Rgb(133, 153, 0),
    peer: Color::Rgb(38, 139, 210),
    selection_bg: Color::Rgb(20, 64, 74),
    added: Color::Rgb(133, 153, 0),
    added_bg: Color::Rgb(18, 58, 30),
    removed: Color::Rgb(220, 50, 47),
    removed_bg: Color::Rgb(58, 30, 32),
    lineno: Color::Rgb(88, 110, 117),
    path: Color::Rgb(42, 161, 152),
    command: Color::Rgb(181, 137, 0),
    syntax_comment: Color::Rgb(88, 110, 117),
    syntax_string: Color::Rgb(42, 161, 152),
    syntax_number: Color::Rgb(211, 54, 130),
    syntax_keyword: Color::Rgb(133, 153, 0),
    syntax_type: Color::Rgb(181, 137, 0),
    syntax_function: Color::Rgb(38, 139, 210),
    context_system: Color::Rgb(38, 139, 210),
    context_user: Color::Rgb(108, 113, 196),
    context_assistant: Color::Rgb(147, 161, 161),
    context_tool: Color::Rgb(203, 75, 22),
    context_tool_schema: Color::Rgb(211, 54, 130),
    agent_accent: Color::Rgb(38, 139, 210),
    agent_border: Color::Rgb(24, 84, 126),
    plan_accent: Color::Rgb(181, 137, 0),
    plan_border: Color::Rgb(110, 84, 10),
};

/// Dracula — vivid purple dark.
const DRACULA: Palette = Palette {
    name: "dracula",
    blurb: "Dracula — vivid purple dark",
    bg: Color::Rgb(40, 42, 54),
    panel: Color::Rgb(68, 71, 90),
    panel_dark: Color::Rgb(33, 34, 44),
    border: Color::Rgb(58, 60, 78),
    border_active: Color::Rgb(189, 147, 249),
    user_block: Color::Rgb(40, 48, 66),
    assistant_block: Color::Rgb(46, 48, 60),
    thinking_block: Color::Rgb(56, 50, 38),
    tool_block: Color::Rgb(36, 56, 56),
    result_block: Color::Rgb(40, 56, 48),
    edit_block: Color::Rgb(60, 50, 40),
    todo_block: Color::Rgb(56, 56, 40),
    error_block: Color::Rgb(62, 40, 44),
    peer_block: Color::Rgb(36, 46, 70),
    text: Color::Rgb(248, 248, 242),
    muted: Color::Rgb(198, 199, 214),
    dim: Color::Rgb(98, 114, 164),
    user: Color::Rgb(189, 147, 249),
    assistant: Color::Rgb(248, 248, 242),
    progress: Color::Rgb(241, 250, 140),
    tool: Color::Rgb(139, 233, 253),
    edit: Color::Rgb(255, 184, 108),
    error: Color::Rgb(255, 85, 85),
    todo: Color::Rgb(241, 250, 140),
    success: Color::Rgb(80, 250, 123),
    peer: Color::Rgb(122, 140, 196),
    selection_bg: Color::Rgb(60, 64, 88),
    added: Color::Rgb(80, 250, 123),
    added_bg: Color::Rgb(28, 56, 38),
    removed: Color::Rgb(255, 85, 85),
    removed_bg: Color::Rgb(60, 32, 36),
    lineno: Color::Rgb(98, 114, 164),
    path: Color::Rgb(139, 233, 253),
    command: Color::Rgb(241, 250, 140),
    syntax_comment: Color::Rgb(98, 114, 164),
    syntax_string: Color::Rgb(241, 250, 140),
    syntax_number: Color::Rgb(189, 147, 249),
    syntax_keyword: Color::Rgb(255, 121, 198),
    syntax_type: Color::Rgb(139, 233, 253),
    syntax_function: Color::Rgb(80, 250, 123),
    context_system: Color::Rgb(122, 140, 196),
    context_user: Color::Rgb(189, 147, 249),
    context_assistant: Color::Rgb(248, 248, 242),
    context_tool: Color::Rgb(255, 184, 108),
    context_tool_schema: Color::Rgb(255, 121, 198),
    agent_accent: Color::Rgb(139, 175, 250),
    agent_border: Color::Rgb(78, 96, 150),
    plan_accent: Color::Rgb(241, 250, 140),
    plan_border: Color::Rgb(140, 140, 72),
};

/// High-contrast dark — maximal luminance contrast for accessibility.
const CONTRAST: Palette = Palette {
    name: "contrast",
    blurb: "High-contrast dark (accessibility)",
    bg: Color::Rgb(0, 0, 0),
    panel: Color::Rgb(20, 20, 20),
    panel_dark: Color::Rgb(10, 10, 10),
    border: Color::Rgb(85, 85, 85),
    border_active: Color::Rgb(77, 163, 255),
    user_block: Color::Rgb(10, 16, 30),
    assistant_block: Color::Rgb(22, 22, 22),
    thinking_block: Color::Rgb(30, 22, 6),
    tool_block: Color::Rgb(6, 28, 18),
    result_block: Color::Rgb(8, 26, 24),
    edit_block: Color::Rgb(32, 20, 4),
    todo_block: Color::Rgb(28, 26, 4),
    error_block: Color::Rgb(34, 10, 10),
    peer_block: Color::Rgb(6, 14, 34),
    text: Color::Rgb(255, 255, 255),
    muted: Color::Rgb(200, 200, 200),
    dim: Color::Rgb(140, 140, 140),
    user: Color::Rgb(209, 140, 255),
    assistant: Color::Rgb(255, 255, 255),
    progress: Color::Rgb(255, 230, 0),
    tool: Color::Rgb(53, 224, 224),
    edit: Color::Rgb(255, 157, 0),
    error: Color::Rgb(255, 82, 82),
    todo: Color::Rgb(255, 230, 0),
    success: Color::Rgb(0, 230, 118),
    peer: Color::Rgb(77, 163, 255),
    selection_bg: Color::Rgb(30, 40, 60),
    added: Color::Rgb(0, 230, 118),
    added_bg: Color::Rgb(4, 40, 22),
    removed: Color::Rgb(255, 82, 82),
    removed_bg: Color::Rgb(48, 10, 10),
    lineno: Color::Rgb(120, 120, 120),
    path: Color::Rgb(53, 224, 224),
    command: Color::Rgb(255, 157, 0),
    syntax_comment: Color::Rgb(140, 140, 140),
    syntax_string: Color::Rgb(0, 230, 118),
    syntax_number: Color::Rgb(255, 157, 0),
    syntax_keyword: Color::Rgb(209, 140, 255),
    syntax_type: Color::Rgb(53, 224, 224),
    syntax_function: Color::Rgb(77, 163, 255),
    context_system: Color::Rgb(77, 163, 255),
    context_user: Color::Rgb(209, 140, 255),
    context_assistant: Color::Rgb(255, 255, 255),
    context_tool: Color::Rgb(255, 157, 0),
    context_tool_schema: Color::Rgb(224, 100, 224),
    agent_accent: Color::Rgb(77, 163, 255),
    agent_border: Color::Rgb(40, 90, 150),
    plan_accent: Color::Rgb(255, 230, 0),
    plan_border: Color::Rgb(150, 120, 0),
};

/// Catppuccin Latte — soft pastel light.
const CATPPUCCIN_LATTE: Palette = Palette {
    name: "catppuccin-latte",
    blurb: "Catppuccin Latte — soft pastel light",
    bg: Color::Rgb(239, 241, 245),
    panel: Color::Rgb(230, 233, 239),
    panel_dark: Color::Rgb(220, 224, 232),
    border: Color::Rgb(188, 192, 204),
    border_active: Color::Rgb(30, 102, 245),
    user_block: Color::Rgb(222, 230, 248),
    assistant_block: Color::Rgb(230, 231, 238),
    thinking_block: Color::Rgb(244, 236, 220),
    tool_block: Color::Rgb(220, 238, 232),
    result_block: Color::Rgb(224, 240, 224),
    edit_block: Color::Rgb(250, 232, 220),
    todo_block: Color::Rgb(246, 238, 218),
    error_block: Color::Rgb(250, 226, 228),
    peer_block: Color::Rgb(216, 228, 250),
    text: Color::Rgb(76, 79, 105),
    muted: Color::Rgb(108, 111, 133),
    dim: Color::Rgb(156, 160, 176),
    user: Color::Rgb(114, 135, 253),
    assistant: Color::Rgb(76, 79, 105),
    progress: Color::Rgb(223, 142, 29),
    tool: Color::Rgb(23, 146, 153),
    edit: Color::Rgb(254, 100, 11),
    error: Color::Rgb(210, 15, 57),
    todo: Color::Rgb(223, 142, 29),
    success: Color::Rgb(64, 160, 43),
    peer: Color::Rgb(30, 102, 245),
    selection_bg: Color::Rgb(206, 220, 245),
    added: Color::Rgb(64, 160, 43),
    added_bg: Color::Rgb(214, 238, 210),
    removed: Color::Rgb(210, 15, 57),
    removed_bg: Color::Rgb(248, 218, 222),
    lineno: Color::Rgb(156, 160, 176),
    path: Color::Rgb(32, 159, 181),
    command: Color::Rgb(223, 142, 29),
    syntax_comment: Color::Rgb(156, 160, 176),
    syntax_string: Color::Rgb(64, 160, 43),
    syntax_number: Color::Rgb(254, 100, 11),
    syntax_keyword: Color::Rgb(136, 57, 239),
    syntax_type: Color::Rgb(223, 142, 29),
    syntax_function: Color::Rgb(30, 102, 245),
    context_system: Color::Rgb(30, 102, 245),
    context_user: Color::Rgb(114, 135, 253),
    context_assistant: Color::Rgb(76, 79, 105),
    context_tool: Color::Rgb(254, 100, 11),
    context_tool_schema: Color::Rgb(136, 57, 239),
    agent_accent: Color::Rgb(30, 102, 245),
    agent_border: Color::Rgb(140, 164, 224),
    plan_accent: Color::Rgb(223, 142, 29),
    plan_border: Color::Rgb(206, 180, 130),
};

/// Gruvbox — retro warm light.
const GRUVBOX_LIGHT: Palette = Palette {
    name: "gruvbox-light",
    blurb: "Gruvbox — retro warm light",
    bg: Color::Rgb(251, 241, 199),
    panel: Color::Rgb(235, 219, 178),
    panel_dark: Color::Rgb(249, 245, 215),
    border: Color::Rgb(213, 196, 161),
    border_active: Color::Rgb(66, 123, 88),
    user_block: Color::Rgb(222, 236, 238),
    assistant_block: Color::Rgb(240, 232, 206),
    thinking_block: Color::Rgb(248, 232, 186),
    tool_block: Color::Rgb(224, 238, 214),
    result_block: Color::Rgb(228, 238, 208),
    edit_block: Color::Rgb(250, 228, 196),
    todo_block: Color::Rgb(248, 232, 190),
    error_block: Color::Rgb(250, 222, 206),
    peer_block: Color::Rgb(216, 232, 240),
    text: Color::Rgb(60, 56, 54),
    muted: Color::Rgb(124, 111, 100),
    dim: Color::Rgb(146, 133, 118),
    user: Color::Rgb(143, 63, 113),
    assistant: Color::Rgb(60, 56, 54),
    progress: Color::Rgb(181, 118, 20),
    tool: Color::Rgb(66, 123, 88),
    edit: Color::Rgb(175, 58, 3),
    error: Color::Rgb(157, 0, 6),
    todo: Color::Rgb(181, 118, 20),
    success: Color::Rgb(121, 116, 14),
    peer: Color::Rgb(7, 102, 120),
    selection_bg: Color::Rgb(218, 208, 178),
    added: Color::Rgb(121, 116, 14),
    added_bg: Color::Rgb(216, 232, 196),
    removed: Color::Rgb(157, 0, 6),
    removed_bg: Color::Rgb(244, 214, 208),
    lineno: Color::Rgb(124, 111, 100),
    path: Color::Rgb(7, 102, 120),
    command: Color::Rgb(181, 118, 20),
    syntax_comment: Color::Rgb(124, 111, 100),
    syntax_string: Color::Rgb(121, 116, 14),
    syntax_number: Color::Rgb(143, 63, 113),
    syntax_keyword: Color::Rgb(157, 0, 6),
    syntax_type: Color::Rgb(181, 118, 20),
    syntax_function: Color::Rgb(66, 123, 88),
    context_system: Color::Rgb(7, 102, 120),
    context_user: Color::Rgb(143, 63, 113),
    context_assistant: Color::Rgb(60, 56, 54),
    context_tool: Color::Rgb(175, 58, 3),
    context_tool_schema: Color::Rgb(108, 48, 86),
    agent_accent: Color::Rgb(7, 102, 120),
    agent_border: Color::Rgb(120, 164, 178),
    plan_accent: Color::Rgb(181, 118, 20),
    plan_border: Color::Rgb(206, 168, 110),
};

/// Solarized — balanced light.
const SOLARIZED_LIGHT: Palette = Palette {
    name: "solarized-light",
    blurb: "Solarized — balanced light",
    bg: Color::Rgb(253, 246, 227),
    panel: Color::Rgb(238, 232, 213),
    panel_dark: Color::Rgb(247, 241, 224),
    border: Color::Rgb(147, 161, 161),
    border_active: Color::Rgb(38, 139, 210),
    user_block: Color::Rgb(224, 236, 240),
    assistant_block: Color::Rgb(240, 236, 220),
    thinking_block: Color::Rgb(250, 240, 210),
    tool_block: Color::Rgb(222, 240, 232),
    result_block: Color::Rgb(228, 240, 214),
    edit_block: Color::Rgb(250, 230, 210),
    todo_block: Color::Rgb(248, 238, 206),
    error_block: Color::Rgb(250, 224, 216),
    peer_block: Color::Rgb(216, 232, 242),
    text: Color::Rgb(88, 110, 117),
    muted: Color::Rgb(101, 123, 131),
    dim: Color::Rgb(147, 161, 161),
    user: Color::Rgb(108, 113, 196),
    assistant: Color::Rgb(88, 110, 117),
    progress: Color::Rgb(181, 137, 0),
    tool: Color::Rgb(42, 161, 152),
    edit: Color::Rgb(203, 75, 22),
    error: Color::Rgb(220, 50, 47),
    todo: Color::Rgb(181, 137, 0),
    success: Color::Rgb(133, 153, 0),
    peer: Color::Rgb(38, 139, 210),
    selection_bg: Color::Rgb(214, 226, 236),
    added: Color::Rgb(133, 153, 0),
    added_bg: Color::Rgb(224, 236, 196),
    removed: Color::Rgb(220, 50, 47),
    removed_bg: Color::Rgb(248, 216, 212),
    lineno: Color::Rgb(147, 161, 161),
    path: Color::Rgb(42, 161, 152),
    command: Color::Rgb(181, 137, 0),
    syntax_comment: Color::Rgb(147, 161, 161),
    syntax_string: Color::Rgb(42, 161, 152),
    syntax_number: Color::Rgb(211, 54, 130),
    syntax_keyword: Color::Rgb(133, 153, 0),
    syntax_type: Color::Rgb(181, 137, 0),
    syntax_function: Color::Rgb(38, 139, 210),
    context_system: Color::Rgb(38, 139, 210),
    context_user: Color::Rgb(108, 113, 196),
    context_assistant: Color::Rgb(88, 110, 117),
    context_tool: Color::Rgb(203, 75, 22),
    context_tool_schema: Color::Rgb(211, 54, 130),
    agent_accent: Color::Rgb(38, 139, 210),
    agent_border: Color::Rgb(130, 170, 206),
    plan_accent: Color::Rgb(181, 137, 0),
    plan_border: Color::Rgb(206, 176, 110),
};

/// High-contrast light — maximal luminance contrast for accessibility.
const CONTRAST_LIGHT: Palette = Palette {
    name: "contrast-light",
    blurb: "High-contrast light (accessibility)",
    bg: Color::Rgb(255, 255, 255),
    panel: Color::Rgb(240, 240, 240),
    panel_dark: Color::Rgb(247, 247, 247),
    border: Color::Rgb(153, 153, 153),
    border_active: Color::Rgb(0, 51, 204),
    user_block: Color::Rgb(228, 234, 250),
    assistant_block: Color::Rgb(238, 238, 238),
    thinking_block: Color::Rgb(250, 244, 224),
    tool_block: Color::Rgb(224, 244, 236),
    result_block: Color::Rgb(228, 246, 230),
    edit_block: Color::Rgb(250, 234, 222),
    todo_block: Color::Rgb(248, 244, 220),
    error_block: Color::Rgb(250, 228, 228),
    peer_block: Color::Rgb(222, 230, 250),
    text: Color::Rgb(0, 0, 0),
    muted: Color::Rgb(60, 60, 60),
    dim: Color::Rgb(110, 110, 110),
    user: Color::Rgb(122, 31, 162),
    assistant: Color::Rgb(0, 0, 0),
    progress: Color::Rgb(138, 109, 0),
    tool: Color::Rgb(0, 110, 110),
    edit: Color::Rgb(178, 60, 0),
    error: Color::Rgb(204, 0, 0),
    todo: Color::Rgb(138, 109, 0),
    success: Color::Rgb(0, 122, 31),
    peer: Color::Rgb(0, 51, 204),
    selection_bg: Color::Rgb(214, 224, 248),
    added: Color::Rgb(0, 122, 31),
    added_bg: Color::Rgb(216, 240, 220),
    removed: Color::Rgb(204, 0, 0),
    removed_bg: Color::Rgb(248, 218, 218),
    lineno: Color::Rgb(130, 130, 130),
    path: Color::Rgb(0, 110, 110),
    command: Color::Rgb(138, 109, 0),
    syntax_comment: Color::Rgb(110, 110, 110),
    syntax_string: Color::Rgb(0, 122, 31),
    syntax_number: Color::Rgb(178, 60, 0),
    syntax_keyword: Color::Rgb(122, 31, 162),
    syntax_type: Color::Rgb(0, 110, 110),
    syntax_function: Color::Rgb(0, 51, 204),
    context_system: Color::Rgb(0, 51, 204),
    context_user: Color::Rgb(122, 31, 162),
    context_assistant: Color::Rgb(0, 0, 0),
    context_tool: Color::Rgb(178, 60, 0),
    context_tool_schema: Color::Rgb(150, 20, 110),
    agent_accent: Color::Rgb(0, 51, 204),
    agent_border: Color::Rgb(110, 140, 210),
    plan_accent: Color::Rgb(138, 109, 0),
    plan_border: Color::Rgb(196, 168, 110),
};

/// The compile-time built-in palettes, in display order. A `static` (not
/// `const`) so its elements have stable `'static` addresses the registry can
/// reference without leaking.
static BUILTINS: [Palette; 18] = [
    // Originals.
    FOREST,
    OCEAN,
    PAPER,
    EMBER,
    SAKURA,
    GLACIER,
    DAWN,
    // Popular dark palettes + high-contrast dark.
    CATPPUCCIN,
    GRUVBOX,
    NORD,
    TOKYONIGHT,
    SOLARIZED,
    DRACULA,
    CONTRAST,
    // Light variants + high-contrast light.
    CATPPUCCIN_LATTE,
    GRUVBOX_LIGHT,
    SOLARIZED_LIGHT,
    CONTRAST_LIGHT,
];

/// Where a registered theme came from — controls picker labels and, for custom
/// themes, which directory tier shadowed which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeSource {
    /// A compile-time built-in.
    Builtin,
    /// Loaded from a project `.bonsai/themes/` (or `.claude`/`.agents`) file.
    Project,
    /// Loaded from the global `$BONSAI_HOME/themes/` directory.
    Global,
}

/// One entry in the active theme registry. Both palettes are `'static`:
/// built-ins point into [`BUILTINS`]; custom themes are `Box::leak`ed when
/// loaded.
pub(crate) struct ThemeEntry {
    /// The palette widgets actually draw (adapted to the terminal color mode).
    palette: &'static Palette,
    /// The pre-adaptation source palette, written verbatim by `/theme export`
    /// so exports never bake in color-mode downsampling. Equal to `palette`
    /// until a non-truecolor mode adapts it.
    original: &'static Palette,
    source: ThemeSource,
}

/// A `Copy` summary of a registered theme for the picker, so the render path
/// never holds the registry lock.
#[derive(Debug, Clone, Copy)]
pub struct ThemeOverview {
    pub name: &'static str,
    pub blurb: &'static str,
    pub source: ThemeSource,
}

/// The runtime theme registry: built-ins first (their indices never move), then
/// custom themes. Rebuilt in place by [`rescan_themes`]; read on every draw
/// through [`palette()`]. The write side only runs on explicit user action.
static REGISTRY: LazyLock<RwLock<Vec<ThemeEntry>>> =
    LazyLock::new(|| RwLock::new(builtin_entries()));

static CURRENT: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Builds the default registry — every built-in palette, adapted to the
/// terminal color mode here.
fn builtin_entries() -> Vec<ThemeEntry> {
    let mode = capability::color_mode();
    let mut entries = Vec::with_capacity(BUILTINS.len());
    for original in &BUILTINS {
        entries.push(make_entry(original, ThemeSource::Builtin, mode));
    }
    entries
}

/// Builds a registry entry, adapting the display palette to the terminal color
/// `mode` while keeping the un-adapted `original` for `/theme export`. In
/// truecolor mode the display palette *is* the original (no extra allocation).
fn make_entry(
    original: &'static Palette,
    source: ThemeSource,
    mode: capability::ColorMode,
) -> ThemeEntry {
    let palette: &'static Palette = if matches!(mode, capability::ColorMode::TrueColor) {
        original
    } else {
        intern_palette(capability::adapt(original, mode))
    };
    ThemeEntry {
        palette,
        original,
        source,
    }
}

/// Interns a palette by value: re-scanning an unchanged theme reuses one leaked
/// allocation instead of leaking a fresh `&'static Palette` on every rescan. The
/// pool is bounded by the number of *distinct* palettes ever seen, not by the
/// number of rescans, so a long session cycling themes has stable heap use.
fn intern_palette(palette: Palette) -> &'static Palette {
    static POOL: LazyLock<Mutex<HashSet<&'static Palette>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    let mut pool = POOL.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(&existing) = pool.get(&palette) {
        return existing;
    }
    let leaked: &'static Palette = Box::leak(Box::new(palette));
    pool.insert(leaked);
    leaked
}

/// Interns a string by content, so a custom theme's `name`/`blurb` are leaked
/// once regardless of how many times the file is re-parsed across rescans.
pub(crate) fn intern_str(value: &str) -> &'static str {
    static POOL: LazyLock<Mutex<HashSet<&'static str>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    let mut pool = POOL.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(&existing) = pool.get(value) {
        return existing;
    }
    let leaked: &'static str = Box::leak(value.to_string().into_boxed_str());
    pool.insert(leaked);
    leaked
}

fn registry() -> std::sync::RwLockReadGuard<'static, Vec<ThemeEntry>> {
    REGISTRY.read().unwrap_or_else(PoisonError::into_inner)
}

/// The active palette. Widgets must read colors through this on every draw
/// so `/theme` switches take effect on the next frame.
pub fn palette() -> &'static Palette {
    let reg = registry();
    reg[CURRENT.load(Ordering::Relaxed) % reg.len()].palette
}

/// The active theme's pre-adaptation palette — the source `/theme export` writes,
/// so exports carry true colors even under a downsampled color mode.
pub fn current_original() -> &'static Palette {
    let reg = registry();
    reg[CURRENT.load(Ordering::Relaxed) % reg.len()].original
}

pub fn current_theme_name() -> &'static str {
    palette().name
}

pub fn current_theme_index() -> usize {
    let reg = registry();
    CURRENT.load(Ordering::Relaxed) % reg.len()
}

/// Number of registered themes (built-in + custom).
pub fn theme_count() -> usize {
    registry().len()
}

/// The palette at a registry index, or `None` if out of range.
pub fn theme_at(index: usize) -> Option<&'static Palette> {
    registry().get(index).map(|entry| entry.palette)
}

/// Names of all registered themes, in registry order.
pub fn theme_names() -> Vec<&'static str> {
    registry().iter().map(|entry| entry.palette.name).collect()
}

/// `Copy` summaries of every registered theme, for the picker.
pub fn theme_overview() -> Vec<ThemeOverview> {
    registry()
        .iter()
        .map(|entry| ThemeOverview {
            name: entry.palette.name,
            blurb: entry.palette.blurb,
            source: entry.source,
        })
        .collect()
}

pub fn theme_index(name: &str) -> Option<usize> {
    let needle = name.trim().to_lowercase();
    registry()
        .iter()
        .position(|entry| entry.palette.name == needle)
}

/// Monotonic token for palette-derived caches (the transcript layout cache
/// keys rendered lines on it). Bumped by every path that can change what
/// `palette()` returns: `set_theme` (covers `/theme`, the picker's live
/// preview/apply/revert, and startup restore) and `swap_registry` (custom
/// theme rescans), plus the test-only registry mutators.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

fn bump_generation() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Activates a theme by name. Returns false (leaving the theme unchanged)
/// when the name is unknown.
pub fn set_theme(name: &str) -> bool {
    match theme_index(name) {
        Some(index) => {
            CURRENT.store(index, Ordering::Relaxed);
            bump_generation();
            true
        }
        None => false,
    }
}

#[cfg(test)]
pub(crate) fn reset_registry_for_tests() {
    *REGISTRY.write().unwrap_or_else(PoisonError::into_inner) = builtin_entries();
    CURRENT.store(0, Ordering::Relaxed);
    bump_generation();
}

/// Appends a synthetic custom theme (a renamed clone of the default) to the
/// registry so widget tests can exercise custom-theme rendering. Callers must
/// hold [`TEST_LOCK`] and reset afterwards.
#[cfg(test)]
pub(crate) fn install_custom_theme_for_tests(name: &'static str, source: ThemeSource) {
    let mut palette = *builtin_by_name("forest").expect("forest built-in exists");
    palette.name = name;
    palette.blurb = "test custom";
    let interned = intern_palette(palette);
    REGISTRY
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .push(ThemeEntry {
            palette: interned,
            original: interned,
            source,
        });
    bump_generation();
}

// ── Custom theme files ──────────────────────────────────────────────────────

/// Project and global roots for theme discovery, captured once at startup so
/// [`rescan_themes`] can re-run without re-plumbing them. Empty until
/// [`init_theme_files`] runs (so registry-only tests never touch the disk).
static THEME_ROOTS: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();

/// Looks up a built-in palette by (case-insensitive) name. `extends` resolves
/// against built-ins only — never another custom theme — so inheritance can't
/// cycle or depend on load order.
pub(crate) fn builtin_by_name(name: &str) -> Option<&'static Palette> {
    let needle = name.trim().to_lowercase();
    BUILTINS.iter().find(|palette| palette.name == needle)
}

/// Names of the built-in themes, for `extends` error messages.
pub(crate) fn builtin_names() -> Vec<&'static str> {
    BUILTINS.iter().map(|palette| palette.name).collect()
}

/// Records the discovery roots and loads custom themes for the first time.
/// Called once at startup, before the persisted theme is restored, so a
/// persisted *custom* theme resolves before the first frame. Returns one
/// human-readable message per file that failed to load.
pub fn init_theme_files(project_root: &Path, bonsai_home: &Path) -> Vec<String> {
    let _ = THEME_ROOTS.set((project_root.to_path_buf(), bonsai_home.to_path_buf()));
    rescan_themes()
}

/// Rediscovers custom themes and rebuilds the registry in place: built-ins keep
/// their slots (indices stay stable), a custom theme shadows a same-named
/// built-in, and genuinely new customs are appended in name order. The active
/// theme is re-selected by name; if it vanished, falls back to the default.
/// Returns one message per file that failed to parse. A no-op before
/// [`init_theme_files`] has run.
pub fn rescan_themes() -> Vec<String> {
    let Some((project_root, bonsai_home)) = THEME_ROOTS.get() else {
        return Vec::new();
    };
    let (entries, errors) = load_registry(project_root, bonsai_home);
    swap_registry(entries);
    errors
}

/// Builds the full registry (built-ins + valid customs) and collects load errors.
fn load_registry(project_root: &Path, bonsai_home: &Path) -> (Vec<ThemeEntry>, Vec<String>) {
    use crate::resource::discovery::{Provenance, ResourceKind, disabled_names, discover};

    let mut errors = Vec::new();
    let disabled = disabled_names(project_root, bonsai_home, ResourceKind::Themes);
    let mut entries: Vec<_> = builtin_entries()
        .into_iter()
        .filter(|entry| !is_disabled_theme(&disabled, entry.palette.name))
        .collect();
    for resource in discover(project_root, bonsai_home, ResourceKind::Themes) {
        if is_disabled_theme(&disabled, &resource.name) {
            continue;
        }
        let content = match std::fs::read_to_string(&resource.path) {
            Ok(content) => content,
            Err(err) => {
                errors.push(format!(
                    "theme '{}' ({}): {err}",
                    resource.name,
                    resource.path.display()
                ));
                continue;
            }
        };
        match spec::parse_theme(&resource.name, &resource.path, &content) {
            Ok(palette) => {
                let source = match resource.provenance {
                    Provenance::Global { .. } => ThemeSource::Global,
                    _ => ThemeSource::Project,
                };
                // Intern the parsed palette so widgets can hold a `'static` ref
                // without re-leaking identical content on every rescan.
                let original = intern_palette(palette);
                let entry = make_entry(original, source, capability::color_mode());
                match entries
                    .iter_mut()
                    .find(|existing| existing.palette.name == original.name)
                {
                    Some(slot) => *slot = entry, // shadow a same-named built-in in place
                    None => entries.push(entry),
                }
            }
            Err(err) => errors.push(err.to_string()),
        }
    }
    if entries.is_empty() {
        entries = builtin_entries();
    }
    (entries, errors)
}

fn is_disabled_theme(disabled: &std::collections::BTreeSet<String>, name: &str) -> bool {
    disabled
        .iter()
        .any(|disabled_name| disabled_name.trim().eq_ignore_ascii_case(name))
}

/// Swaps in a freshly built registry, keeping the active theme selected by name
/// across the rebuild (falling back to the first theme if it disappeared). The
/// active name is snapshotted *under the write lock* so a concurrent `set_theme`
/// can't make the swap re-select a stale index. An empty registry is refused so
/// the modulo in [`palette()`] can never divide by zero.
fn swap_registry(entries: Vec<ThemeEntry>) {
    if entries.is_empty() {
        return;
    }
    let mut reg = REGISTRY.write().unwrap_or_else(PoisonError::into_inner);
    let active = reg
        .get(CURRENT.load(Ordering::Relaxed) % reg.len().max(1))
        .map(|entry| entry.palette.name);
    *reg = entries;
    let index = active
        .and_then(|name| reg.iter().position(|entry| entry.palette.name == name))
        .unwrap_or(0);
    CURRENT.store(index, Ordering::Relaxed);
    bump_generation();
}

/// Braille spinner frames, advanced by the app tick (one frame per event-loop
/// iteration, ~50ms) wherever something is actively running.
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner_frame(tick: u64) -> &'static str {
    SPINNER[(tick % SPINNER.len() as u64) as usize]
}

pub fn base() -> Style {
    let p = palette();
    Style::default().fg(p.text).bg(p.bg)
}

pub fn panel() -> Style {
    let p = palette();
    Style::default().fg(p.text).bg(p.panel)
}

/// Foreground-only: inherits the background of whatever surface renders it.
pub fn muted() -> Style {
    Style::default().fg(palette().muted)
}

/// Foreground-only: inherits the background of whatever surface renders it.
pub fn dim() -> Style {
    Style::default().fg(palette().dim)
}

/// Carries no background so it blends into whatever row it sits on
/// (header canvas or a frame's border row).
pub fn title() -> Style {
    Style::default()
        .fg(palette().border_active)
        .add_modifier(Modifier::BOLD)
}

/// Foreground-only: inherits the background of whatever surface renders it.
pub fn label(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// Foreground-only: inherits the background of whatever surface renders it.
pub fn body(color: Color) -> Style {
    Style::default().fg(color)
}

pub fn block(fg: Color, bg: Color) -> Style {
    Style::default().fg(fg).bg(bg)
}

/// Linear RGB blend from `a` (t = 0.0) to `b` (t = 1.0), for intensity ramps
/// such as the `/usage` activity heatmap. Non-RGB colors (256-color or
/// monochrome adapted palettes) can't be interpolated, so the nearer endpoint
/// wins.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let lerp = |a: u8, b: u8| -> u8 {
                (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8
            };
            Color::Rgb(lerp(ar, br), lerp(ag, bg), lerp(ab, bb))
        }
        _ if t < 0.5 => a,
        _ => b,
    }
}

pub fn input() -> Style {
    let p = palette();
    Style::default().fg(p.text).bg(p.panel_dark)
}

pub fn composer_placeholder() -> Style {
    let p = palette();
    Style::default().fg(p.dim).bg(p.panel_dark)
}

pub fn composer_meta() -> Style {
    let p = palette();
    Style::default().fg(p.muted).bg(p.panel_dark)
}

/// A collapsed-paste chip (`[Text 1]` / `[Image 1]`) rendered inline in the
/// composer. Uses the command accent on a subtle panel fill so it reads as a
/// distinct pill rather than editable text.
pub fn composer_chip() -> Style {
    let p = palette();
    Style::default()
        .fg(p.command)
        .bg(p.panel)
        .add_modifier(Modifier::BOLD)
}

pub fn frame(frame_title: impl Into<String>, active: bool) -> UiBlock<'static> {
    let p = palette();
    UiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(if active {
            Style::default().fg(p.border_active).bg(p.bg)
        } else {
            Style::default().fg(p.border).bg(p.bg)
        })
        .title(frame_title.into())
        .title_style(if active { title() } else { muted() })
        .style(panel())
        .padding(Padding::horizontal(1))
}

/// Mode-tinted frame: blue for the agent view, yellow for plan mode. The
/// accent shows in both states (bright when focused, dim otherwise) so the
/// active mode is readable at a glance.
pub fn view_frame(
    frame_title: impl Into<String>,
    active: bool,
    view: crate::tui::event::View,
) -> UiBlock<'static> {
    let (accent, border) = view_accent(view);
    let color = if active { accent } else { border };
    UiBlock::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color).bg(palette().bg))
        .title(frame_title.into())
        .title_style(Style::default().fg(color).add_modifier(Modifier::BOLD))
        .style(panel())
        .padding(Padding::horizontal(1))
}

/// (bright, dim) accent pair for a view.
pub fn view_accent(view: crate::tui::event::View) -> (Color, Color) {
    let p = palette();
    match view {
        crate::tui::event::View::Agent => (p.agent_accent, p.agent_border),
        crate::tui::event::View::Plan => (p.plan_accent, p.plan_border),
    }
}

/// Resolve a persona color spec — a palette name (`blue`/`green`/`amber`/`red`/
/// `magenta`/`cyan`/`gray`) or a `#rrggbb` hex — to a `Color`. Names map onto
/// theme palette slots so they adapt to the active theme; an unrecognized spec
/// yields `None`, so callers fall back to the default accent.
pub fn persona_color(spec: &str) -> Option<Color> {
    let spec = spec.trim();
    if let Some(hex) = spec.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    let p = palette();
    // The core names map to theme palette slots so they adapt to the active
    // theme (built-in personas use these). Everything else resolves to a fixed
    // accent RGB from the extended palette.
    let color = match spec.to_ascii_lowercase().as_str() {
        "blue" => p.agent_accent,
        "green" => p.success,
        "amber" => p.todo,
        "red" => p.error,
        "magenta" => p.plan_accent,
        "cyan" => p.tool,
        "gray" | "grey" => p.muted,
        other => return extended_persona_color(other),
    };
    Some(color)
}

/// A broad fixed-RGB accent palette so the agent composer can offer many distinct
/// persona colors beyond the theme-mapped core names.
fn extended_persona_color(name: &str) -> Option<Color> {
    let (r, g, b) = match name {
        "sky" => (0x58, 0xc7, 0xf3),
        "indigo" => (0x72, 0x87, 0xfd),
        "teal" => (0x4e, 0xc9, 0xb0),
        "turquoise" => (0x40, 0xe0, 0xd0),
        "mint" => (0x7b, 0xe0, 0xb0),
        "lime" => (0xb9, 0xf2, 0x7c),
        "olive" => (0x9d, 0x9d, 0x36),
        "yellow" => (0xe0, 0xc7, 0x5a),
        "gold" => (0xd4, 0xaf, 0x37),
        "orange" => (0xff, 0x9e, 0x64),
        "coral" => (0xff, 0x8a, 0x65),
        "salmon" => (0xfa, 0x80, 0x72),
        "rose" => (0xf3, 0x8b, 0xa8),
        "pink" => (0xff, 0x75, 0xa0),
        "purple" => (0x9d, 0x7c, 0xd8),
        "violet" => (0xb4, 0x8e, 0xad),
        "lavender" => (0xc4, 0xb5, 0xfd),
        "brown" => (0xb0, 0x89, 0x68),
        "slate" => (0x8a, 0x9b, 0xb5),
        "white" => (0xe6, 0xe6, 0xe6),
        _ => return None,
    };
    Some(Color::Rgb(r, g, b))
}

fn parse_hex_color(hex: &str) -> Option<Color> {
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

pub fn pill(color: Color) -> Style {
    Style::default()
        .fg(color)
        .bg(palette().panel_dark)
        .add_modifier(Modifier::BOLD)
}

pub fn selection_block(fg: Color) -> Style {
    Style::default().fg(fg).bg(palette().selection_bg)
}

/// Themed scrollbar. Box-drawing glyphs are used instead of the default
/// block elements because terminals stretch them across line gaps, giving a
/// continuous bar even with extra line spacing.
pub fn scrollbar(orientation: ScrollbarOrientation) -> Scrollbar<'static> {
    let p = palette();
    let horizontal = matches!(
        orientation,
        ScrollbarOrientation::HorizontalBottom | ScrollbarOrientation::HorizontalTop
    );
    // Track uses a dotted glyph so it never visually merges with the block
    // border (which also draws `│` on its right edge). The thumb is a solid
    // bar for contrast.
    let (thumb, track) = if horizontal {
        ("━", "┄")
    } else {
        ("┃", "┊")
    };
    Scrollbar::default()
        .orientation(orientation)
        .thumb_symbol(thumb)
        .track_symbol(Some(track))
        .begin_symbol(None)
        .end_symbol(None)
        .thumb_style(Style::default().fg(p.border_active))
        .track_style(Style::default().fg(p.border))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persona_color_resolves_names_and_hex() {
        assert!(persona_color("amber").is_some());
        assert_eq!(persona_color("#e0af68"), Some(Color::Rgb(0xe0, 0xaf, 0x68)));
        assert_eq!(persona_color("  green  "), persona_color("green"));
        assert_eq!(persona_color("nonsense"), None);
        assert_eq!(persona_color("#zzzz"), None);
        assert_eq!(persona_color("#12345"), None); // wrong length
    }

    #[test]
    fn theme_lookup_is_case_insensitive_and_rejects_unknown_names() {
        let _guard = TEST_LOCK.blocking_lock();
        reset_registry_for_tests();
        assert_eq!(theme_index("forest"), Some(0));
        assert_eq!(theme_index(" Ocean "), Some(1));
        assert_eq!(theme_index("PAPER"), Some(2));
        assert_eq!(theme_index("ember"), Some(3));
        assert_eq!(theme_index("sakura"), Some(4));
        assert_eq!(theme_index("glacier"), Some(5));
        assert_eq!(theme_index("dawn"), Some(6));
        // New built-ins are appended after the originals; casing/whitespace still normalize.
        assert_eq!(theme_index("catppuccin"), Some(7));
        assert_eq!(theme_index(" Dracula "), Some(12));
        assert_eq!(theme_index("CONTRAST-LIGHT"), Some(17));
        assert_eq!(theme_index("neon"), None);
        assert!(!set_theme("neon"), "unknown theme must not switch");
    }

    #[test]
    fn registry_defaults_to_builtins_in_display_order() {
        let _guard = TEST_LOCK.blocking_lock();
        reset_registry_for_tests();
        assert_eq!(theme_count(), BUILTINS.len());
        assert_eq!(theme_names().first().copied(), Some("forest"));
        assert_eq!(theme_at(0).map(|p| p.name), Some("forest"));
        assert_eq!(theme_at(theme_count()), None);
        let overview = theme_overview();
        assert_eq!(overview.len(), BUILTINS.len());
        assert!(
            overview.iter().all(|o| o.source == ThemeSource::Builtin),
            "default registry is all built-ins"
        );
    }

    fn write_theme(dir: &Path, name: &str, body: &str) {
        let path = dir.join(".bonsai/themes").join(format!("{name}.toml"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn load_registry_appends_valid_custom_theme() {
        // Pure: touches no global state, so no TEST_LOCK needed.
        let project = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write_theme(
            project.path(),
            "mytheme",
            "extends = \"forest\"\nbg = \"#010203\"\n",
        );

        let (entries, errors) = load_registry(project.path(), home.path());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(entries.len(), BUILTINS.len() + 1);
        let custom = entries.last().unwrap();
        assert_eq!(custom.palette.name, "mytheme");
        assert_eq!(custom.palette.bg, Color::Rgb(1, 2, 3));
        assert_eq!(custom.source, ThemeSource::Project);
    }

    #[test]
    fn load_registry_reports_invalid_file_and_skips_it() {
        let project = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write_theme(project.path(), "broken", "bg = \"#101010\"\n"); // missing roles

        let (entries, errors) = load_registry(project.path(), home.path());
        assert_eq!(
            entries.len(),
            BUILTINS.len(),
            "invalid theme not registered"
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("broken"), "{}", errors[0]);
        assert!(errors[0].contains("missing roles:"), "{}", errors[0]);
    }

    #[test]
    fn custom_theme_shadows_builtin_in_place() {
        let project = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write_theme(
            project.path(),
            "forest",
            "extends = \"forest\"\nbg = \"#000000\"\n",
        );

        let (entries, errors) = load_registry(project.path(), home.path());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(entries.len(), BUILTINS.len(), "shadowing keeps the count");
        assert_eq!(entries[0].palette.name, "forest", "same slot index");
        assert_eq!(entries[0].source, ThemeSource::Project, "now a custom");
        assert_eq!(entries[0].palette.bg, Color::Rgb(0, 0, 0));
    }

    #[test]
    fn project_theme_shadows_global() {
        let project = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write_theme(
            project.path(),
            "shared",
            "extends = \"forest\"\nbg = \"#111111\"\n",
        );
        // Global tier: <home>/themes/shared.toml
        let global = home.path().join("themes");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("shared.toml"),
            "extends = \"forest\"\nbg = \"#222222\"\n",
        )
        .unwrap();

        let (entries, _errors) = load_registry(project.path(), home.path());
        let shared = entries.iter().find(|e| e.palette.name == "shared").unwrap();
        assert_eq!(
            shared.palette.bg,
            Color::Rgb(0x11, 0x11, 0x11),
            "project wins"
        );
        assert_eq!(shared.source, ThemeSource::Project);
    }

    #[test]
    fn set_custom_theme_round_trips_through_registry() {
        let _guard = TEST_LOCK.blocking_lock();
        reset_registry_for_tests();
        let project = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write_theme(
            project.path(),
            "custom",
            "extends = \"ocean\"\nbg = \"#0a0b0c\"\n",
        );

        let (entries, errors) = load_registry(project.path(), home.path());
        assert!(errors.is_empty(), "{errors:?}");
        swap_registry(entries);
        assert!(set_theme("custom"), "custom theme selectable by name");
        assert_eq!(current_theme_name(), "custom");
        assert_eq!(palette().bg, Color::Rgb(0x0a, 0x0b, 0x0c));
        reset_registry_for_tests();
    }

    #[test]
    fn intern_palette_dedupes_by_value() {
        let forest = *builtin_by_name("forest").unwrap();
        let a = intern_palette(forest);
        let b = intern_palette(forest);
        assert!(
            std::ptr::eq(a, b),
            "identical content interns to one allocation"
        );
        let mut other = forest;
        other.bg = Color::Rgb(1, 2, 3);
        assert!(
            !std::ptr::eq(a, intern_palette(other)),
            "distinct content is distinct"
        );
    }

    #[test]
    fn rescan_reuses_interned_palette_and_strings() {
        // The heart of the no-leak-per-rescan guarantee: re-scanning an unchanged
        // theme resolves to the *same* allocation, not a fresh leak.
        let project = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write_theme(
            project.path(),
            "stable",
            "extends = \"forest\"\nbg = \"#010203\"\n",
        );

        let first = load_registry(project.path(), home.path()).0;
        let second = load_registry(project.path(), home.path()).0;
        let pick = |entries: &[ThemeEntry]| {
            entries
                .iter()
                .find(|entry| entry.palette.name == "stable")
                .unwrap()
                .palette
        };
        let (p1, p2) = (pick(&first), pick(&second));
        assert!(std::ptr::eq(p1, p2), "re-scan reuses the interned palette");
        assert!(
            std::ptr::eq(p1.name, p2.name),
            "interned name is reused too"
        );
    }

    #[test]
    fn disabled_theme_is_excluded_from_registry() {
        let project = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write_theme(
            project.path(),
            "hidden",
            "extends = \"forest\"\nbg = \"#010203\"\n",
        );
        std::fs::write(project.path().join(".bonsai/themes/.disabled"), "hidden\n").unwrap();

        let (entries, errors) = load_registry(project.path(), home.path());
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            !entries.iter().any(|entry| entry.palette.name == "hidden"),
            "a .disabled theme is not registered"
        );
        assert_eq!(entries.len(), BUILTINS.len(), "only built-ins remain");
    }

    #[test]
    fn disabled_builtin_theme_is_excluded_from_registry() {
        let project = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".bonsai/themes")).unwrap();
        std::fs::write(project.path().join(".bonsai/themes/.disabled"), "forest\n").unwrap();

        let (entries, errors) = load_registry(project.path(), home.path());
        assert!(errors.is_empty(), "{errors:?}");
        assert!(!entries.is_empty(), "registry keeps a usable fallback");
        assert!(
            !entries.iter().any(|entry| entry.palette.name == "forest"),
            "a disabled built-in theme is not registered"
        );
    }

    #[test]
    fn disabled_builtins_do_not_hide_valid_custom_theme() {
        let project = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write_theme(
            project.path(),
            "custom",
            "extends = \"forest\"\nbg = \"#010203\"\n",
        );
        std::fs::write(
            project.path().join(".bonsai/themes/.disabled"),
            builtin_names().join("\n"),
        )
        .unwrap();

        let (entries, errors) = load_registry(project.path(), home.path());
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(entries.len(), 1, "only the custom theme remains");
        assert_eq!(entries[0].palette.name, "custom");
    }

    #[test]
    fn swap_registry_ignores_empty_input() {
        let _guard = TEST_LOCK.blocking_lock();
        reset_registry_for_tests();
        let before = theme_count();
        swap_registry(Vec::new());
        assert_eq!(theme_count(), before, "empty swap is a no-op");
        assert_eq!(current_theme_name(), "forest");
        reset_registry_for_tests();
    }

    #[test]
    fn theme_switch_bumps_generation() {
        let _guard = TEST_LOCK.blocking_lock();
        reset_registry_for_tests();
        let before = generation();
        assert!(set_theme("forest"));
        assert!(generation() > before, "set_theme must bump the generation");
        let before = generation();
        swap_registry(builtin_entries());
        assert!(
            generation() > before,
            "registry swap must bump the generation"
        );
        reset_registry_for_tests();
    }

    #[test]
    fn exported_file_reloads_through_discovery() {
        // Pure: render forest to a file, then discover+parse it back — the full
        // /theme export → /theme <name> round-trip minus the command plumbing.
        let project = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let forest = builtin_by_name("forest").unwrap();
        let dir = project.path().join(".bonsai/themes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("forest_copy.toml"),
            spec::render_theme_toml(forest),
        )
        .unwrap();

        let (entries, errors) = load_registry(project.path(), home.path());
        assert!(errors.is_empty(), "{errors:?}");
        let copy = entries
            .iter()
            .find(|entry| entry.palette.name == "forest_copy")
            .expect("exported theme should load");
        assert_eq!(copy.palette.bg, forest.bg);
        assert_eq!(copy.palette.syntax_keyword, forest.syntax_keyword);
        assert_eq!(copy.source, ThemeSource::Project);
    }

    #[test]
    fn vanished_active_theme_falls_back_to_default() {
        let _guard = TEST_LOCK.blocking_lock();
        reset_registry_for_tests();
        let project = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write_theme(
            project.path(),
            "temp",
            "extends = \"forest\"\nbg = \"#020202\"\n",
        );

        let (entries, _errors) = load_registry(project.path(), home.path());
        swap_registry(entries);
        assert!(set_theme("temp"));
        // Rebuild without the custom theme (as if the file were deleted).
        swap_registry(builtin_entries());
        assert_eq!(current_theme_name(), "forest", "fell back to the default");
        reset_registry_for_tests();
    }

    #[test]
    fn every_theme_defines_distinct_surfaces() {
        for theme in &BUILTINS {
            assert_ne!(theme.bg, theme.panel, "{}: bg vs panel", theme.name);
            assert_ne!(
                theme.added_bg, theme.removed_bg,
                "{}: diff backgrounds",
                theme.name
            );
            assert_ne!(
                theme.agent_accent, theme.plan_accent,
                "{}: view accents",
                theme.name
            );
            // The peer (inter-agent) lane must read as its own conversation
            // color, not blend into the user/assistant lanes.
            assert_ne!(theme.peer, theme.user, "{}: peer vs user", theme.name);
            assert_ne!(
                theme.peer, theme.assistant,
                "{}: peer vs assistant",
                theme.name
            );
            assert_ne!(
                theme.peer_block, theme.user_block,
                "{}: peer vs user block",
                theme.name
            );
            assert_ne!(
                theme.peer_block, theme.assistant_block,
                "{}: peer vs assistant block",
                theme.name
            );
        }
    }

    #[test]
    fn every_theme_defines_distinct_context_role_colors() {
        for theme in &BUILTINS {
            let colors = [
                ("system", theme.context_system),
                ("user", theme.context_user),
                ("assistant", theme.context_assistant),
                ("tool", theme.context_tool),
                ("tool schema", theme.context_tool_schema),
            ];
            for (index, (left_name, left)) in colors.iter().enumerate() {
                for (right_name, right) in colors.iter().skip(index + 1) {
                    assert_ne!(
                        left, right,
                        "{}: context {left_name} vs {right_name}",
                        theme.name
                    );
                }
            }
        }
    }

    #[test]
    fn every_theme_defines_rgb_syntax_roles() {
        // The struct literal already forces each syntax role to be set; this
        // guards that they resolve to real colors (not accidentally left as a
        // non-rgb placeholder) so highlighting re-themes correctly.
        for theme in &BUILTINS {
            for (role, color) in [
                ("comment", theme.syntax_comment),
                ("string", theme.syntax_string),
                ("number", theme.syntax_number),
                ("keyword", theme.syntax_keyword),
                ("type", theme.syntax_type),
                ("function", theme.syntax_function),
            ] {
                assert!(
                    matches!(color, Color::Rgb(..)),
                    "{}: syntax {role} must be an explicit rgb color",
                    theme.name
                );
            }
        }
    }

    #[test]
    fn text_styles_carry_no_background() {
        // The surface owns the background; a bg here would chip on any row
        // that isn't the panel (this was the header leak).
        for style in [
            muted(),
            dim(),
            title(),
            body(palette().text),
            label(palette().tool),
        ] {
            assert_eq!(style.bg, None, "text styles must inherit the surface bg");
        }
    }
}
