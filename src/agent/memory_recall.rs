//! Per-turn memory recall: after a human turn is accepted, ask the memory
//! service for entries relevant to the fresh user input and inject them as a
//! user-context note — background data the model can use, never instructions.
//! No-op without a memory service or in smol mode.

use super::*;
use serde_json::Value;

/// Rough token budget for one turn's recall note. Entries are dropped whole
/// from the tail when they would overflow — never truncated mid-entry.
const MEMORY_RECALL_TOKEN_BUDGET: usize = 500;

impl Agent {
    pub(super) async fn inject_recalled_memory(&mut self, user_input: &str) {
        let Some(memory) = self.memory.clone() else {
            return;
        };
        // Smol sessions carry no memory surface at all (no index, no
        // memory_write tool), so recall stays consistent with that.
        if self.smol_mode {
            return;
        }

        let entries = memory.recall(user_input).await;
        if entries.is_empty() {
            return;
        }

        let mut names: Vec<String> = Vec::new();
        let mut rendered_entries: Vec<String> = Vec::new();
        let mut spent_tokens = 0usize;
        for entry in entries {
            let rendered = crate::memory::render_recall_entry(&entry);
            let cost = self.prompt_estimator.estimate_text_for_report(&rendered);
            // Always keep the top entry, even when it alone exceeds the
            // budget: recall already judged it the most relevant.
            if !rendered_entries.is_empty() && spent_tokens + cost > MEMORY_RECALL_TOKEN_BUDGET {
                break;
            }
            spent_tokens += cost;
            names.push(entry.name.clone());
            rendered_entries.push(rendered);
        }

        memory.mark_recalled(names.iter().cloned());
        let mut note = crate::memory::render_recall_note(&rendered_entries);
        // Same hygiene as other injected context: never carry a secret that
        // leaked into a memory file back into the conversation.
        crate::redact::redact_in_place(&mut note);
        self.push_user_message_raw(&note);
    }

    pub(super) fn rebuild_recalled_memory_from_messages(&self) {
        let Some(memory) = &self.memory else {
            return;
        };
        let mut names = HashSet::new();
        for message in &self.messages {
            let Ok(value) = serde_json::to_value(message) else {
                continue;
            };
            if let Some(content) = value.get("content").and_then(Value::as_str) {
                names.extend(crate::memory::recalled_names_in_text(content));
            }
            names.extend(memory_write_names_in_message(&value, memory));
        }
        memory.replace_recalled(names);
    }
}

fn memory_write_names_in_message(
    message: &Value,
    memory: &crate::memory::MemoryService,
) -> Vec<String> {
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| {
            let function = call.get("function")?;
            let name = function.get("name").and_then(Value::as_str)?;
            if name != "memory_write" {
                return None;
            }
            let arguments = function.get("arguments").and_then(Value::as_str)?;
            let args = serde_json::from_str::<Value>(arguments).ok()?;
            memory_write_name_from_args(&args, memory)
        })
        .collect()
}

fn memory_write_name_from_args(
    args: &Value,
    memory: &crate::memory::MemoryService,
) -> Option<String> {
    if args.get("action").and_then(Value::as_str) != Some("save") {
        return None;
    }
    if let Some(name) = args
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| crate::memory::entry::is_valid_name(name))
    {
        return Some(name.to_string());
    }

    let description = args
        .get("description")
        .and_then(Value::as_str)
        .map(crate::memory::entry::normalize_description)
        .filter(|description| !description.is_empty())?;
    let tier = args
        .get("tier")
        .and_then(Value::as_str)
        .and_then(crate::memory::entry::MemoryTier::from_label);

    memory
        .store()
        .entries()
        .into_iter()
        .find(|entry| {
            entry.description == description
                && match tier {
                    Some(tier) => entry.tier == tier,
                    None => true,
                }
        })
        .map(|entry| entry.name)
}
