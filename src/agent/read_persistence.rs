use super::*;
use crate::tool::ProjectPathResolver;
use crate::tool::read_evidence::{
    DelegatedReadEvidence, ReadCoverage, ReadEvidenceRecord, ReadProvenance,
};

impl Agent {
    pub(crate) fn delegated_read_evidence_manifest(
        &self,
        subtask_id: &str,
        result: &str,
    ) -> Vec<DelegatedReadEvidence> {
        const MAX_RECORDS: usize = 64;

        let mut seen = HashSet::new();
        self.read_evidence_snapshot()
            .into_iter()
            .filter(|record| {
                record.target_live
                    && !record.target_stubbed
                    && record.evidence.observation_is_current()
            })
            .filter(|record| {
                let observation = record.evidence.observation();
                seen.insert((
                    observation.canonical_path().to_path_buf(),
                    observation.window().clone(),
                    observation.coverage(),
                ))
            })
            .take(MAX_RECORDS)
            .map(|record| {
                let observation = record.evidence.observation();
                let project_relative = observation
                    .canonical_path()
                    .strip_prefix(&self.project_root)
                    .ok()
                    .and_then(std::path::Path::to_str);
                let cited_in_result = result.contains(observation.display_path())
                    || project_relative.is_some_and(|path| result.contains(path));
                DelegatedReadEvidence {
                    subtask_id: subtask_id.to_string(),
                    launch_group_id: None,
                    source_id: record.source_id,
                    cited_in_result,
                    evidence: record.evidence,
                }
            })
            .collect()
    }

    pub(crate) fn import_delegated_read_evidence(&mut self, evidence: &[DelegatedReadEvidence]) {
        let mut known = self
            .delegated_read_evidence
            .iter()
            .map(|entry| (entry.subtask_id.clone(), entry.source_id.clone()))
            .collect::<HashSet<_>>();
        self.delegated_read_evidence.extend(
            evidence
                .iter()
                .filter(|entry| known.insert((entry.subtask_id.clone(), entry.source_id.clone())))
                .cloned(),
        );
    }

    pub(crate) fn read_evidence_snapshot(&self) -> Vec<ReadEvidenceRecord> {
        let mut records = Vec::new();
        for (index, message) in self.messages.iter().enumerate() {
            let Some(message_id) = self.message_ids.get(index) else {
                continue;
            };
            let target_stubbed = self.message_has_control(index, message, |state| state.stubbed);
            let target_live = !self.message_has_control(index, message, |state| {
                state.stubbed || state.drop_next_turn
            });
            let target_content_digest =
                crate::tool::digest_content(message_content_string(message).as_bytes());

            if let Some(call_id) = tool_message_call_id(message)
                && let Some(detail) = self.tool_context_details.get(&call_id)
                && let Some(evidence) = detail.read_evidence.as_ref()
            {
                let returned_chars = evidence.observation().visible_chars();
                let admission = self.inspection_events.get(&call_id);
                records.push(ReadEvidenceRecord {
                    source_id: format!("tool:{call_id}"),
                    provenance: ReadProvenance::ParentVisible,
                    target_message_id: message_id.clone(),
                    target_content_digest,
                    target_tool_call_id: Some(call_id),
                    tool_name: Some(detail.name.clone()),
                    tool_arguments: Some(detail.arguments.clone()),
                    target_live,
                    target_stubbed,
                    evidence: evidence.clone(),
                    admission_outcome: admission
                        .map(|event| event.outcome.label())
                        .unwrap_or("executed")
                        .to_string(),
                    admission_reason: admission
                        .map(|event| event.reason.label())
                        .unwrap_or("no_fresh_visible_coverage")
                        .to_string(),
                    requested_chars: admission
                        .map(|event| event.requested_chars)
                        .unwrap_or(returned_chars),
                    returned_chars: admission
                        .map(|event| event.returned_chars)
                        .unwrap_or(returned_chars),
                    avoided_chars: admission.map(|event| event.avoided_chars).unwrap_or(0),
                });
            }

            if let Some(evidence) = self.mention_read_evidence.get(message_id) {
                for (mention_index, evidence) in evidence.iter().enumerate() {
                    let returned_chars = evidence.observation().visible_chars();
                    records.push(ReadEvidenceRecord {
                        source_id: format!("mention:{message_id}:{mention_index}"),
                        provenance: ReadProvenance::MentionVisible,
                        target_message_id: message_id.clone(),
                        target_content_digest,
                        target_tool_call_id: None,
                        tool_name: None,
                        tool_arguments: None,
                        target_live,
                        target_stubbed,
                        evidence: evidence.clone(),
                        admission_outcome: "executed".to_string(),
                        admission_reason: "mention_expansion".to_string(),
                        requested_chars: returned_chars,
                        returned_chars,
                        avoided_chars: 0,
                    });
                }
            }
        }
        records
    }

    pub(crate) fn inspection_event_snapshot(&self) -> Vec<InspectionEventRecord> {
        let mut records = Vec::new();
        for (index, message) in self.messages.iter().enumerate() {
            let Some(call_id) = tool_message_call_id(message) else {
                continue;
            };
            let Some(message_id) = self.message_ids.get(index) else {
                continue;
            };
            let Some(admission) = self.inspection_events.get(&call_id) else {
                continue;
            };
            let Some(detail) = self.tool_context_details.get(&call_id) else {
                continue;
            };
            let target_stubbed = self.message_has_control(index, message, |state| state.stubbed);
            let target_live = !self.message_has_control(index, message, |state| {
                state.stubbed || state.drop_next_turn
            });
            records.push(InspectionEventRecord {
                call_id,
                target_message_id: message_id.clone(),
                target_content_digest: crate::tool::digest_content(
                    message_content_string(message).as_bytes(),
                ),
                tool_name: detail.name.clone(),
                tool_arguments: detail.arguments.clone(),
                target_live,
                target_stubbed,
                admission: admission.clone(),
            });
        }
        records
    }

