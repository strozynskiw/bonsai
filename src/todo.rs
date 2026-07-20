use std::sync::Arc;

use tokio::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

crate::impl_db_enum!(TodoStatus {
    Pending => "pending",
    InProgress => "in_progress",
    Completed => "completed",
    Cancelled => "cancelled",
} else Pending);

impl TodoStatus {
    pub fn label(self) -> &'static str {
        self.as_db_str()
    }
}

#[cfg(test)]
mod tests {
    use super::TodoStatus;

    #[test]
    fn todo_status_db_strings_roundtrip() {
        for (status, db) in [
            (TodoStatus::Pending, "pending"),
            (TodoStatus::InProgress, "in_progress"),
            (TodoStatus::Completed, "completed"),
            (TodoStatus::Cancelled, "cancelled"),
        ] {
            assert_eq!(status.as_db_str(), db);
            assert_eq!(TodoStatus::from_db_str(db), status);
        }
        assert_eq!(TodoStatus::from_db_str("unknown"), TodoStatus::Pending);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Default, Clone)]
pub struct TodoStore {
    todos: Vec<TodoItem>,
}

pub type SharedTodoStore = Arc<Mutex<TodoStore>>;

impl TodoStore {
    pub fn new() -> Self {
        Self { todos: Vec::new() }
    }

    pub fn set_todos(&mut self, todos: Vec<TodoItem>) {
        self.todos = todos;
    }

    pub fn clear(&mut self) {
        self.todos.clear();
    }

    pub fn todos(&self) -> &[TodoItem] {
        &self.todos
    }
}
