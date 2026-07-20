use super::*;
use crate::onboarding::FirstRunStep;
use crate::tui::widgets::bonsai::BonsaiTree;

/// Fixed seed for the wizard tree. The bonsai's silhouette is hardcoded in
/// `TreeBuilder::build`; the seed only jitters leaf tones and pad edges, so
/// any seed reads well — a constant keeps renders (and tests) deterministic.
const ONBOARDING_TREE_SEED: u32 = 0x00B0_45A1;

/// Minimum columns the side-by-side layout needs: a tree wide enough to read
/// plus a content column that fits the choice rows without heavy wrapping.
const TREE_MIN_WIDTH: u16 = crate::tui::widgets::bonsai::MIN_AREA.0;
const CONTENT_MIN_WIDTH: u16 = 48;

pub(super) fn render_onboarding(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    step: FirstRunStep,
    cursor: usize,
) {
    let block = theme::frame("Welcome to Bonsai", true);
    let inner = block.inner(area);
    let mut lines = match step {
        FirstRunStep::CredentialStorage => credential_lines(cursor),
        FirstRunStep::Provider => provider_lines(),
        FirstRunStep::Model => model_lines(app),
        FirstRunStep::WorkspaceTrust => workspace_trust_lines(cursor),
        FirstRunStep::Sandbox => sandbox_lines(app, cursor),
        FirstRunStep::Autonomy => autonomy_lines(cursor),
        FirstRunStep::FirstPrompt => first_prompt_lines(),
    };
    lines.insert(0, setup_progress_line(step));
    lines.insert(1, Line::default());
    lines.push(Line::from(""));
    lines.push(super::common::footer_hint_line(&[
        ("Up/Down", "choose"),
        ("Enter", "continue"),
        ("Esc", "later"),
    ]));

    f.render_widget(block, area);
    let show_tree = inner.width >= TREE_MIN_WIDTH + CONTENT_MIN_WIDTH
        && inner.height > crate::tui::widgets::bonsai::MIN_AREA.1;
    if show_tree {
        // The real procedural bonsai (the site hero) grows with setup: each
        // completed step advances its reveal, and finishing the wizard is the
        // fully grown tree. Tree on the left, step content on the right.
        // Content wins the width contest: choice rows read poorly when
        // wrapped, while the tree scales down gracefully. Only columns beyond
        // a comfortable reading width (64) go to the tree, and past ~40 cols
        // the tree hits MAX_SCALE and would only gain empty margin anyway.
        let tree_width = inner.width.saturating_sub(64).clamp(TREE_MIN_WIDTH, 40);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(tree_width), Constraint::Min(0)])
            .split(inner);
        let tree_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(columns[0]);
        let progress = step.number() as f64 / FirstRunStep::TOTAL as f64;
        BonsaiTree::generate(
            tree_rows[0].width,
            tree_rows[0].height,
            ONBOARDING_TREE_SEED,
        )
        .render(tree_rows[0], f.buffer_mut(), progress);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(step_caption(step), theme::dim())))
                .alignment(ratatui::layout::Alignment::Center),
            tree_rows[1],
        );
        f.render_widget(
            Paragraph::new(lines.clone())
                .style(theme::panel())
                .wrap(Wrap { trim: false }),
            vertically_centered(centered_reading_area(columns[1]), lines.len() as u16),
        );
    } else {
        f.render_widget(
            Paragraph::new(lines)
                .style(theme::panel())
                .wrap(Wrap { trim: false }),
            inner,
        );
    }
}

/// One quiet line of bonsai wisdom per step, matched to what the step does:
/// soil (storage), light (provider), seed (model), roots (trust), pruning
/// (sandbox), tending (autonomy), growth (first prompt).
fn step_caption(step: FirstRunStep) -> &'static str {
    match step {
        FirstRunStep::CredentialStorage => "start with good soil",
        FirstRunStep::Provider => "choose your light",
        FirstRunStep::Model => "pick your seed",
        FirstRunStep::WorkspaceTrust => "let roots take hold",
        FirstRunStep::Sandbox => "prune with care",
        FirstRunStep::Autonomy => "decide how closely to tend",
        FirstRunStep::FirstPrompt => "now, watch it grow",
    }
}