    pub(crate) async fn restore_read_evidence(&mut self, records: Vec<ReadEvidenceRecord>) {
        let mut candidates = records
            .into_iter()
            .filter(|record| self.persisted_evidence_target_index(record).is_some())
            .filter(|record| self.persisted_evidence_path_is_valid(record))
            .collect::<Vec<_>>();

        futures::future::join_all(
            candidates
                .iter_mut()
                .map(|record| record.evidence.refresh_freshness()),
        )
        .await;

        for record in candidates {
            let Some(target_index) = self.persisted_evidence_target_index(&record) else {
                continue;
            };
            let target_message = &self.messages[target_index];
            let target_is_live = !self.message_has_control(target_index, target_message, |state| {
                state.stubbed || state.drop_next_turn
            });
            let validated_parent_full_read = record.provenance == ReadProvenance::ParentVisible
                && record.target_live
                && !record.target_stubbed
                && target_is_live
                && record.evidence.observation_is_current()
                && record.evidence.observation().coverage() == ReadCoverage::Full;

            if validated_parent_full_read {
                let observation = record.evidence.observation();
                if let Some(file_digest) = observation.file_digest_at_read() {
                    self.read_tracker
                        .mark_read_with_file_digest(observation.canonical_path(), file_digest, true)
                        .await;
                } else {
                    self.read_tracker
                        .mark_read(observation.canonical_path())
                        .await;
                }
            }

            match record.provenance {
                ReadProvenance::ParentVisible => {
                    let Some(call_id) = record.target_tool_call_id else {
                        continue;
                    };
                    let rendered = message_content_string(target_message);
                    self.tool_context_details.insert(
                        call_id.clone(),
                        ToolContextDetail {
                            call_id,
                            name: record.tool_name.unwrap_or_else(|| "read".to_string()),
                            arguments: record.tool_arguments.unwrap_or_default(),
                            read_evidence: Some(record.evidence),
                            result: ToolContextResult::Text { rendered },
                            reuse_target_call_id: None,
                        },
                    );
                }
                ReadProvenance::MentionVisible => {
                    self.mention_read_evidence
                        .entry(record.target_message_id)
                        .or_default()
                        .push(record.evidence);
                }
                // Delegated evidence is session-volatile and never restored as
                // a context row or promoted into parent write authorization.
                ReadProvenance::DelegatedObserved => {}
            }
        }
    }

    pub(crate) fn restore_inspection_events(&mut self, records: Vec<InspectionEventRecord>) {
        for record in records {
            let Some(target_index) = self.persisted_inspection_target_index(&record) else {
                continue;
            };
            if matches!(
                record.admission.outcome,
                InspectionOutcome::Reused | InspectionOutcome::Rejected
            ) {
                let Some(reuse_target) = record.admission.reuse_target_tool_call_id.as_ref() else {
                    continue;
                };
                if !self.messages.iter().any(|message| {
                    tool_message_call_id(message).as_deref() == Some(reuse_target.as_str())
                }) {
                    continue;
                }
                let rendered = message_content_string(&self.messages[target_index]);
                self.tool_context_details.insert(
                    record.call_id.clone(),
                    ToolContextDetail {
                        call_id: record.call_id.clone(),
                        name: record.tool_name.clone(),
                        arguments: record.tool_arguments.clone(),
                        read_evidence: None,
                        result: ToolContextResult::Text { rendered },
                        reuse_target_call_id: Some(reuse_target.clone()),
                    },
                );
            }
            self.inspection_events
                .insert(record.call_id, record.admission);
        }
    }

    fn persisted_evidence_target_index(&self, record: &ReadEvidenceRecord) -> Option<usize> {
        let index = self
            .message_ids
            .iter()
            .position(|message_id| message_id == &record.target_message_id)?;
        if crate::tool::digest_content(message_content_string(&self.messages[index]).as_bytes())
            != record.target_content_digest
        {
            return None;
        }
        match record.provenance {
            ReadProvenance::ParentVisible => {
                let expected_call_id = record.target_tool_call_id.as_deref()?;
                (tool_message_call_id(&self.messages[index]).as_deref() == Some(expected_call_id))
                    .then_some(index)
            }
            ReadProvenance::MentionVisible => record.target_tool_call_id.is_none().then_some(index),
            ReadProvenance::DelegatedObserved => None,
        }
    }

    fn persisted_evidence_path_is_valid(&self, record: &ReadEvidenceRecord) -> bool {
        let stored = record.evidence.observation().canonical_path();
        let raw_path = stored.to_string_lossy();
        ProjectPathResolver::new(&self.project_root)
            .resolve_existing(&raw_path)
            .is_ok_and(|resolved| resolved.canonical_path() == stored)
            && record.evidence.freshness_baseline().canonical_path() == stored
    }

    fn persisted_inspection_target_index(&self, record: &InspectionEventRecord) -> Option<usize> {
        let index = self
            .message_ids
            .iter()
            .position(|message_id| message_id == &record.target_message_id)?;
        let message = &self.messages[index];
        if tool_message_call_id(message).as_deref() != Some(record.call_id.as_str())
            || crate::tool::digest_content(message_content_string(message).as_bytes())
                != record.target_content_digest
        {
            return None;
        }
        Some(index)
    }
}
