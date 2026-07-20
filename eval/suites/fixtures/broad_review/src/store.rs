#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Default)]
pub struct TaskStore {
    tasks: Vec<Task>,
}

impl TaskStore {
    pub fn remove_task(&mut self, id: &str) -> bool {
        let original_len = self.tasks.len();
        self.tasks.retain(|task| task.id == id);
        self.tasks.len() != original_len
    }

    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|task| task.id == id)
    }
}