fn centered_reading_area(area: Rect) -> Rect {
    let width = area.width.min(84);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y,
        width,
        area.height,
    )
}

/// Center a known-height block vertically so short steps don't hug the top
/// edge while the tree fills the full column beside them.
fn vertically_centered(area: Rect, content_height: u16) -> Rect {
    let offset = area.height.saturating_sub(content_height) / 2;
    Rect::new(
        area.x,
        area.y.saturating_add(offset),
        area.width,
        area.height.saturating_sub(offset),
    )
}

fn setup_progress_line(step: FirstRunStep) -> Line<'static> {
    let palette = theme::palette();
    let total = FirstRunStep::TOTAL;
    let mut spans = Vec::with_capacity(total * 2 + 1);
    for index in 1..=total {
        let complete = index <= step.number();
        spans.push(Span::styled(
            if complete { "●" } else { "○" },
            theme::body(if complete {
                palette.success
            } else {
                palette.dim
            }),
        ));
        if index < total {
            spans.push(Span::styled("  ", theme::dim()));
        }
    }
    spans.push(Span::styled(
        format!("    step {} of {total}", step.number()),
        theme::muted(),
    ));
    Line::from(spans).centered()
}

fn credential_lines(cursor: usize) -> Vec<Line<'static>> {
    let choices = crate::session::CredentialPersistence::ALL;
    let details = [
        file_storage_detail(),
        "stronger OS isolation; may prompt",
        "memory only; cleared when Bonsai exits",
    ];
    let mut lines = intro(
        "Choose credential storage",
        "Where new provider credentials are kept. You can override it per provider.",
    );
    for (index, persistence) in choices.iter().enumerate() {
        lines.push(choice_line(
            index == cursor,
            persistence.label(),
            details[index],
        ));
    }
    lines
}

fn provider_lines() -> Vec<Line<'static>> {
    let mut lines = intro(
        "Connect a provider",
        "A provider is where your prompts run: Anthropic, OpenAI, a gateway, or your own machine.",
    );
    lines.push(hint_line("/authorize", "pick a provider and sign in"));
    lines.push(hint_line(
        "/wizard",
        "add a local model server (Ollama, LM Studio…)",
    ));
    lines.push(Line::from(""));
    lines.push(choice_line(
        true,
        "Continue",
        "connect whenever you're ready",
    ));
    lines
}

