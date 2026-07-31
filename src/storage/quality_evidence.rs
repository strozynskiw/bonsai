use std::collections::{HashMap, HashSet};

use serde::Serialize;

use super::*;
use crate::self_review::SelfReviewRunRecord;
use crate::verification::VerificationRunRecord;

const SELF_REVIEW_FINGERPRINT_DOMAIN: &str = "bonsai:self-review-evidence:v1";
const VERIFICATION_FINGERPRINT_DOMAIN: &str = "bonsai:verification-evidence:v1";

pub(super) fn self_review_fingerprint(run: &SelfReviewRunRecord) -> Result<String> {
    evidence_fingerprint(SELF_REVIEW_FINGERPRINT_DOMAIN, run)
}

pub(super) fn verification_fingerprint(run: &VerificationRunRecord) -> Result<String> {
    evidence_fingerprint(VERIFICATION_FINGERPRINT_DOMAIN, run)
}

pub(super) fn duplicate_self_review_fingerprints(
    runs: &[(SessionId, SelfReviewRunRecord)],
) -> Result<HashSet<String>> {
    duplicate_cross_session_fingerprints(runs, self_review_fingerprint)
}

fn duplicate_verification_fingerprints(
    runs: &[(SessionId, VerificationRunRecord)],
) -> Result<HashSet<String>> {
    duplicate_cross_session_fingerprints(runs, verification_fingerprint)
}

fn duplicate_cross_session_fingerprints<T>(
    runs: &[(SessionId, T)],
    fingerprint: impl Fn(&T) -> Result<String>,
) -> Result<HashSet<String>> {
    let mut first_sessions = HashMap::<String, SessionId>::new();
    let mut duplicates = HashSet::new();
    for (session_id, run) in runs {
        let fingerprint = fingerprint(run)?;
        match first_sessions.entry(fingerprint.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(*session_id);
            }
            std::collections::hash_map::Entry::Occupied(entry) if entry.get() != session_id => {
                duplicates.insert(fingerprint);
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
        }
    }
    Ok(duplicates)
}

fn evidence_fingerprint<T: Serialize>(domain: &str, value: &T) -> Result<String> {
    let encoded = serde_json::to_vec(value).context("Failed to encode quality evidence")?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    hasher.update(&encoded);
    Ok(hasher.finalize().to_hex().to_string())
}

impl Storage {
    /// Count verification rows whose exact session-agnostic fingerprints occur
    /// in more than one durable session. Every member of such a group is
    /// quarantined because persisted data cannot prove which row was original.
    pub(super) async fn load_quarantined_verification_run_count(&self) -> Result<i64> {
        let candidate_rows = sqlx::query(
            r#"
            SELECT DISTINCT current.session_id
            FROM verification_runs current
            WHERE EXISTS (
                SELECT 1
                FROM verification_runs other
                WHERE other.session_id != current.session_id
                  AND other.started_at_ms = current.started_at_ms
            )
            ORDER BY current.session_id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to load duplicate verification candidates")?;

        let mut runs = Vec::new();
        for row in candidate_rows {
            let session_id = SessionId::from_raw(row.try_get("session_id")?);
            runs.extend(
                self.load_verification_runs(session_id)
                    .await?
                    .into_iter()
                    .map(|run| (session_id, run)),
            );
        }

        let duplicates = duplicate_verification_fingerprints(&runs)?;
        let quarantined = runs
            .iter()
            .map(|(_, run)| verification_fingerprint(run))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|fingerprint| duplicates.contains(fingerprint))
            .count();
        i64::try_from(quarantined).context("Quarantined verification count is out of range")
    }
}
