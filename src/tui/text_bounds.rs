use unicode_segmentation::UnicodeSegmentation;

pub(crate) fn grapheme_count(text: &str) -> usize {
    text.graphemes(true).count()
}

pub(crate) fn byte_index_for_grapheme_index(text: &str, index: usize) -> usize {
    text.grapheme_indices(true)
        .nth(index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

pub(crate) fn grapheme_index_after_byte(text: &str, byte_index: usize) -> usize {
    let byte_index = byte_index.min(text.len());
    let mut count = 0usize;
    for (start, grapheme) in text.grapheme_indices(true) {
        if byte_index <= start {
            return count;
        }
        count += 1;
        if byte_index <= start + grapheme.len() {
            return count;
        }
    }
    count
}

pub(crate) fn line_start(text: &str, row: usize) -> usize {
    if row == 0 {
        return 0;
    }
    let mut count = 0usize;
    for line in text.split('\n').take(row) {
        count += grapheme_count(line) + 1;
    }
    count
}

pub(crate) fn line_length(text: &str, row: usize) -> usize {
    text.split('\n').nth(row).map(grapheme_count).unwrap_or(0)
}

pub(crate) fn row_col_at(text: &str, index: usize) -> (usize, usize) {
    let mut row = 0usize;
    let mut col = 0usize;
    for (i, grapheme) in text.graphemes(true).enumerate() {
        if i >= index {
            break;
        }
        if grapheme == "\n" {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (row, col)
}

pub(crate) fn previous_word_start(text: &str, index: usize) -> usize {
    let graphemes = graphemes(text);
    let mut i = index.min(graphemes.len());
    while i > 0 && is_word_separator_grapheme(graphemes[i - 1]) {
        i -= 1;
    }
    while i > 0 && !is_word_separator_grapheme(graphemes[i - 1]) {
        i -= 1;
    }
    i
}

pub(crate) fn next_word_start(text: &str, index: usize) -> usize {
    let graphemes = graphemes(text);
    let mut i = index.min(graphemes.len());
    while i < graphemes.len() && !is_word_separator_grapheme(graphemes[i]) {
        i += 1;
    }
    while i < graphemes.len() && is_word_separator_grapheme(graphemes[i]) {
        i += 1;
    }
    i
}

pub(crate) fn word_bounds_at(text: &str, index: usize) -> (usize, usize) {
    let graphemes = graphemes(text);
    if graphemes.is_empty() {
        return (0, 0);
    }
    let index = index.min(graphemes.len());
    if index >= graphemes.len() {
        return (graphemes.len(), graphemes.len());
    }
    let index_is_separator = is_word_separator_grapheme(graphemes[index]);
    let mut start = index;
    while start > 0 && is_word_separator_grapheme(graphemes[start - 1]) == index_is_separator {
        start -= 1;
    }
    let mut end = index;
    while end < graphemes.len() && is_word_separator_grapheme(graphemes[end]) == index_is_separator
    {
        end += 1;
    }
    (start, end)
}

pub(crate) fn line_bounds_at(text: &str, index: usize) -> (usize, usize) {
    let graphemes = graphemes(text);
    if graphemes.is_empty() {
        return (0, 0);
    }
    let index = index.min(graphemes.len());
    let mut start = index;
    while start > 0 && graphemes[start - 1] != "\n" {
        start -= 1;
    }
    let mut end = index;
    while end < graphemes.len() && graphemes[end] != "\n" {
        end += 1;
    }
    (start, end)
}

fn graphemes(text: &str) -> Vec<&str> {
    text.graphemes(true).collect()
}

fn is_word_separator_grapheme(grapheme: &str) -> bool {
    grapheme
        .chars()
        .next()
        .is_none_or(|ch| !(ch.is_alphanumeric() || ch == '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_bounds_select_word_or_separator_run() {
        assert_eq!(word_bounds_at("alpha beta", 7), (6, 10));
        assert_eq!(word_bounds_at("alpha  beta", 5), (5, 7));
    }

    #[test]
    fn line_bounds_select_logical_line_without_newline() {
        assert_eq!(line_bounds_at("one\ntwo\nthree", 5), (4, 7));
    }

    #[test]
    fn grapheme_indexes_keep_combining_marks_together() {
        let text = "e\u{301}clair";

        assert_eq!(grapheme_count(text), 6);
        assert_eq!(byte_index_for_grapheme_index(text, 1), "e\u{301}".len());
        assert_eq!(grapheme_index_after_byte(text, "e".len()), 1);
        assert_eq!(word_bounds_at(text, 0), (0, 6));
    }
}
