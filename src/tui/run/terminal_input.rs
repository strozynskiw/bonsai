use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};

/// Crossterm resolves a lone `ESC` byte as an Escape key when it is the final
/// byte in one terminal read. If an SGR mouse report is split at exactly that
/// boundary, its remaining `[<...M` bytes become ordinary key events and leak
/// into the composer. Wait only long enough to recover the rest of that one
/// terminal report; a genuine Escape key remains responsive.
const SPLIT_MOUSE_GRACE: Duration = Duration::from_millis(8);
const MAX_SGR_MOUSE_TAIL_LEN: usize = 32;

#[derive(Debug, Default)]
pub(super) struct TerminalInput {
    pending: VecDeque<Event>,
}

impl TerminalInput {
    pub(super) fn poll(&self, timeout: Duration) -> io::Result<bool> {
        if self.pending.is_empty() {
            event::poll(timeout)
        } else {
            Ok(true)
        }
    }

    pub(super) fn read(&mut self) -> io::Result<Event> {
        self.read_with(event::poll, event::read)
    }

    fn read_with(
        &mut self,
        mut poll: impl FnMut(Duration) -> io::Result<bool>,
        mut read: impl FnMut() -> io::Result<Event>,
    ) -> io::Result<Event> {
        let first = match self.pending.pop_front() {
            Some(event) => event,
            None => read()?,
        };
        if !is_plain_escape_press(&first) {
            return Ok(first);
        }

        let deadline = Instant::now() + SPLIT_MOUSE_GRACE;
        let mut tail = String::with_capacity(MAX_SGR_MOUSE_TAIL_LEN);
        let mut consumed = VecDeque::new();
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            if !poll(remaining)? {
                break;
            }

            let next = read()?;
            let Some(ch) = split_mouse_tail_char(&next) else {
                consumed.push_back(next);
                break;
            };
            consumed.push_back(next);
            tail.push(ch);

            match classify_sgr_mouse_tail(&tail) {
                SplitMouseTail::Incomplete => {}
                SplitMouseTail::Complete(mouse) => return Ok(Event::Mouse(mouse)),
                SplitMouseTail::Invalid => break,
            }
        }

        self.pending.extend(consumed);
        Ok(first)
    }
}

fn is_plain_escape_press(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    )
}

fn split_mouse_tail_char(event: &Event) -> Option<char> {
    let Event::Key(KeyEvent {
        code: KeyCode::Char(ch),
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }) = event
    else {
        return None;
    };
    let expected_modifiers = if ch.is_ascii_uppercase() {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::NONE
    };
    (*modifiers == expected_modifiers).then_some(*ch)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitMouseTail {
    Incomplete,
    Complete(MouseEvent),
    Invalid,
}

fn classify_sgr_mouse_tail(tail: &str) -> SplitMouseTail {
    const PREFIX: &str = "[<";

    if tail.len() > MAX_SGR_MOUSE_TAIL_LEN {
        return SplitMouseTail::Invalid;
    }
    if !PREFIX.starts_with(tail) && !tail.starts_with(PREFIX) {
        return SplitMouseTail::Invalid;
    }
    let Some(parameters) = tail.strip_prefix(PREFIX) else {
        return SplitMouseTail::Incomplete;
    };
    let Some(terminator) = parameters
        .chars()
        .last()
        .filter(|ch| matches!(ch, 'M' | 'm'))
    else {
        return if parameters
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b';')
            && parameters.bytes().filter(|byte| *byte == b';').count() <= 3
        {
            SplitMouseTail::Incomplete
        } else {
            SplitMouseTail::Invalid
        };
    };
    let body = &parameters[..parameters.len() - terminator.len_utf8()];
    let body = body.strip_suffix(';').unwrap_or(body);
    parse_sgr_mouse(body, terminator == 'm')
        .map(SplitMouseTail::Complete)
        .unwrap_or(SplitMouseTail::Invalid)
}

