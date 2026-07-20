use ratatui::layout::{Constraint, Direction, Layout, Rect};

use crate::agent::PersonaView;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppLayout {
    pub header: Rect,
    /// Transcript area: the full conversation in the agent view, the
    /// narrower chat column in the plan view.
    pub main: Rect,
    /// Plan canvas, present only in the plan view.
    pub plan: Option<Rect>,
    pub sidebar: Option<Rect>,
    /// Fixed-height bordered composer.
    pub input: Rect,
    /// Status and contextual key hints below the composer border.
    pub input_footer: Rect,
    pub show_input: bool,
}

pub fn split(area: Rect, surface: PersonaView) -> AppLayout {
    const COMPOSER_HEIGHT: u16 = 4;
    const FOOTER_HEIGHT: u16 = 1;
    let input_height = COMPOSER_HEIGHT + FOOTER_HEIGHT;
    let show_input = area.height >= 18;
    // The header is a single status/brand row; the agent/plan switch lives in
    // the composer footer, so there is no second tab row to account for.
    let header_height = 1;

    let vertical_constraints = if show_input {
        vec![
            Constraint::Length(header_height),
            Constraint::Min(1),
            Constraint::Length(input_height),
        ]
    } else {
        vec![Constraint::Length(header_height), Constraint::Min(1)]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(vertical_constraints)
        .split(area);

    let mut cursor = 0;
    let header = chunks[cursor];
    cursor += 1;
    let main = chunks[cursor];
    cursor += 1;
    let input_band = if show_input { chunks[cursor] } else { area };
    let (input, input_footer) = if show_input {
        let input_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(COMPOSER_HEIGHT),
                Constraint::Length(FOOTER_HEIGHT),
            ])
            .split(input_band);
        (input_chunks[0], input_chunks[1])
    } else {
        (area, Rect::default())
    };

    // Canvas surface: chat keeps the left column, the canvas takes the larger
    // right pane, and the sidebar yields its space to the canvas.
    if matches!(surface, PersonaView::Canvas) {
        // The chat column is read-only while planning — the planner researches
        // and drafts on the canvas — so the canvas earns the larger share (2/3).
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Fill(1), Constraint::Fill(2)])
            .split(main);
        return AppLayout {
            header,
            main: body[0],
            plan: Some(body[1]),
            sidebar: None,
            input,
            input_footer,
            show_input,
        };
    }

    // The TODO sidebar belongs only to the `todo` surface; `chat` is bare.
    let sidebar_visible =
        matches!(surface, PersonaView::Todo) && area.width >= 104 && main.width >= 72;
    let (main, sidebar) = if sidebar_visible {
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(52), Constraint::Length(34)])
            .split(main);
        (body[0], Some(body[1]))
    } else {
        (main, None)
    };

    AppLayout {
        header,
        main,
        plan: None,
        sidebar,
        input,
        input_footer,
        show_input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_sidebar_in_narrow_terminals() {
        let layout = split(Rect::new(0, 0, 80, 24), PersonaView::Todo);

        assert!(layout.sidebar.is_none());
        assert_eq!(layout.main.width, 80);
    }

    #[test]
    fn shows_sidebar_when_space_allows() {
        let layout = split(Rect::new(0, 0, 120, 32), PersonaView::Todo);

        assert!(layout.sidebar.is_some());
        assert_eq!(layout.sidebar.unwrap().width, 34);
    }

    #[test]
    fn hides_input_on_short_terminals() {
        let layout = split(Rect::new(0, 0, 100, 14), PersonaView::Todo);
        assert!(!layout.show_input);
    }

    #[test]
    fn shows_input_on_medium_terminals() {
        let layout = split(Rect::new(0, 0, 100, 20), PersonaView::Todo);
        assert!(layout.show_input);
    }

    #[test]
    fn input_and_footer_have_fixed_two_row_geometry() {
        let small = split(Rect::new(0, 0, 100, 20), PersonaView::Todo);
        let medium = split(Rect::new(0, 0, 100, 28), PersonaView::Todo);
        let large = split(Rect::new(0, 0, 100, 40), PersonaView::Todo);
        for layout in [small, medium, large] {
            assert_eq!(layout.input.height, 4);
            assert_eq!(layout.input_footer.height, 1);
            assert_eq!(layout.input_footer.y, layout.input.bottom());
            assert_eq!(layout.main.bottom(), layout.input.y);
        }
    }

    #[test]
    fn plan_view_splits_chat_and_canvas_without_sidebar() {
        let layout = split(Rect::new(0, 0, 120, 32), PersonaView::Canvas);

        let plan = layout.plan.expect("plan view should have a canvas");
        assert!(layout.sidebar.is_none());
        assert!(
            plan.width > layout.main.width,
            "canvas should be the bigger pane: chat={} canvas={}",
            layout.main.width,
            plan.width
        );
        assert_eq!(plan.width + layout.main.width, 120);
    }

    #[test]
    fn agent_view_has_no_plan_pane() {
        let layout = split(Rect::new(0, 0, 120, 32), PersonaView::Todo);
        assert!(layout.plan.is_none());
    }

    #[test]
    fn chat_surface_has_neither_sidebar_nor_canvas() {
        // `chat` is bare even on a wide terminal that would otherwise fit the
        // TODO sidebar.
        let layout = split(Rect::new(0, 0, 120, 32), PersonaView::Chat);
        assert!(layout.sidebar.is_none());
        assert!(layout.plan.is_none());
    }
}
