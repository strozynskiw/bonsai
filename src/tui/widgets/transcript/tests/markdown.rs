use super::*;

#[test]
fn preserves_fenced_code_blocks() {
    let lines = render_markdown("```rust\nfn main() {}\n```", 40);
    let rendered: Vec<String> = lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect();

    assert!(!rendered.iter().any(|line| line.contains("code: rust")));
    assert!(rendered.iter().any(|line| line.contains("rust")));
    assert!(rendered.iter().any(|line| line.contains("fn main() {}")));
}

#[test]
fn preview_full_markdown_sample() {
    let body = "Some intro.\n\n```code\n```md\n## Blockquote\n> This is a quote.\n```\n\n- one\n- two\n1. numbered\n\n---\n\n| Name | Age |\n| --- | --- |\n| Alice | 30 |\n";
    let lines = render_markdown(body, 60);
    eprintln!("==== preview ====");
    for line in lines {
        let s: String = line.spans.iter().map(|sp| sp.content.as_ref()).collect();
        eprintln!("|{s}|");
    }
    eprintln!("==== end ====");
}

#[test]
fn horizontal_rule_renders_as_separator_line() {
    let lines = render_markdown("above\n\n---\n\nbelow", 40);
    let rendered: Vec<String> = lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect();

    assert!(
        rendered.iter().any(|line| line.contains("─")),
        "horizontal rule should be drawn, got: {rendered:?}"
    );
}

#[test]
fn code_block_uses_full_top_and_bottom_borders() {
    let lines = render_markdown("```rust\nfn main() {}\n```", 40);
    let rendered: Vec<String> = lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect();

    assert!(
        rendered
            .first()
            .is_some_and(|line| line.starts_with("┌─ rust ")),
        "top border should include the lang label, got: {rendered:?}"
    );
    assert!(
        rendered.last().is_some_and(|line| line.starts_with("└")),
        "bottom border should close the box, got: {rendered:?}"
    );
}

#[test]
fn code_block_wraps_long_lines_inside_the_frame() {
    // The frame caps itself at min(width, 80); a code line longer than that
    // must wrap inside the box instead of poking out past the right border.
    let long = format!(
        "```rust\nlet run_failed = result.is_err() || matches!(&result, Ok(AgentRunOutcome::Failed)); // {}\n```",
        "x".repeat(60)
    );
    let width = 100usize;
    let lines = render_markdown(&long, width);
    let rendered: Vec<String> = lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect();

    let frame_width = width.min(80);
    let code_rows: Vec<&String> = rendered
        .iter()
        .filter(|line| line.starts_with("│ "))
        .collect();
    assert!(
        code_rows.len() >= 2,
        "an over-wide line should wrap onto multiple rows, got: {rendered:?}"
    );
    for row in &code_rows {
        assert!(
            row.chars().count() <= frame_width,
            "code row must stay inside the frame ({frame_width} cols): {row:?}"
        );
    }
    // Nothing was lost to truncation: the wrapped rows still carry the tail.
    assert!(
        code_rows.last().is_some_and(|row| row.contains("xxx")),
        "wrapped continuation should carry the line tail, got: {rendered:?}"
    );
}

#[test]
fn nested_code_block_with_md_fence_renders_inner_as_markdown() {
    let body = "```code\n```md\n## Blockquote\n> This is a quote.\n```\n```\n";
    let lines = render_markdown(body, 80);
    let rendered: Vec<String> = lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect();

    assert!(
        rendered.iter().any(|line| line.contains("Blockquote")),
        "heading should be parsed, got: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("│") && line.contains("This is a quote.")),
        "blockquote should use the rail, got: {rendered:?}"
    );
}

#[test]
fn code_block_uses_box_drawing_borders() {
    let lines = render_markdown("```rust\nfn main() {}\n```", 40);
    let rendered: Vec<String> = lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect();

    assert!(rendered.iter().any(|line| line.contains("┌─")));
    assert!(rendered.iter().any(|line| line.contains("└")));
}

#[test]
fn renders_tables_with_box_borders() {
    let lines = render_markdown(
        "| Name | Age | Role |\n| --- | --- | --- |\n| Alice | 28 | Developer |\n| Bob | 34 | Designer |",
        80,
    );
    let rendered: Vec<String> = lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect();

    assert!(rendered.iter().any(|line| line.contains("Name")));
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("Name") && line.contains("Age") && line.contains("│"))
    );
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("┌") && line.contains("┐"))
    );
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("└") && line.contains("┘"))
    );
    assert!(!rendered.iter().any(|line| line.contains("NameAgeRole")));
}

