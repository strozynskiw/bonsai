//! Read-only plan selection and matching.

use super::{Finding, PlanDoc};

/// Read-only selection service over a plan document.
#[derive(Debug, Clone, Copy)]
pub struct PlanQuery<'a> {
    doc: &'a PlanDoc,
}

impl PlanDoc {
    /// Creates a read-only query service for this document.
    pub fn query(&self) -> PlanQuery<'_> {
        PlanQuery { doc: self }
    }

    /// Returns findings in stable severity order.
    pub fn findings_in_severity_order(&self) -> Vec<&Finding> {
        self.query().findings_in_severity_order()
    }
}

impl<'a> PlanQuery<'a> {
    /// Returns findings in stable severity order.
    pub fn findings_in_severity_order(self) -> Vec<&'a Finding> {
        let mut ordered: Vec<_> = self.doc.findings.iter().collect();
        ordered.sort_by_key(|finding| finding.severity);
        ordered
    }

    pub(super) fn finding_index(self, target: &str) -> anyhow::Result<usize> {
        let trimmed = target.trim();
        if trimmed.is_empty() {
            anyhow::bail!("finding match text must not be empty");
        }

        let needle = trimmed.to_lowercase();
        if let Some(index) = self
            .doc
            .findings
            .iter()
            .position(|finding| finding.issue.to_lowercase().contains(&needle))
        {
            return Ok(index);
        }

        let needle_tokens = normalized_tokens(trimmed);
        if needle_tokens.len() < 2 {
            anyhow::bail!("No finding matching {target:?} found.");
        }

        let mut matches = self
            .doc
            .findings
            .iter()
            .enumerate()
            .filter(|(_, finding)| issue_contains_ordered_tokens(&finding.issue, &needle_tokens))
            .map(|(index, _)| index);
        let Some(index) = matches.next() else {
            anyhow::bail!("No finding matching {target:?} found.");
        };
        if matches.next().is_some() {
            anyhow::bail!("Multiple findings match {target:?}; use more specific text.");
        }
        Ok(index)
    }

    pub(super) fn section_index(self, heading: &str) -> Option<usize> {
        let target = heading.trim().to_lowercase();
        self.doc
            .sections
            .iter()
            .position(|section| section.heading.to_lowercase() == target)
    }

    pub(super) fn phase_index(self, name: &str) -> Option<usize> {
        let target = name.trim().to_lowercase();
        self.doc
            .phases
            .iter()
            .position(|phase| phase.name.to_lowercase() == target)
    }

    pub(super) fn flat_task_index(self, target: &str) -> Option<usize> {
        let target = target.to_lowercase();
        self.doc
            .tasks
            .iter()
            .position(|task| task.text.to_lowercase().contains(&target))
    }

    pub(super) fn task_location(self, target: &str) -> Option<(Option<usize>, usize)> {
        let needle = target.to_lowercase();
        if let Some(index) = self.flat_task_index(&needle) {
            return Some((None, index));
        }
        for (phase_index, phase) in self.doc.phases.iter().enumerate() {
            if let Some(index) = phase
                .tasks
                .iter()
                .position(|task| task.text.to_lowercase().contains(&needle))
            {
                return Some((Some(phase_index), index));
            }
        }
        None
    }
}

fn normalized_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn issue_contains_ordered_tokens(issue: &str, needle_tokens: &[String]) -> bool {
    if needle_tokens.is_empty() {
        return false;
    }
    let mut needle_index = 0;
    for token in normalized_tokens(issue) {
        if token == needle_tokens[needle_index] {
            needle_index += 1;
            if needle_index == needle_tokens.len() {
                return true;
            }
        }
    }
    false
}
