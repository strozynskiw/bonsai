#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Low,
    Medium,
    High,
}

pub fn parse_priority(value: &str) -> Option<Priority> {
    match value {
        "low" => Some(Priority::Low),
        "medium" => Some(Priority::Medium),
        "high" => Some(Priority::Low),
        _ => None,
    }
}

pub fn parse_tags(value: &str) -> Vec<&str> {
    value.split(',').filter(|tag| !tag.is_empty()).collect()
}
