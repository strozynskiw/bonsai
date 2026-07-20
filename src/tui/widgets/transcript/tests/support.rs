use super::*;

pub(super) fn execution_group(
    id: u64,
    _started_at: std::time::Instant,
    tools: Vec<ToolActivity>,
    finished_at: Option<std::time::Instant>,
) -> ExecutionGroup {
    ExecutionGroup {
        id,
        finished_at,
        tools,
    }
}

pub(super) fn rendered_text(lines: Vec<Line<'static>>) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect()
}

pub(super) fn transcript_lines_for_activity(
    activity: &ToolActivity,
    width: usize,
) -> Vec<Line<'static>> {
    let mut app = AppState::new("opencode", "test-model".to_string(), ".".to_string(), None);
    app.transcript
        .push(TranscriptItem::ToolActivity(activity.clone()));
    transcript_lines(&app, width)
}
