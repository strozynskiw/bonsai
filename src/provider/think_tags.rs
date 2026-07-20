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

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// Text routed out of one [`ThinkTagSplitter::push`]/[`finish`](ThinkTagSplitter::finish) call.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ThinkSplit {
    /// Text outside `<think>` blocks — the visible assistant answer.
    pub visible: String,
    /// Text inside `<think>` blocks — the model's reasoning.
    pub reasoning: String,
}

#[derive(Debug, Default)]
pub(crate) struct ThinkTagSplitter {
    in_think: bool,
    /// Bytes held back because they might be the leading part of a split tag.
    pending: String,
}

impl ThinkTagSplitter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed a text delta, returning the visible and reasoning text it yields.
    pub(crate) fn push(&mut self, chunk: &str) -> ThinkSplit {
        self.pending.push_str(chunk);
        let mut out = ThinkSplit::default();
        loop {
            let (tag, bucket) = if self.in_think {
                (CLOSE, &mut out.reasoning)
            } else {
                (OPEN, &mut out.visible)
            };
            if let Some(idx) = self.pending.find(tag) {
                bucket.push_str(&self.pending[..idx]);
                self.pending.drain(..idx + tag.len());
                self.in_think = !self.in_think;
                continue;
            }
            // No complete tag: emit everything except a trailing run that could
            // be the start of `tag` once the next chunk arrives.
            let keep = partial_suffix(&self.pending, tag);
            let cut = self.pending.len() - keep;
            bucket.push_str(&self.pending[..cut]);
            self.pending.drain(..cut);
            break;
        }
        out
    }

    /// Flush any buffered tail at end of stream so no text is dropped. An
    /// unterminated `<think>` is surfaced as reasoning (the block was opened but
    /// the stream ended before it closed).
    pub(crate) fn finish(&mut self) -> ThinkSplit {
        let rest = std::mem::take(&mut self.pending);
        let mut out = ThinkSplit::default();
        if self.in_think {
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
    fn text_before_and_after_think_block() {
        let out = run(&["hi <think>hmm</think> there"]);
        assert_eq!(out.visible, "hi  there");
        assert_eq!(out.reasoning, "hmm");
    }

    #[test]
    fn multiple_think_blocks() {
        let out = run(&["<think>one</think>A<think>two</think>B"]);
        assert_eq!(out.reasoning, "onetwo");
        assert_eq!(out.visible, "AB");
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
        assert_eq!(out.visible, "café  δ");
        assert_eq!(out.reasoning, "π");
    }
}
