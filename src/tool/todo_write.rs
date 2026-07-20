use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

use crate::todo::{SharedTodoStore, TodoItem, TodoStatus};
use crate::tool::schema::{
    array_property, object, parse_args, string_enum_property, string_property,
};
use crate::tool::{Tool, ToolOutput};

#[derive(Deserialize)]
struct TodoWriteArgs {
    todos: Vec<TodoItemInput>,
}

#[derive(Deserialize)]
struct TodoItemInput {
    content: String,
    status: TodoStatus,
}

pub struct TodoWriteTool {
    todo_store: SharedTodoStore,
}

impl TodoWriteTool {
    pub fn new(todo_store: SharedTodoStore) -> Self {
        Self { todo_store }
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::LocalState
    }

    fn name(&self) -> &str {
        "todowrite"
    }

    fn description(&self) -> &str {
        "Create or update the agent's task list. The todos array replaces the entire list — carry unfinished and completed items across calls, changing only their statuses. Use this for anything larger than a single trivial answer or one tiny edit, and keep progress inline."
    }

    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        crate::tool::ParallelPolicy::Serialized
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object(
            [(
                "todos",
                array_property(
                    "The full todo list to persist, replacing any previous list",
                    object(
                        [
                            ("content", string_property("Todo content")),
                            (
                                "status",
                                string_enum_property(
                                    "pending | in_progress | completed | cancelled",
                                    &["pending", "in_progress", "completed", "cancelled"],
                                ),
                            ),
                        ],
                        &["content", "status"],
                    ),
                ),
            )],
            &["todos"],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: TodoWriteArgs = parse_args("todowrite", args)?;
        let mut items = Vec::with_capacity(args.todos.len());
        let mut in_progress_count = 0;
        for item in &args.todos {
            let content = item.content.trim().to_string();
            if content.is_empty() {
                anyhow::bail!("todo content must not be empty");
            }
            if matches!(item.status, TodoStatus::InProgress) {
                in_progress_count += 1;
            }
            items.push(TodoItem {
                content,
                status: item.status,
            });
        }

        if in_progress_count > 1 {
            anyhow::bail!("only one todo may be in_progress at a time");
        }

        let mut store = self.todo_store.lock().await;
        // An identical resend is pure bookkeeping churn (observed live: 44
        // todowrite calls for 12 tasks, many byte-identical). Succeed without
        // touching the store and teach the cheaper cadence.
        if store.todos() == items.as_slice() {
            return Ok(ToolOutput::Text(
                "Todo list unchanged — this is identical to the current list, so nothing was \
                 recorded. Call todowrite only when an item's status or content changes, and \
                 batch it into the same turn as the work itself."
                    .to_string(),
            ));
        }
        // The list is whole-replace, and models routinely resend only the
        // current step, silently discarding everything not yet finished
        // (observed live in multi-phase runs, where only the active phase's
        // items ever survived). Apply the write — the model may genuinely be
        // replanning — but name what fell out so an accidental drop is
        // corrected on the next call instead of vanishing.
        let dropped_unfinished: Vec<String> = store
            .todos()
            .iter()
            .filter(|existing| {
                matches!(
                    existing.status,
                    TodoStatus::Pending | TodoStatus::InProgress
                ) && !items.iter().any(|item| item.content == existing.content)
            })
            .map(|existing| existing.content.clone())
            .collect();
        store.set_todos(items);

        if store.todos().is_empty() {
            return Ok(ToolOutput::Text("Todo list cleared.".to_string()));
        }

        let mut result = String::from("Todo list updated:\n");
        for todo in store.todos() {
            result.push_str(&format!("- [{:?}] {}\n", todo.status, todo.content));
        }
        if !dropped_unfinished.is_empty() {
            result.push_str(&format!(
                "\nNote: this replaced the whole list and dropped {} unfinished item(s): {}. \
                 todowrite replaces the entire list — carry unfinished and completed items \
                 across calls, updating only their statuses. If the drop was intentional \
                 (replanning), ignore this; otherwise resend the full list.",
                dropped_unfinished.len(),
                dropped_unfinished.join("; ")
            ));
        }

        Ok(ToolOutput::Text(result.trim_end().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn updates_todo_store() {
        let store = std::sync::Arc::new(Mutex::new(crate::todo::TodoStore::new()));
        let tool = TodoWriteTool::new(store.clone());
        let result = tool
            .execute(serde_json::json!({
                "todos": [{"content": "Ship feature", "status": "in_progress"}]
            }))
            .await
            .unwrap();
        assert!(matches!(result, ToolOutput::Text(_)));
        assert_eq!(store.lock().await.todos().len(), 1);
    }

    #[tokio::test]
    async fn identical_resend_succeeds_with_unchanged_hint() {
        let store = std::sync::Arc::new(Mutex::new(crate::todo::TodoStore::new()));
        let tool = TodoWriteTool::new(store.clone());
        let list = serde_json::json!({
            "todos": [
                {"content": "Ship feature", "status": "in_progress"},
                {"content": "Write tests", "status": "pending"}
            ]
        });
        tool.execute(list.clone()).await.unwrap();

        let result = tool.execute(list).await.unwrap();
        let ToolOutput::Text(text) = result else {
            panic!("expected text output");
        };
        assert!(text.contains("unchanged"), "{text}");
        assert_eq!(store.lock().await.todos().len(), 2);

        // A real status change still lands normally.
        let updated = tool
            .execute(serde_json::json!({
                "todos": [
                    {"content": "Ship feature", "status": "completed"},
                    {"content": "Write tests", "status": "in_progress"}
                ]
            }))
            .await
            .unwrap();
        let ToolOutput::Text(text) = updated else {
            panic!("expected text output");
        };
        assert!(text.contains("Todo list updated"), "{text}");
    }

    #[tokio::test]
    async fn replace_dropping_unfinished_items_applies_but_names_the_drop() {
        // Whole-list replaces that resend only the current step silently lose
        // every other unfinished item; the write must land (replanning is
        // legitimate) but the result must name what fell out so an accidental
        // drop gets corrected.
        let store = std::sync::Arc::new(Mutex::new(crate::todo::TodoStore::new()));
        let tool = TodoWriteTool::new(store.clone());
        tool.execute(serde_json::json!({
            "todos": [
                {"content": "Phase 1: caesar", "status": "completed"},
                {"content": "Phase 2: rpn", "status": "in_progress"},
                {"content": "Phase 3: csv", "status": "pending"}
            ]
        }))
        .await
        .unwrap();

        let result = tool
            .execute(serde_json::json!({
                "todos": [
                    {"content": "Phase 3: csv", "status": "in_progress"}
                ]
            }))
            .await
            .unwrap();
        let ToolOutput::Text(text) = result else {
            panic!("expected text output");
        };
        // The write applied…
        assert_eq!(store.lock().await.todos().len(), 1);
        // …and the dropped unfinished item is named; the completed one is not.
        assert!(text.contains("dropped 1 unfinished"), "{text}");
        assert!(text.contains("Phase 2: rpn"), "{text}");
        assert!(!text.contains("Phase 1: caesar"), "{text}");

        // A replace that carries items forward (status-only changes) stays quiet.
        let result = tool
            .execute(serde_json::json!({
                "todos": [
                    {"content": "Phase 3: csv", "status": "completed"}
                ]
            }))
            .await
            .unwrap();
        let ToolOutput::Text(text) = result else {
            panic!("expected text output");
        };
        assert!(!text.contains("dropped"), "{text}");
    }

    #[tokio::test]
    async fn ignores_legacy_priority_field() {
        // Older models may still send "priority"; it must be tolerated, not required.
        let store = std::sync::Arc::new(Mutex::new(crate::todo::TodoStore::new()));
        let tool = TodoWriteTool::new(store.clone());
        let result = tool
            .execute(serde_json::json!({
                "todos": [{"content": "Ship feature", "status": "pending", "priority": "high"}]
            }))
            .await;
        assert!(result.is_ok());
        assert_eq!(store.lock().await.todos().len(), 1);
    }

    #[tokio::test]
    async fn rejects_multiple_in_progress() {
        let store = std::sync::Arc::new(Mutex::new(crate::todo::TodoStore::new()));
        let tool = TodoWriteTool::new(store);
        let result = tool
            .execute(serde_json::json!({
                "todos": [
                    {"content": "A", "status": "in_progress"},
                    {"content": "B", "status": "in_progress"}
                ]
            }))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn clears_todos_on_empty_list() {
        let store = std::sync::Arc::new(Mutex::new(crate::todo::TodoStore::new()));
        let tool = TodoWriteTool::new(store.clone());
        // Seed with a todo first.
        tool.execute(serde_json::json!({"todos": [{"content": "x", "status": "pending"}]}))
            .await
            .unwrap();
        assert!(!store.lock().await.todos().is_empty());
        // Empty list clears.
        let result = tool.execute(serde_json::json!({"todos": []})).await;
        assert!(result.is_ok());
        assert!(store.lock().await.todos().is_empty());
    }

    #[tokio::test]
    async fn rejects_blank_content() {
        let store = std::sync::Arc::new(Mutex::new(crate::todo::TodoStore::new()));
        let tool = TodoWriteTool::new(store);
        let result = tool
            .execute(serde_json::json!({
                "todos": [{"content": "   ", "status": "pending"}]
            }))
            .await;
        assert!(result.is_err());
    }
}
