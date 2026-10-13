//! Streaming splitter for models that emit their reasoning inline in the text
//! channel as `<think>…</think>` blocks (e.g. MiniMax and many open models)
//! instead of on a dedicated reasoning channel.
//!
//! Providers feed each text delta through [`ThinkTagSplitter::push`], which
//! returns the visible answer text and the reasoning text to forward to
//! `assistant_delta` / `reasoning_delta` respectively. Because the tags can be
//! split across stream chunks (`<thi` then `nk>`), the splitter buffers a short
//! tail that could be the start of a tag and only commits it once it knows
//! whether a tag completed. Call [`ThinkTagSplitter::finish`] at end of stream
//! to flush whatever remains.
//!
//! Only one leading block is eligible. Once answer text starts, all later tags
//! are literal content, including code examples. Transports with native
//! reasoning use [`ThinkTagSplitter::use_native_reasoning_channel`] before
//! feeding answer text, preserving even a literal tag at its start.

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// Text routed out of one [`ThinkTagSplitter::push`]/[`finish`](ThinkTagSplitter::finish) call.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ThinkSplit {
    /// Visible assistant answer text, with later literal tags preserved.
    pub visible: String,
    /// Text inside the leading inline reasoning block.
    pub reasoning: String,
}

#[derive(Debug, Default)]
pub(crate) struct ThinkTagSplitter {
    state: ThinkState,
    /// Bytes held back because they might be the leading part of a split tag.
    pending: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ThinkState {
    #[default]
    Prefix,
    Reasoning,
    Answer,
}

impl ThinkTagSplitter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Native reasoning makes the text channel an answer channel. Flush a
    /// possible opening-tag fragment literally before any later answer delta.
    /// If a legacy inline block already opened, finish that block normally;
    /// streamed reasoning cannot be retroactively reclassified.
    pub(crate) fn use_native_reasoning_channel(&mut self) -> ThinkSplit {
        if self.state == ThinkState::Reasoning {
            return ThinkSplit::default();
        }
        self.state = ThinkState::Answer;
        ThinkSplit {
            visible: std::mem::take(&mut self.pending),
            reasoning: String::new(),
        }
    }

    /// Feed a text delta, returning the visible and reasoning text it yields.
    pub(crate) fn push(&mut self, chunk: &str) -> ThinkSplit {
        if self.state == ThinkState::Answer {
            return ThinkSplit {
                visible: chunk.to_string(),
                reasoning: String::new(),
            };
        }
        self.pending.push_str(chunk);
        let mut out = ThinkSplit::default();
        loop {
            match self.state {
                ThinkState::Prefix => {
                    let after_whitespace = self.pending.trim_start_matches(char::is_whitespace);
                    let leading = self.pending.len() - after_whitespace.len();
                    out.visible.push_str(&self.pending[..leading]);
                    if after_whitespace.starts_with(OPEN) {
                        self.pending.drain(..leading + OPEN.len());
                        self.state = ThinkState::Reasoning;
                    } else if OPEN.starts_with(after_whitespace) {
                        // Hold only a possible opening tag, never an arbitrary
                        // amount of whitespace or answer text.
                        self.pending.drain(..leading);
                        break;
                    } else {
                        self.pending.drain(..leading);
                        self.state = ThinkState::Answer;
                    }
                }
                ThinkState::Reasoning => {
                    if let Some(idx) = self.pending.find(CLOSE) {
                        out.reasoning.push_str(&self.pending[..idx]);
                        self.pending.drain(..idx + CLOSE.len());
                        self.state = ThinkState::Answer;
                        continue;
                    }
                    let keep = partial_suffix(&self.pending, CLOSE);
                    let cut = self.pending.len() - keep;
                    out.reasoning.push_str(&self.pending[..cut]);
                    self.pending.drain(..cut);
                    break;
                }
                ThinkState::Answer => {
                    out.visible.push_str(&self.pending);
                    self.pending.clear();
                    break;
                }
            }
        }
        out
    }

    /// Flush any buffered tail at end of stream so no text is dropped. An
    /// unterminated `<think>` is surfaced as reasoning (the block was opened but
    /// the stream ended before it closed).
    pub(crate) fn finish(&mut self) -> ThinkSplit {
        let rest = std::mem::take(&mut self.pending);
        let mut out = ThinkSplit::default();
        if self.state == ThinkState::Reasoning {
            out.reasoning = rest;
        } else {
            out.visible = rest;
        }
        out
    }
}

