//! The modal invariant matrix: every popup family rendered at four terminal sizes
//! and checked against the shared chrome rules — inside the area, minimum size,
//! intact frame, a title, a visible footer, horizontally centred.

use super::*;
use crate::tui::event::{DetailModal, ManagerModal, PickerModal};

/// Tool sizes the matrix runs: a roomy terminal, a normal one, a laptop split and
/// the smallest size the TUI claims to support.
const SIZES: [(u16, u16); 4] = [(150, 45), (100, 30), (70, 22), (50, 16)];

/// Assert one modal honours the shell's chrome contract at `area`.
fn assert_chrome_contract(area: Rect, kind: &ModalKind) {
    let modal = modal_area(area, kind);
    let context = format!("{kind:?} at {}x{}", area.width, area.height);
    assert!(
        modal.width <= area.width && modal.height <= area.height,
        "escapes the area: {context} -> {modal:?}"
    );
    assert!(
        modal.width >= MIN_MODAL_WIDTH.min(area.width)
            && modal.height >= MIN_MODAL_HEIGHT.min(area.height),
        "below the size floor: {context} -> {modal:?}"
    );
    assert_eq!(
        modal.x,
        area.x + (area.width - modal.width) / 2,
        "not centred: {context} -> {modal:?}"
    );

    let buffer = render_kind_to_buffer(area, kind.clone());

    // Frame intact: body text that overflowed the inner width would paint over a
    // vertical border.
    for y in modal.y + 1..modal.y + modal.height.saturating_sub(1) {
        for x in [modal.x, modal.x + modal.width - 1] {
            let symbol = buffer[(x, y)].symbol();
            assert!(
                symbol == "│" || symbol == "╎",
                "frame overwritten at ({x},{y}) with {symbol:?}: {context}"
            );
        }
    }

    // The title lives in the top border.
    let top: String = (modal.x..modal.x + modal.width)
        .map(|x| buffer[(x, modal.y)].symbol())
        .collect();
    assert!(
        top.chars().any(|ch| ch.is_alphanumeric()),
        "missing title: {context} top={top:?}"
    );

    // Hints and their spacer occupy the last inner rows.
    let last = modal.y + modal.height - 2;
    let footer = format!("{}{}", row_text(&buffer, last), row_text(&buffer, last - 1));
    assert!(
        !footer.trim().is_empty(),
        "blank footer rows: {context} modal={modal:?}"
    );
}

/// One fixture per popup family: pickers, tables, decisions, reading surfaces and
/// the list+detail manager.
fn fixture_kinds() -> Vec<ModalKind> {
    let question_options = |count: usize| {
        (0..count)
            .map(|index| crate::interaction::QuestionOption {
                label: format!("Option {index}"),
                description: format!("Description {index} with a little prose"),
                preselected: false,
            })
            .collect::<Vec<_>>()
    };
    vec![
        ModalKind::Picker(PickerModal::SessionPicker {
            sessions: vec![session_summary(1, "work", "codex", "gpt-5", "active", 3)],
            cursor: 0,
        }),
        ModalKind::Picker(PickerModal::PlanPicker {
            plans: vec![saved_plan(
                1,
                "Ship the modal system",
                Some("main"),
                "started",
            )],
            query: String::new(),
            cursor: 0,
        }),
        ModalKind::Picker(PickerModal::PlanOpenChoice {
            plan: saved_plan(1, "Ship the modal system", Some("main"), "started"),
            cursor: 1,
        }),
        ModalKind::Picker(PickerModal::StartPlanChoice { cursor: 0 }),
        ModalKind::Picker(PickerModal::ReviewScopePicker { cursor: 1 }),
        ModalKind::Picker(PickerModal::ModePicker {
            rows: vec![ModeRow::Header("agent"), ModeRow::Header("sandbox")],
            cursor: 0,
        }),
        ModalKind::Picker(PickerModal::AuthorizeProviderPicker {
            providers: vec![ProviderOption {
                provider_id: "codex".to_string(),
                provider_label: "Codex".to_string(),
                authorized: true,
                current: false,
                uses_endpoint_auth_form: false,
            }],
            query: String::new(),
            cursor: 0,
        }),
        ModalKind::Picker(PickerModal::ModelPicker {
            entries: vec![model_picker_entry("codex", "Codex", "gpt-5")],
        }),
        ModalKind::Manager(ManagerModal::TaskList {
            tasks: vec![task_snapshot(
                "t1",
                "cargo build",
                BackgroundTaskStatus::Running,
            )],
            cursor: 0,
        }),
        ModalKind::Detail(DetailModal::QuestionPrompt {
            request_id: 1,
            prompt: "Which cache strategy should this goal select?".to_string(),
            header: Some("Questions".to_string()),
            options: question_options(4),
            multiple: false,
            origin: Some("subagent explore (sub-1)".to_string()),
            cursor: 1,
            selected: vec![false; 4],
        }),
        ModalKind::Detail(DetailModal::PermissionPrompt {
            request_id: 1,
            command: "cargo test --locked".to_string(),
            origin: None,
        }),
        ModalKind::Detail(DetailModal::WebDomainPrompt {
            request_id: 1,
            url: "https://example.com/x".to_string(),
            host: "example.com".to_string(),
            redirected_from: None,
            origin: None,
        }),
        ModalKind::Detail(DetailModal::Help),
        ModalKind::Detail(DetailModal::CommandHelp),
        ModalKind::Detail(DetailModal::Context(Box::new(context_report_with_rows(6)))),
        ModalKind::Detail(DetailModal::Episodes {
            report: Box::new(context_report_with_rows(3)),
            cursor: 0,
        }),
    ]
}

#[test]
fn every_popup_keeps_the_shell_contract_at_four_sizes() {
    for (width, height) in SIZES {
        let area = Rect::new(0, 0, width, height);
        for kind in fixture_kinds() {
            assert_chrome_contract(area, &kind);
        }
    }
}

#[test]
fn every_popup_footer_advertises_escape() {
    // The dismiss key is the one hint no popup may drop: it comes straight from
    // the keymap's `Esc` arm, so a footer that loses it means the hints drifted
    // from the binding.
    let area = Rect::new(0, 0, 100, 30);
    for kind in fixture_kinds() {
        let buffer = render_kind_to_buffer(area, kind.clone());
        let modal = modal_area(area, &kind);
        let footer = (modal.y + modal.height.saturating_sub(4)..modal.y + modal.height)
            .map(|y| row_text(&buffer, y))
            .collect::<String>();
        assert!(
            footer.contains("Esc"),
            "{kind:?} footer drops the dismiss hint: {footer:?}"
        );
    }
}

#[test]
fn popups_never_touch_the_terminal_edge_on_a_wide_screen() {
    let area = Rect::new(0, 0, 100, 30);
    for kind in fixture_kinds() {
        let modal = modal_area(area, &kind);
        assert!(
            modal.y > area.y && modal.y + modal.height < area.y + area.height,
            "{kind:?} touches the terminal edge: {modal:?}"
        );
    }
}