#[test]
fn renders_markdown_fenced_as_markdown() {
    let lines = render_markdown("```markdown\n# Title\n\n- item\n```", 40);
    let rendered: Vec<String> = lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect();

    assert!(!rendered.iter().any(|line| line.contains("code: markdown")));
    assert!(rendered.iter().any(|line| line.contains("Title")));
    assert!(rendered.iter().any(|line| line.contains("item")));
}

#[test]
fn renders_nested_markdown_fence_inside_plain_code_block() {
    let lines = render_markdown(
        "````\n```md\n| Name | Age |\n| --- | --- |\n| Alice | 30 |\n```\n````",
        80,
    );
    let rendered: Vec<String> = lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect();

    assert!(!rendered.iter().any(|line| line.contains("```md")));
    assert!(rendered.iter().any(|line| line.contains("Name")));
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("Alice") && line.contains("30") && line.contains("│"))
    );
}

#[test]
fn ordered_list_marker_appears_once_per_item() {
    let body = "1. **Title**\n\n   Continuation paragraph.\n\n   - nested bullet\n\n   > quoted line\n\n   ```rust\n   fn main() {}\n   ```\n\n2. Second item.\n";
    let rendered = rendered_text(render_markdown(body, 60));

    let numbered = rendered
        .iter()
        .filter(|line| line.contains("1. ") || line.contains("2. "))
        .count();
    assert_eq!(
        numbered, 2,
        "each ordered item should carry its number exactly once, got: {rendered:?}"
    );

    let title = rendered.iter().find(|l| l.contains("Title")).unwrap();
    assert!(
        title.contains("1. "),
        "first item marker on title: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .find(|l| l.contains("Continuation"))
            .is_some_and(|l| !l.contains("1. ")),
        "continuation paragraph must not repeat the marker: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .find(|l| l.contains("nested bullet"))
            .is_some_and(|l| l.contains("•") && !l.contains("1. ")),
        "nested bullet must use its own marker, not the parent number: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .find(|l| l.contains("quoted line"))
            .is_some_and(|l| l.contains("│") && !l.contains("1. ")),
        "quote inside an item must not repeat the marker: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .find(|l| l.contains("fn main() {}"))
            .is_some_and(|l| !l.contains("1. ")),
        "code inside an item must not repeat the marker: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .find(|l| l.contains("Second item"))
            .is_some_and(|l| l.contains("2. ")),
        "second item advances the counter: {rendered:?}"
    );
}

#[test]
fn nested_ordered_list_keeps_parent_counter() {
    let body = "1. first\n   - a\n   - b\n2. second\n";
    let rendered = rendered_text(render_markdown(body, 40));

    assert_eq!(
        rendered.iter().filter(|l| l.contains("1. ")).count(),
        1,
        "parent '1.' marker exactly once: {rendered:?}"
    );
    assert_eq!(
        rendered.iter().filter(|l| l.contains("2. ")).count(),
        1,
        "parent '2.' marker exactly once: {rendered:?}"
    );
    assert_eq!(
        rendered
            .iter()
            .filter(|l| l.contains("•") && (l.contains("a") || l.contains("b")))
            .count(),
        2,
        "nested bullets keep their own markers: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .find(|l| l.contains("a"))
            .is_some_and(|l| !l.contains("1.")),
        "nested content must not carry the parent number: {rendered:?}"
    );
}

#[test]
fn ordered_list_honors_explicit_start() {
    let body = "3. three\n4. four\n";
    let rendered = rendered_text(render_markdown(body, 40));

    assert!(
        rendered
            .iter()
            .find(|l| l.contains("three"))
            .is_some_and(|l| l.contains("3. ")),
        "explicit start value honored: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .find(|l| l.contains("four"))
            .is_some_and(|l| l.contains("4. ")),
        "counter advances from start: {rendered:?}"
    );
}

#[test]
fn parses_tool_result_truncation_marker() {
    let preview = tool_result_preview("[Output truncated: 123 chars total]\nhello")
        .expect("truncation should be detected");

    assert!(preview.contains("[truncated] 123 chars total"));
    assert!(preview.contains("hello"));
}