/// Length of the longest suffix of `s` that is a *proper* prefix of `tag`, so a
/// tag split across chunk boundaries is held back rather than emitted as literal
/// text. Returns 0 when no suffix could begin the tag.
fn partial_suffix(s: &str, tag: &str) -> usize {
    let max = (tag.len() - 1).min(s.len());
    for len in (1..=max).rev() {
        let start = s.len() - len;
        if s.is_char_boundary(start) && tag.starts_with(&s[start..]) {
            return len;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a sequence of chunks through one splitter and concatenate output.
    fn run(chunks: &[&str]) -> ThinkSplit {
        let mut splitter = ThinkTagSplitter::new();
        let mut out = ThinkSplit::default();
        for chunk in chunks {
            let split = splitter.push(chunk);
            out.visible.push_str(&split.visible);
            out.reasoning.push_str(&split.reasoning);
        }
        let tail = splitter.finish();
        out.visible.push_str(&tail.visible);
        out.reasoning.push_str(&tail.reasoning);
        out
    }

    #[test]
    fn splits_single_block_in_one_chunk() {
        let out = run(&["<think>reasoning here</think>the answer"]);
        assert_eq!(out.reasoning, "reasoning here");
        assert_eq!(out.visible, "the answer");
    }

    #[test]
    fn tags_split_across_chunks_are_not_leaked() {
        // The classic streaming case: tags arrive in pieces.
        let out = run(&["<thi", "nk>hidden", " thoughts</thi", "nk>vis", "ible"]);
        assert_eq!(out.reasoning, "hidden thoughts");
        assert_eq!(out.visible, "visible");
    }

    #[test]
    fn plain_text_passes_through_untouched() {
        let out = run(&["just ", "a normal ", "answer"]);
        assert_eq!(out.visible, "just a normal answer");
        assert_eq!(out.reasoning, "");
    }

    #[test]
    fn tags_after_answer_text_stay_literal() {
        let out = run(&["hi <think>hmm</think> there"]);
        assert_eq!(out.visible, "hi <think>hmm</think> there");
        assert_eq!(out.reasoning, "");
    }

    #[test]
    fn later_think_blocks_are_answer_content() {
        let out = run(&["<think>one</think>A<think>two</think>B"]);
        assert_eq!(out.reasoning, "one");
        assert_eq!(out.visible, "A<think>two</think>B");
    }

    #[test]
    fn unterminated_think_is_flushed_as_reasoning() {
        let out = run(&["<think>still thinking when the stream ends"]);
        assert_eq!(out.reasoning, "still thinking when the stream ends");
        assert_eq!(out.visible, "");
    }

    #[test]
    fn lone_angle_bracket_is_not_held_forever() {
        // A `<` that never becomes a tag must still surface as visible text.
        let out = run(&["a < b is true"]);
        assert_eq!(out.visible, "a < b is true");
        assert_eq!(out.reasoning, "");
    }

    #[test]
    fn multibyte_text_around_partial_tag() {
        // A held-back partial-tag suffix must not slice through a multibyte char.
        let out = run(&["café <", "think>π</think> δ"]);
        assert_eq!(out.visible, "café <think>π</think> δ");
        assert_eq!(out.reasoning, "");
    }

    #[test]
    fn initial_whitespace_does_not_hide_the_prefix() {
        let out = run(&["\n\t", "<th", "ink>π</th", "ink> café"]);
        assert_eq!(out.visible, "\n\t café");
        assert_eq!(out.reasoning, "π");
    }

    #[test]
    fn markdown_example_is_preserved_across_every_chunk_boundary() {
        let text = "```xml\n<think>café π</think>\n```";
        for boundary in (0..=text.len()).filter(|offset| text.is_char_boundary(*offset)) {
            let out = run(&[&text[..boundary], &text[boundary..]]);
            assert_eq!(out.visible, text, "boundary {boundary}");
            assert!(out.reasoning.is_empty());
        }
    }

    #[test]
    fn reasoning_prefix_then_literal_example_is_chunk_invariant() {
        let text = "<think>π café</think><think>literal example</think>";
        for boundary in (0..=text.len()).filter(|offset| text.is_char_boundary(*offset)) {
            let out = run(&[&text[..boundary], &text[boundary..]]);
            assert_eq!(
                out.visible, "<think>literal example</think>",
                "boundary {boundary}"
            );
            assert_eq!(out.reasoning, "π café", "boundary {boundary}");
        }
    }

    #[test]
    fn native_channel_preserves_an_opening_tag_fragment() {
        let mut splitter = ThinkTagSplitter::new();
        assert_eq!(splitter.push("<thi"), ThinkSplit::default());
        assert_eq!(splitter.use_native_reasoning_channel().visible, "<thi");
        let next = splitter.push("nk>literal</think>");
        assert_eq!(next.visible, "nk>literal</think>");
        assert!(next.reasoning.is_empty());
    }

    #[test]
    fn native_channel_does_not_reclassify_started_inline_reasoning() {
        let mut splitter = ThinkTagSplitter::new();
        assert_eq!(splitter.push("<think>π</thi").reasoning, "π");
        assert_eq!(
            splitter.use_native_reasoning_channel(),
            ThinkSplit::default()
        );
        let next = splitter.push("nk><think>literal</think>");
        assert_eq!(next.visible, "<think>literal</think>");
        assert!(next.reasoning.is_empty());
    }
}