fn model_lines(app: &AppState) -> Vec<Line<'static>> {
    let mut lines = intro(
        "Choose a model",
        "Each provider starts on a sensible default; switching is one command away.",
    );
    lines.push(Line::from(vec![
        Span::styled("Current  ", theme::muted()),
        Span::styled(
            format!("{} / {}", app.provider, app.model),
            theme::body(theme::palette().text),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(hint_line(
        "/model",
        "pick model + reasoning effort, mid-session too",
    ));
    lines.push(hint_line(
        "/smol",
        "SMOL mode: trims tools + context for small models",
    ));
    lines.push(Line::from(""));
    lines.push(choice_line(
        true,
        "Continue",
        "keep the current model for now",
    ));
    lines
}

fn workspace_trust_lines(cursor: usize) -> Vec<Line<'static>> {
    let mut lines = intro(
        "Choose workspace trust",
        "Allow this repository to load its own tools and instructions.",
    );
    lines.push(choice_line(
        cursor == 0,
        "Trust this workspace",
        "enable project features on the next launch",
    ));
    lines.push(choice_line(
        cursor == 1,
        "Keep restricted",
        "use built-ins and your global configuration",
    ));
    lines
}

fn sandbox_lines(app: &AppState, cursor: usize) -> Vec<Line<'static>> {
    let backend = app.sandbox.as_ref().map_or(("detecting", true), |sandbox| {
        let backend = sandbox.backend();
        (backend.label(), backend.is_available())
    });
    let (backend_label, backend_available) = backend;
    let mut lines = intro(
        "Choose a sandbox posture",
        "The sandbox confines commands at the OS level, independent of approvals.",
    );
    lines.push(key_value_line("Backend", backend_label));
    if !backend_available {
        lines.push(Line::from(Span::styled(
            "No sandbox backend is available here; approval rules still protect you.",
            theme::dim(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(choice_line(
        cursor == 0,
        "Sandbox, no network",
        "recommended — network egress blocked",
    ));
    lines.push(choice_line(
        cursor == 1,
        "Sandbox with network",
        "confined, network egress allowed",
    ));
    lines.push(choice_line(
        cursor == 2,
        "No sandbox",
        "approval prompts only",
    ));
    lines.push(Line::from(""));
    lines.push(hint_line(
        "/sandbox",
        "change anytime; /sandbox net on|off for egress",
    ));
    lines
}

fn autonomy_lines(cursor: usize) -> Vec<Line<'static>> {
    let mut lines = intro(
        "Choose autonomy",
        "How much Bonsai does without asking. Everything short of yolo keeps the safety floor.",
    );
    for (index, level) in crate::tool::ApprovalLevel::ALL.iter().enumerate() {
        lines.push(choice_line(index == cursor, level.label(), level.summary()));
    }
    lines.push(Line::from(""));
    lines.push(hint_line(
        "/autonomy",
        "change anytime; /yolo toggles full autonomy on and off",
    ));
    lines
}

fn first_prompt_lines() -> Vec<Line<'static>> {
    let mut lines = intro(
        "Run your first task",
        "Setup completes after Bonsai finishes one real prompt.",
    );
    lines.push(choice_line(
        true,
        "Write first prompt",
        "return to the composer and start coding",
    ));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Everything you just set has a command:",
        theme::muted(),
    )));
    lines.push(hint_line("/authorize", "providers and sign-in"));
    lines.push(hint_line("/model", "model and reasoning effort"));
    lines.push(hint_line("/sandbox", "confinement and network"));
    lines.push(hint_line(
        "/autonomy",
        "approval level (/yolo for all of it)",
    ));
    lines.push(hint_line("/wizard", "local and custom providers"));
    lines.push(hint_line("/smol", "small-model profile"));
    lines.push(hint_line("/help", "everything else"));
    lines
}

fn intro(title: &'static str, detail: &'static str) -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(title, theme::body(theme::palette().text))),
        Line::from(Span::styled(detail, theme::dim())),
        Line::from(""),
    ]
}

fn choice_line(selected: bool, label: &str, detail: &str) -> Line<'static> {
    let palette = theme::palette();
    Line::from(vec![
        Span::styled(
            if selected { "> " } else { "  " },
            theme::body(if selected {
                palette.success
            } else {
                palette.muted
            }),
        ),
        Span::styled(
            format!("{label:<24}"),
            if selected {
                theme::label(palette.text)
            } else {
                theme::body(palette.text)
            },
        ),
        Span::styled(detail.to_string(), theme::dim()),
    ])
}

/// A command teaser: the slash command in the tool accent (matching /help),
/// the payoff dimmed.
fn hint_line(command: &'static str, detail: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ", theme::dim()),
        Span::styled(
            format!("{command:<12}"),
            theme::label(theme::palette().tool),
        ),
        Span::styled(detail, theme::dim()),
    ])
}

fn key_value_line(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<13}"), theme::muted()),
        Span::styled(value.to_string(), theme::body(theme::palette().text)),
    ])
}

#[cfg(windows)]
fn file_storage_detail() -> &'static str {
    "encrypted with Windows DPAPI"
}