fn parse_sgr_mouse(body: &str, release: bool) -> Option<MouseEvent> {
    let mut fields = body.split(';');
    let cb = fields.next()?.parse::<u8>().ok()?;
    let column = fields.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    let row = fields.next()?.parse::<u16>().ok()?.checked_sub(1)?;
    if fields.next().is_some() {
        return None;
    }

    let button_number = (cb & 0b0000_0011) | ((cb & 0b1100_0000) >> 4);
    let dragging = cb & 0b0010_0000 != 0;
    let mut kind = match (button_number, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (3..=5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };
    if release && let MouseEventKind::Down(button) = kind {
        kind = MouseEventKind::Up(button);
    }

    let mut modifiers = KeyModifiers::NONE;
    if cb & 0b0000_0100 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if cb & 0b0000_1000 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if cb & 0b0001_0000 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }

    Some(MouseEvent {
        kind,
        column,
        row,
        modifiers,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    fn source(events: impl IntoIterator<Item = Event>) -> RefCell<VecDeque<Event>> {
        RefCell::new(events.into_iter().collect())
    }

    fn read_from(
        input: &mut TerminalInput,
        source: &RefCell<VecDeque<Event>>,
    ) -> io::Result<Event> {
        input.read_with(
            |_| Ok(!source.borrow().is_empty()),
            || {
                source.borrow_mut().pop_front().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::UnexpectedEof, "test input exhausted")
                })
            },
        )
    }

    fn split_report(tail: &str) -> Vec<Event> {
        std::iter::once(key(KeyCode::Esc, KeyModifiers::NONE))
            .chain(tail.chars().map(|ch| {
                let modifiers = if ch.is_ascii_uppercase() {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                };
                key(KeyCode::Char(ch), modifiers)
            }))
            .collect()
    }

    #[test]
    fn split_sgr_drag_is_reassembled_as_mouse_event() {
        let source = source(split_report("[<32;46;4M"));
        let mut input = TerminalInput::default();

        let event = read_from(&mut input, &source).unwrap();

        assert_eq!(
            event,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 45,
                row: 3,
                modifiers: KeyModifiers::NONE,
            })
        );
        assert!(source.borrow().is_empty());
        assert!(input.pending.is_empty());
    }

    #[test]
    fn split_mouse_burst_never_surfaces_protocol_characters() {
        let events = ["[<32;46;4M", "[<32;48;5M", "[<32;52;6M"]
            .into_iter()
            .flat_map(split_report)
            .chain(std::iter::once(key(KeyCode::Char('x'), KeyModifiers::NONE)));
        let source = source(events);
        let mut input = TerminalInput::default();

        for expected in [(45, 3), (47, 4), (51, 5)] {
            let Event::Mouse(mouse) = read_from(&mut input, &source).unwrap() else {
                panic!("split report was not recovered as a mouse event");
            };
            assert_eq!((mouse.column, mouse.row), expected);
        }
        assert_eq!(
            read_from(&mut input, &source).unwrap(),
            key(KeyCode::Char('x'), KeyModifiers::NONE)
        );
    }

    #[test]
    fn non_protocol_input_after_escape_is_preserved_in_order() {
        let source = source([
            key(KeyCode::Esc, KeyModifiers::NONE),
            key(KeyCode::Char('['), KeyModifiers::NONE),
            key(KeyCode::Char('x'), KeyModifiers::NONE),
        ]);
        let mut input = TerminalInput::default();

        assert_eq!(
            read_from(&mut input, &source).unwrap(),
            key(KeyCode::Esc, KeyModifiers::NONE)
        );
        assert_eq!(
            read_from(&mut input, &source).unwrap(),
            key(KeyCode::Char('['), KeyModifiers::NONE)
        );
        assert_eq!(
            read_from(&mut input, &source).unwrap(),
            key(KeyCode::Char('x'), KeyModifiers::NONE)
        );
    }

    #[test]
    fn split_sgr_release_preserves_button_and_modifiers() {
        let event = parse_sgr_mouse("28;8;6", true).unwrap();

        assert_eq!(event.kind, MouseEventKind::Up(MouseButton::Left));
        assert_eq!((event.column, event.row), (7, 5));
        assert_eq!(
            event.modifiers,
            KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL
        );
    }
}
