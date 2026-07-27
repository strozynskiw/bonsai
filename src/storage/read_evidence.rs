use super::*;
use crate::tool::read_evidence::{
    InspectionEventRecord, InspectionOutcome, InspectionReason, ReadAdmissionMetadata,
    ReadCoverage, ReadEvidence, ReadEvidenceRecord, ReadFreshness, ReadProvenance, ReadWindow,
};

impl Storage {
    pub(crate) async fn replace_read_evidence_snapshot_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        records: &[ReadEvidenceRecord],
        now: i64,
    ) -> Result<()> {
        storage_op!(
            tx,
            "delete read evidence",
            sqlx::query("DELETE FROM read_evidence WHERE session_id = ?").bind(session_id.as_i64()),
        )?;

        for (seq, record) in records.iter().enumerate() {
            let observation = record.evidence.observation();
            let baseline = record.evidence.freshness_baseline();
            storage_op!(
                tx,
                "insert read evidence",
                sqlx::query(
                    r#"
                INSERT INTO read_evidence (
                  session_id, seq, lane_kind, lane_id, source_id, provenance,
                  target_message_id, target_content_digest, target_tool_call_id,
                  tool_name, tool_arguments,
                  target_live, target_stubbed, canonical_path, display_path,
                  requested_offset, requested_limit, displayed_start_line,
                  displayed_end_line, total_lines, coverage, visible_digest,
                  visible_chars, file_digest_at_read, baseline_len,
                  baseline_modified_ms, baseline_file_digest, baseline_status,
                  observation_is_current, admission_outcome, admission_reason,
                  requested_chars, returned_chars, avoided_chars
                )
                VALUES (?, ?, 'parent', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
                )
                .bind(session_id.as_i64())
                .bind(seq as i64)
                .bind(session_id.to_string())
                .bind(&record.source_id)
                .bind(record.provenance.label())
                .bind(&record.target_message_id)
                .bind(record.target_content_digest.to_hex().to_string())
                .bind(record.target_tool_call_id.as_deref())
                .bind(record.tool_name.as_deref())
                .bind(record.tool_arguments.as_deref())
                .bind(i64::from(record.target_live))
                .bind(i64::from(record.target_stubbed))
                .bind(observation.canonical_path().to_string_lossy().as_ref())
                .bind(observation.display_path())
                .bind(usize_to_i64(observation.window().requested_offset))
                .bind(usize_to_i64(observation.window().requested_limit))
                .bind(usize_to_i64(observation.window().start_line))
                .bind(observation.window().end_line.map(usize_to_i64))
                .bind(observation.window().total_lines.map(usize_to_i64))
                .bind(observation.coverage().label())
                .bind(observation.visible_digest().to_hex().as_str())
                .bind(usize_to_i64(observation.visible_chars()))
                .bind(
                    observation
                        .file_digest_at_read()
                        .map(|digest| digest.to_hex().to_string()),
                )
                .bind(u64_to_i64(baseline.len()))
                .bind(baseline.modified().and_then(system_time_to_ms))
                .bind(
                    baseline
                        .current_file_digest()
                        .map(|digest| digest.to_hex().to_string()),
                )
                .bind(baseline.status().label())
                .bind(i64::from(baseline.observation_is_current()))
                .bind(&record.admission_outcome)
                .bind(&record.admission_reason)
                .bind(usize_to_i64(record.requested_chars))
                .bind(usize_to_i64(record.returned_chars))
                .bind(usize_to_i64(record.avoided_chars)),
            )?;
        }

        touch_session(tx, session_id, now).await
    }

    pub(super) async fn load_read_evidence(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<ReadEvidenceRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT source_id, provenance, target_message_id, target_content_digest,
                   target_tool_call_id,
                   tool_name, tool_arguments, target_live, target_stubbed,
                   canonical_path, display_path, requested_offset, requested_limit,
                   displayed_start_line, displayed_end_line, total_lines, coverage,
                   visible_digest, visible_chars, file_digest_at_read, baseline_len,
                   baseline_modified_ms, baseline_file_digest, baseline_status,
                   observation_is_current, admission_outcome, admission_reason,
                   requested_chars, returned_chars, avoided_chars
            FROM read_evidence
            WHERE session_id = ?
            ORDER BY seq
            "#,
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load read evidence for session {session_id}"))?;

        rows.into_iter()
            .map(read_evidence_record_from_row)
            .collect()
    }

    pub(crate) async fn replace_inspection_events_snapshot_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        records: &[InspectionEventRecord],
        now: i64,
    ) -> Result<()> {
        storage_op!(
            tx,
            "delete inspection events",
            sqlx::query("DELETE FROM inspection_events WHERE session_id = ?")
                .bind(session_id.as_i64()),
        )?;

        for (seq, record) in records.iter().enumerate() {
            storage_op!(
                tx,
                "insert inspection event",
                sqlx::query(
                    r#"
                INSERT INTO inspection_events (
                  session_id, seq, lane_kind, lane_id, call_id,
                  target_message_id, target_content_digest, tool_name,
                  tool_arguments, target_live, target_stubbed, outcome, reason,
                  reuse_target_tool_call_id, requested_chars, returned_chars,
                  avoided_chars
                )
                VALUES (?, ?, 'parent', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
                )
                .bind(session_id.as_i64())
                .bind(seq as i64)
                .bind(session_id.to_string())
                .bind(&record.call_id)
                .bind(&record.target_message_id)
                .bind(record.target_content_digest.to_hex().to_string())
                .bind(&record.tool_name)
                .bind(&record.tool_arguments)
                .bind(i64::from(record.target_live))
                .bind(i64::from(record.target_stubbed))
                .bind(record.admission.outcome.label())
                .bind(record.admission.reason.label())
                .bind(record.admission.reuse_target_tool_call_id.as_deref())
                .bind(usize_to_i64(record.admission.requested_chars))
                .bind(usize_to_i64(record.admission.returned_chars))
                .bind(usize_to_i64(record.admission.avoided_chars)),
            )?;
        }

        touch_session(tx, session_id, now).await
    }

    pub(super) async fn load_inspection_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<InspectionEventRecord>> {
        let rows = sqlx::query(
            r#"
            SELECT call_id, target_message_id, target_content_digest, tool_name,
                   tool_arguments, target_live, target_stubbed, outcome, reason,
                   reuse_target_tool_call_id, requested_chars, returned_chars,
                   avoided_chars
            FROM inspection_events
            WHERE session_id = ?
            ORDER BY seq
            "#,
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load inspection events for session {session_id}"))?;

        rows.into_iter()
            .map(inspection_event_record_from_row)
            .collect()
    }
}

fn inspection_event_record_from_row(row: sqlx::sqlite::SqliteRow) -> Result<InspectionEventRecord> {
    let outcome_text: String = row.try_get("outcome")?;
    let outcome = InspectionOutcome::from_label(&outcome_text)
        .with_context(|| format!("Invalid inspection outcome {outcome_text:?}"))?;
    let reason_text: String = row.try_get("reason")?;
    let reason = InspectionReason::from_label(&reason_text)
        .with_context(|| format!("Invalid inspection reason {reason_text:?}"))?;
    Ok(InspectionEventRecord {
        call_id: row.try_get("call_id")?,
        target_message_id: row.try_get("target_message_id")?,
        target_content_digest: parse_digest(row.try_get("target_content_digest")?)?,
        tool_name: row.try_get("tool_name")?,
        tool_arguments: row.try_get("tool_arguments")?,
        target_live: row.try_get::<i64, _>("target_live")? != 0,
        target_stubbed: row.try_get::<i64, _>("target_stubbed")? != 0,
        admission: ReadAdmissionMetadata {
            outcome,
            reason,
            reuse_target_tool_call_id: row.try_get("reuse_target_tool_call_id")?,
            requested_chars: nonnegative_usize(&row, "requested_chars")?,
            returned_chars: nonnegative_usize(&row, "returned_chars")?,
            avoided_chars: nonnegative_usize(&row, "avoided_chars")?,
        },
    })
}

fn read_evidence_record_from_row(row: sqlx::sqlite::SqliteRow) -> Result<ReadEvidenceRecord> {
    let provenance_text: String = row.try_get("provenance")?;
    let provenance = ReadProvenance::from_label(&provenance_text)
        .with_context(|| format!("Invalid read evidence provenance {provenance_text:?}"))?;
    let coverage_text: String = row.try_get("coverage")?;
    let coverage = ReadCoverage::from_label(&coverage_text)
        .with_context(|| format!("Invalid read evidence coverage {coverage_text:?}"))?;
    let freshness_text: String = row.try_get("baseline_status")?;
    let freshness = ReadFreshness::from_label(&freshness_text)
        .with_context(|| format!("Invalid read evidence freshness {freshness_text:?}"))?;
    let visible_digest = parse_digest(row.try_get::<String, _>("visible_digest")?)?;
    let file_digest_at_read = optional_digest(row.try_get("file_digest_at_read")?)?;
    let baseline_file_digest = optional_digest(row.try_get("baseline_file_digest")?)?;
    let canonical_path = PathBuf::from(row.try_get::<String, _>("canonical_path")?);

    let evidence = ReadEvidence::from_persisted_parts(
        row.try_get("display_path")?,
        canonical_path,
        ReadWindow {
            requested_offset: nonnegative_usize(&row, "requested_offset")?,
            requested_limit: nonnegative_usize(&row, "requested_limit")?,
            start_line: nonnegative_usize(&row, "displayed_start_line")?,
            end_line: optional_nonnegative_usize(&row, "displayed_end_line")?,
            total_lines: optional_nonnegative_usize(&row, "total_lines")?,
        },
        coverage,
        visible_digest,
        nonnegative_usize(&row, "visible_chars")?,
        file_digest_at_read,
        row.try_get::<Option<i64>, _>("baseline_modified_ms")?
            .and_then(system_time_from_ms),
        nonnegative_u64(&row, "baseline_len")?,
        baseline_file_digest,
        freshness,
        row.try_get::<i64, _>("observation_is_current")? != 0,
    );

    Ok(ReadEvidenceRecord {
        source_id: row.try_get("source_id")?,
        provenance,
        target_message_id: row.try_get("target_message_id")?,
        target_content_digest: parse_digest(row.try_get("target_content_digest")?)?,
        target_tool_call_id: row.try_get("target_tool_call_id")?,
        tool_name: row.try_get("tool_name")?,
        tool_arguments: row.try_get("tool_arguments")?,
        target_live: row.try_get::<i64, _>("target_live")? != 0,
        target_stubbed: row.try_get::<i64, _>("target_stubbed")? != 0,
        evidence,
        admission_outcome: row.try_get("admission_outcome")?,
        admission_reason: row.try_get("admission_reason")?,
        requested_chars: nonnegative_usize(&row, "requested_chars")?,
        returned_chars: nonnegative_usize(&row, "returned_chars")?,
        avoided_chars: nonnegative_usize(&row, "avoided_chars")?,
    })
}

fn parse_digest(value: String) -> Result<blake3::Hash> {
    blake3::Hash::from_hex(&value)
        .with_context(|| format!("Invalid persisted BLAKE3 digest {value:?}"))
}

fn optional_digest(value: Option<String>) -> Result<Option<blake3::Hash>> {
    value.map(parse_digest).transpose()
}

fn nonnegative_usize(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<usize> {
    let value: i64 = row.try_get(column)?;
    usize::try_from(value).with_context(|| format!("Invalid negative or oversized {column}"))
}

fn optional_nonnegative_usize(
    row: &sqlx::sqlite::SqliteRow,
    column: &str,
) -> Result<Option<usize>> {
    row.try_get::<Option<i64>, _>(column)?
        .map(|value| {
            usize::try_from(value)
                .with_context(|| format!("Invalid negative or oversized {column}"))
        })
        .transpose()
}

fn nonnegative_u64(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<u64> {
    let value: i64 = row.try_get(column)?;
    u64::try_from(value).with_context(|| format!("Invalid negative {column}"))
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