#[cfg(not(windows))]
fn file_storage_detail() -> &'static str {
    "secure local file; works everywhere"
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn rendered(step: FirstRunStep, cursor: usize) -> String {
        rendered_at_size(step, cursor, 100, 28)
    }

    fn rendered_at_size(step: FirstRunStep, cursor: usize, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        let app = AppState::new("codex", "gpt-test".to_string(), ".".to_string(), None);
        terminal
            .draw(|frame| render_onboarding(frame, area, &app, step, cursor))
            .expect("onboarding should render");
        let buffer = terminal.backend().buffer();
        let mut output = String::new();
        for y in 0..height {
            for x in 0..width {
                output.push_str(buffer[(x, y)].symbol());
            }
            output.push('\n');
        }
        output
    }

    #[test]
    fn credential_step_explains_all_choices_and_marks_only_selection() {
        let text = rendered(FirstRunStep::CredentialStorage, 1);
        assert!(text.contains("protected file"));
        assert!(text.contains("> OS credential store"));
        assert!(text.contains("this session only"));
        assert!(text.contains("Esc later"));
    }

    #[test]
    fn guided_steps_name_every_release_requirement() {
        let cases = [
            (FirstRunStep::Provider, "Connect a provider"),
            (FirstRunStep::Model, "Choose a model"),
            (FirstRunStep::WorkspaceTrust, "Choose workspace trust"),
            (FirstRunStep::Sandbox, "Choose a sandbox posture"),
            (FirstRunStep::Autonomy, "Choose autonomy"),
            (FirstRunStep::FirstPrompt, "Run your first task"),
        ];
        for (step, expected) in cases {
            assert!(rendered(step, 0).contains(expected));
        }
    }

    #[test]
    fn provider_step_teaches_authorize_instead_of_launching_it() {
        let text = rendered(FirstRunStep::Provider, 0);
        assert!(text.contains("/authorize"));
        assert!(text.contains("/wizard"));
        assert!(text.contains("Ollama"));
        assert!(
            text.contains("> Continue"),
            "the only action is moving on; /authorize is taught, not run"
        );
    }

    #[test]
    fn model_step_teaches_model_command_instead_of_opening_the_picker() {
        let text = rendered(FirstRunStep::Model, 0);
        assert!(text.contains("/model"));
        assert!(text.contains("/smol"));
        assert!(text.contains("SMOL mode"));
        assert!(text.contains("codex / gpt-test"), "shows the kept default");
        assert!(
            text.contains("> Continue"),
            "the only action is moving on; /model is taught, not opened"
        );
    }

    #[test]
    fn sandbox_step_offers_three_postures_and_the_command() {
        let text = rendered(FirstRunStep::Sandbox, 1);
        assert!(text.contains("Sandbox, no network"));
        assert!(text.contains("> Sandbox with network"));
        assert!(text.contains("No sandbox"));
        assert!(text.contains("/sandbox net"));
    }

    #[test]
    fn autonomy_step_lists_every_level_with_yolo_called_out() {
        let text = rendered(FirstRunStep::Autonomy, 2);
        for level in crate::tool::ApprovalLevel::ALL {
            assert!(
                text.contains(level.label()),
                "missing autonomy level {}",
                level.label()
            );
        }
        assert!(text.contains("> balanced"), "default should be balanced");
        assert!(text.contains("/yolo"));
    }

    #[test]
    fn first_prompt_step_recaps_the_commands() {
        let text = rendered(FirstRunStep::FirstPrompt, 0);
        for command in ["/authorize", "/model", "/sandbox", "/autonomy", "/help"] {
            assert!(text.contains(command), "missing recap for {command}");
        }
    }

    /// The procedural tree paints half-block cells; counting them tracks how
    /// much of the bonsai is revealed at a given step.
    fn tree_cells(text: &str) -> usize {
        text.chars().filter(|&c| c == '▀' || c == '▄').count()
    }

    #[test]
    fn bonsai_grows_as_setup_advances() {
        let sprout = rendered(FirstRunStep::CredentialStorage, 0);
        let mature = rendered(FirstRunStep::FirstPrompt, 0);

        let (early, full) = (tree_cells(&sprout), tree_cells(&mature));
        assert!(early > 0, "step 1 should already show the pot and roots");
        assert!(
            full > early,
            "the tree must keep growing: step 1 = {early} cells, step 6 = {full} cells"
        );
        assert!(sprout.contains("start with good soil"));
        assert!(mature.contains("now, watch it grow"));
    }

    /// Eyeball the wizard layout in a real terminal:
    /// `cargo test onboarding_visual_dump -- --ignored --nocapture`
    #[test]
    #[ignore = "visual inspection helper, not an assertion"]
    fn onboarding_visual_dump() {
        for step in [
            FirstRunStep::CredentialStorage,
            FirstRunStep::Sandbox,
            FirstRunStep::Autonomy,
            FirstRunStep::FirstPrompt,
        ] {
            println!("{}", rendered_at_size(step, step.default_choice(), 120, 32));
        }
    }

    #[test]
    fn narrow_layout_drops_the_tree_but_keeps_setup_controls() {
        let text = rendered_at_size(FirstRunStep::Provider, 0, 60, 18);

        assert_eq!(
            tree_cells(&text),
            0,
            "no room for a readable tree at 60 cols"
        );
        assert!(text.contains("Connect a provider"));
    }
}
