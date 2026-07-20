use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseArea {
    Composer,
    Plan,
    Transcript,
    Modal,
}

#[derive(Debug, Clone, Copy)]
pub struct LastMouseClick {
    pub at: Instant,
    pub area: MouseArea,
    pub column: u16,
    pub row: u16,
    pub count: u8,
}

const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(500);

pub fn next_mouse_click(
    prev: Option<LastMouseClick>,
    area: MouseArea,
    column: u16,
    row: u16,
) -> LastMouseClick {
    let now = Instant::now();
    let count = match prev {
        Some(last)
            if last.area == area
                && now.duration_since(last.at) <= MULTI_CLICK_WINDOW
                && last.column == column
                && last.row == row =>
        {
            (last.count + 1).min(3)
        }
        _ => 1,
    };
    LastMouseClick {
        at: now,
        area,
        column,
        row,
        count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_click_count_progresses_for_composer() {
        // First click: position. Second click: word. Third click: line.
        let first = next_mouse_click(None, MouseArea::Composer, 5, 10);
        assert_eq!(first.count, 1);
        let second = next_mouse_click(Some(first), MouseArea::Composer, 5, 10);
        assert_eq!(second.count, 2);
        let third = next_mouse_click(Some(second), MouseArea::Composer, 5, 10);
        assert_eq!(third.count, 3);
        let fourth = next_mouse_click(Some(third), MouseArea::Composer, 5, 10);
        assert_eq!(fourth.count, 3, "count must be capped at 3");
    }

    #[test]
    fn mouse_click_count_resets_on_area_change() {
        let composer_click = next_mouse_click(None, MouseArea::Composer, 5, 10);
        let next = next_mouse_click(Some(composer_click), MouseArea::Transcript, 5, 10);
        assert_eq!(next.count, 1, "area change resets click count");
    }
}
