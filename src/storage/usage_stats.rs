//! Cross-session usage aggregates for the `/usage` dashboard: lifetime
//! activity, per-day buckets, per-model/per-project rollups, and tool-call
//! stats. Every query here is a deliberate full scan — the tables hold at most
//! tens of thousands of rows and the GROUP BYs run over expressions an index
//! cannot serve.
//!
//! Calendar math happens entirely in SQLite (`'localtime'`): day buckets,
//! "today", and weekday all come from the same timezone engine, so the
//! heatmap can never disagree with its own buckets. Bonsai has no Rust date
//! dependency, deliberately.
//!
//! Nested-agent turns retain their execution lane and per-turn model identity.
//! Session totals remain the billing rollup; turn queries use lane-local warmup
//! and rewrite exclusions so child cold starts never count against the parent.

use super::*;

/// "Today" as SQLite's `'localtime'` sees it — the sole time authority for
/// streaks and heatmap alignment. `weekday` is 0 = Sunday (`strftime('%w')`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalToday {
    pub day: String,
    pub julian_day: i64,
    pub weekday: u8,
}

/// One local calendar day's parent-turn activity (absent days had none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyUsage {
    /// Local date, `YYYY-MM-DD`.
    pub day: String,
    /// Integer Julian day number of `day`; gap-safe arithmetic for weeks,
    /// streaks, and windows without parsing the date string.
    pub julian_day: i64,
    /// 0 = Sunday, matching `strftime('%w')`.
    pub weekday: u8,
    pub turns: i64,
    pub sessions: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_measured_tokens: i64,
    /// Sum over turns whose cost is known; unknown-cost turns contribute 0.
    pub cost_micros: i64,
    /// Cache savings summed only over turns where both the actual and the
    /// no-cache cost are known, so savings are never fabricated.
    pub savings_micros: i64,
    /// Lane-warm, non-rewrite turns that read 0% from cache.
    pub cold_turns: i64,
    /// Lane-warm, non-rewrite turns that reported a cache measurement at all.
    pub warm_eligible_turns: i64,
}

impl DailyUsage {
    /// Total tokens moved that day; the heatmap intensity metric.
    pub fn total_tokens(&self) -> i64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Rollup for one provider/model pair across all sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelUsage {
    pub provider_id: String,
    pub model: String,
    pub turns: i64,
    pub sessions: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_micros: i64,
    /// Turns whose cost is unknown; when > 0 the cost above is a floor.
    pub unknown_cost_turns: i64,
    pub cache_read_tokens: i64,
    pub cache_measured_tokens: i64,
    /// Test/eval rows without a model identity attributed to the session model.
    pub fallback_attributed_turns: i64,
    pub last_used_ms: i64,
}

impl ModelUsage {
    /// Lifetime cache hit rate for this model, when anything was measured.
    pub fn cache_hit_percent(&self) -> Option<i64> {
        (self.cache_measured_tokens > 0)
            .then(|| self.cache_read_tokens.saturating_mul(100) / self.cache_measured_tokens)
    }
}

/// One of the costliest sessions on record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpensiveSession {
    pub id: SessionId,
    pub name: String,
    pub project_path: String,
    pub provider_id: String,
    pub model: String,
    pub cost_micros: i64,
    pub token_count: i64,
    pub started_at_ms: i64,
}

/// Session-duration aggregates. Duration is
/// `COALESCE(ended_at_ms, updated_at_ms) - started_at_ms`: `updated_at_ms` is
/// touched on every persistence flush, so a crashed or still-active session is
/// bounded by its last real activity instead of needing an artificial cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStats {
    pub total_sessions: i64,
    pub avg_duration_ms: i64,
    pub longest_duration_ms: i64,
    pub longest_session_name: String,
}

/// Per-project rollup from the `sessions` usage columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectUsage {
    pub name: String,
    pub sessions: i64,
    pub tokens: i64,
    pub cost_micros: i64,
}

/// Per-tool-call rollup over finished calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUsage {
    pub name: String,
    pub calls: i64,
    pub failed: i64,
    pub avg_duration_ms: i64,
    pub max_duration_ms: i64,
}

/// Lifetime totals from the `sessions` rollup columns — the complete
/// accounting (includes nested-subagent usage that `usage_turns` lacks).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LifetimeTotals {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_measured_tokens: i64,
    /// Sum over sessions with known cost (`cost_micros >= 0`).
    pub cost_micros: i64,
    /// Sessions whose cost is unknown; when > 0 the sum above is a floor.
    pub unknown_cost_sessions: i64,
}

impl LifetimeTotals {
    pub fn total_tokens(&self) -> i64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Everything the `/usage` modal renders, loaded in one call so the modal
/// variant owns immutable data and rendering stays pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageDashboard {
    pub today: LocalToday,
    /// Ascending by day; sparse — only days with at least one usage turn.
    pub days: Vec<DailyUsage>,
    /// Ordered by cost, then input tokens, descending.
    pub models: Vec<ModelUsage>,
    /// Top sessions by known cost, descending.
    pub top_sessions: Vec<ExpensiveSession>,
    pub session_stats: SessionStats,
    /// Top projects by tokens, descending.
    pub projects: Vec<ProjectUsage>,
    /// Ordered by count, descending.
    pub status_counts: Vec<(SessionStatus, i64)>,
    /// Ordered by call count, descending.
    pub tools: Vec<ToolUsage>,
    pub self_review: crate::self_review::SelfReviewStats,
    pub lifetime: LifetimeTotals,
}

impl UsageDashboard {
    /// The busiest day on record, by total tokens.
    pub fn peak_day(&self) -> Option<&DailyUsage> {
        self.days.iter().max_by_key(|day| day.total_tokens())
    }

    /// Consecutive active days ending today — or yesterday, so a streak isn't
    /// reported broken while today is merely still in progress. 0 when the
    /// last activity is older than yesterday.
    pub fn current_streak_days(&self) -> i64 {
        let mut streak = 0;
        let mut expected = self.today.julian_day;
        for day in self.days.iter().rev() {
            if streak == 0 && day.julian_day == expected - 1 {
                // Today has no activity yet; anchor the streak at yesterday.
                expected -= 1;
            }
            if day.julian_day != expected {
                break;
            }
            streak += 1;
            expected -= 1;
        }
        streak
    }

    /// Days within the trailing `window` local days (including today).
    pub fn window_days(&self, window: i64) -> impl Iterator<Item = &DailyUsage> {
        let first = self.today.julian_day - window.max(1) + 1;
        self.days.iter().filter(move |day| day.julian_day >= first)
    }

    /// Lifetime cache savings summed over turns where both costs were known.
    pub fn savings_micros(&self) -> i64 {
        self.days.iter().map(|day| day.savings_micros).sum()
    }
}

impl Storage {
    /// Load every aggregate the `/usage` dashboard shows. A handful of full
    /// scans over small tables; comfortably under interactive latency.
    pub async fn load_usage_dashboard(&self) -> Result<UsageDashboard> {
        let today = self.load_local_today().await?;
        let days = self.load_daily_usage().await?;
        let models = self.load_model_usage().await?;
        let top_sessions = self.load_expensive_sessions(5).await?;
        let session_stats = self.load_session_stats().await?;
        let projects = self.load_project_usage(8).await?;
        let status_counts = self.load_session_status_counts().await?;
        let tools = self.load_tool_usage().await?;
        let self_review = self.load_self_review_stats().await?;
        let lifetime = self.load_lifetime_totals().await?;
        Ok(UsageDashboard {
            today,
            days,
            models,
            top_sessions,
            session_stats,
            projects,
            status_counts,
            tools,
            self_review,
            lifetime,
        })
    }

    async fn load_local_today(&self) -> Result<LocalToday> {
        let row = sqlx::query(
            "SELECT date('now','localtime') AS day, \
             CAST(strftime('%w','now','localtime') AS INTEGER) AS weekday, \
             CAST(julianday(date('now','localtime')) AS INTEGER) AS julian_day",
        )
        .fetch_one(&self.pool)
        .await
        .context("Failed to resolve local today")?;
        Ok(LocalToday {
            day: row.try_get("day")?,
            julian_day: row.try_get("julian_day")?,
            weekday: row.try_get::<i64, _>("weekday")? as u8,
        })
    }

    async fn load_daily_usage(&self) -> Result<Vec<DailyUsage>> {
        // Cost guards accept both NULL (usage_turns convention) and the -1
        // sentinel (sessions convention) as "unknown" via `>= 0`.
        let rows = sqlx::query(
            "SELECT date(created_at_ms/1000,'unixepoch','localtime') AS day, \
             CAST(julianday(date(created_at_ms/1000,'unixepoch','localtime')) AS INTEGER) AS julian_day, \
             CAST(strftime('%w', created_at_ms/1000,'unixepoch','localtime') AS INTEGER) AS weekday, \
             COUNT(*) AS turns, \
             COUNT(DISTINCT session_id) AS sessions, \
             SUM(COALESCE(prompt_tokens, 0)) AS input_tokens, \
             SUM(COALESCE(completion_tokens, 0)) AS output_tokens, \
             SUM(COALESCE(cache_read_input_tokens, 0)) AS cache_read_tokens, \
             SUM(COALESCE(cache_creation_input_tokens, 0)) AS cache_creation_tokens, \
             SUM(COALESCE(cache_measured_input_tokens, 0)) AS cache_measured_tokens, \
             SUM(CASE WHEN turn_cost_micros >= 0 THEN turn_cost_micros ELSE 0 END) AS cost_micros, \
             SUM(CASE WHEN turn_cost_micros >= 0 AND no_cache_cost_micros >= 0 \
                 THEN no_cache_cost_micros - turn_cost_micros ELSE 0 END) AS savings_micros, \
             SUM(CASE WHEN lane_seq > 1 AND rewrite_kind = 'none' AND actual_cache_read_percent = 0 THEN 1 ELSE 0 END) AS cold_turns, \
             SUM(CASE WHEN lane_seq > 1 AND rewrite_kind = 'none' AND actual_cache_read_percent IS NOT NULL THEN 1 ELSE 0 END) AS warm_eligible_turns \
             FROM usage_turns \
             GROUP BY 1, 2, 3 \
             ORDER BY 1",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to load daily usage")?;
        rows.into_iter()
            .map(|row| {
                Ok(DailyUsage {
                    day: row.try_get("day")?,
                    julian_day: row.try_get("julian_day")?,
                    weekday: row.try_get::<i64, _>("weekday")? as u8,
                    turns: row.try_get("turns")?,
                    sessions: row.try_get("sessions")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cache_read_tokens: row.try_get("cache_read_tokens")?,
                    cache_creation_tokens: row.try_get("cache_creation_tokens")?,
                    cache_measured_tokens: row.try_get("cache_measured_tokens")?,
                    cost_micros: row.try_get("cost_micros")?,
                    savings_micros: row.try_get("savings_micros")?,
                    cold_turns: row.try_get("cold_turns")?,
                    warm_eligible_turns: row.try_get("warm_eligible_turns")?,
                })
            })
            .collect()
    }

    async fn load_model_usage(&self) -> Result<Vec<ModelUsage>> {
        let rows = sqlx::query(
            "SELECT COALESCE(NULLIF(ut.provider_id, ''), s.provider_id) AS provider_id, \
             COALESCE(NULLIF(ut.model, ''), s.model) AS model, \
             COUNT(*) AS turns, \
             COUNT(DISTINCT ut.session_id) AS sessions, \
             SUM(COALESCE(ut.prompt_tokens, 0)) AS input_tokens, \
             SUM(COALESCE(ut.completion_tokens, 0)) AS output_tokens, \
             SUM(CASE WHEN ut.turn_cost_micros >= 0 THEN ut.turn_cost_micros ELSE 0 END) AS cost_micros, \
             SUM(CASE WHEN ut.turn_cost_micros IS NULL OR ut.turn_cost_micros < 0 THEN 1 ELSE 0 END) AS unknown_cost_turns, \
             SUM(COALESCE(ut.cache_read_input_tokens, 0)) AS cache_read_tokens, \
             SUM(COALESCE(ut.cache_measured_input_tokens, 0)) AS cache_measured_tokens, \
             SUM(CASE WHEN ut.model IS NULL OR ut.model = '' THEN 1 ELSE 0 END) AS fallback_attributed_turns, \
             MAX(ut.created_at_ms) AS last_used_ms \
             FROM usage_turns ut \
             JOIN sessions s ON s.id = ut.session_id \
             GROUP BY 1, 2 \
             ORDER BY cost_micros DESC, input_tokens DESC",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to load per-model usage")?;
        rows.into_iter()
            .map(|row| {
                Ok(ModelUsage {
                    provider_id: row.try_get("provider_id")?,
                    model: row.try_get("model")?,
                    turns: row.try_get("turns")?,
                    sessions: row.try_get("sessions")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost_micros: row.try_get("cost_micros")?,
                    unknown_cost_turns: row.try_get("unknown_cost_turns")?,
                    cache_read_tokens: row.try_get("cache_read_tokens")?,
                    cache_measured_tokens: row.try_get("cache_measured_tokens")?,
                    fallback_attributed_turns: row.try_get("fallback_attributed_turns")?,
                    last_used_ms: row.try_get("last_used_ms")?,
                })
            })
            .collect()
    }

    async fn load_expensive_sessions(&self, limit: i64) -> Result<Vec<ExpensiveSession>> {
        // `cost_micros > 0` also excludes the -1 unknown-cost sentinel.
        let rows = sqlx::query(
            "SELECT sessions.id, sessions.name, projects.path AS project_path, \
             sessions.provider_id, sessions.model, sessions.cost_micros, \
             sessions.prompt_token_count + sessions.completion_token_count AS token_count, \
             sessions.started_at_ms \
             FROM sessions JOIN projects ON projects.id = sessions.project_id \
             WHERE sessions.cost_micros > 0 \
             ORDER BY sessions.cost_micros DESC \
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("Failed to load most expensive sessions")?;
        rows.into_iter()
            .map(|row| {
                Ok(ExpensiveSession {
                    id: SessionId::from_raw(row.try_get("id")?),
                    name: row.try_get("name")?,
                    project_path: row.try_get("project_path")?,
                    provider_id: row.try_get("provider_id")?,
                    model: row.try_get("model")?,
                    cost_micros: row.try_get("cost_micros")?,
                    token_count: row.try_get("token_count")?,
                    started_at_ms: row.try_get("started_at_ms")?,
                })
            })
            .collect()
    }

    async fn load_session_stats(&self) -> Result<SessionStats> {
        let aggregate = sqlx::query(
            "SELECT COUNT(*) AS total_sessions, \
             CAST(COALESCE(AVG(COALESCE(ended_at_ms, updated_at_ms) - started_at_ms), 0) AS INTEGER) AS avg_duration_ms, \
             COALESCE(MAX(COALESCE(ended_at_ms, updated_at_ms) - started_at_ms), 0) AS longest_duration_ms \
             FROM sessions \
             WHERE COALESCE(ended_at_ms, updated_at_ms) > started_at_ms",
        )
        .fetch_one(&self.pool)
        .await
        .context("Failed to load session duration stats")?;
        let longest = sqlx::query(
            "SELECT name FROM sessions \
             WHERE COALESCE(ended_at_ms, updated_at_ms) > started_at_ms \
             ORDER BY COALESCE(ended_at_ms, updated_at_ms) - started_at_ms DESC \
             LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .context("Failed to load longest session")?;
        Ok(SessionStats {
            total_sessions: aggregate.try_get("total_sessions")?,
            avg_duration_ms: aggregate.try_get("avg_duration_ms")?,
            longest_duration_ms: aggregate.try_get("longest_duration_ms")?,
            longest_session_name: longest
                .map(|row| row.try_get("name"))
                .transpose()?
                .unwrap_or_default(),
        })
    }

    async fn load_project_usage(&self, limit: i64) -> Result<Vec<ProjectUsage>> {
        let rows = sqlx::query(
            "SELECT COALESCE(NULLIF(projects.display_name, ''), projects.path) AS name, \
             COUNT(*) AS sessions, \
             SUM(sessions.prompt_token_count + sessions.completion_token_count) AS tokens, \
             SUM(CASE WHEN sessions.cost_micros >= 0 THEN sessions.cost_micros ELSE 0 END) AS cost_micros \
             FROM sessions JOIN projects ON projects.id = sessions.project_id \
             GROUP BY projects.id \
             ORDER BY tokens DESC \
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("Failed to load per-project usage")?;
        rows.into_iter()
            .map(|row| {
                Ok(ProjectUsage {
                    name: row.try_get("name")?,
                    sessions: row.try_get("sessions")?,
                    tokens: row.try_get("tokens")?,
                    cost_micros: row.try_get("cost_micros")?,
                })
            })
            .collect()
    }

    async fn load_session_status_counts(&self) -> Result<Vec<(SessionStatus, i64)>> {
        let rows = sqlx::query(
            "SELECT status, COUNT(*) AS count FROM sessions GROUP BY status ORDER BY count DESC",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to load session status counts")?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    SessionStatus::from_db_str(&row.try_get::<String, _>("status")?),
                    row.try_get("count")?,
                ))
            })
            .collect()
    }

    async fn load_tool_usage(&self) -> Result<Vec<ToolUsage>> {
        let rows = sqlx::query(
            "SELECT name, COUNT(*) AS calls, \
             SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END) AS failed, \
             CAST(COALESCE(AVG(duration_ms), 0) AS INTEGER) AS avg_duration_ms, \
             COALESCE(MAX(duration_ms), 0) AS max_duration_ms \
             FROM tool_calls \
             WHERE status != 'running' \
             GROUP BY name \
             ORDER BY calls DESC",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to load tool usage")?;
        rows.into_iter()
            .map(|row| {
                Ok(ToolUsage {
                    name: row.try_get("name")?,
                    calls: row.try_get("calls")?,
                    failed: row.try_get("failed")?,
                    avg_duration_ms: row.try_get("avg_duration_ms")?,
                    max_duration_ms: row.try_get("max_duration_ms")?,
                })
            })
            .collect()
    }

    async fn load_lifetime_totals(&self) -> Result<LifetimeTotals> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(prompt_token_count), 0) AS input_tokens, \
             COALESCE(SUM(completion_token_count), 0) AS output_tokens, \
             COALESCE(SUM(cache_read_input_token_count), 0) AS cache_read_tokens, \
             COALESCE(SUM(cache_creation_input_token_count), 0) AS cache_creation_tokens, \
             COALESCE(SUM(cache_measured_input_token_count), 0) AS cache_measured_tokens, \
             COALESCE(SUM(CASE WHEN cost_micros >= 0 THEN cost_micros ELSE 0 END), 0) AS cost_micros, \
             COALESCE(SUM(CASE WHEN cost_micros < 0 THEN 1 ELSE 0 END), 0) AS unknown_cost_sessions \
             FROM sessions",
        )
        .fetch_one(&self.pool)
        .await
        .context("Failed to load lifetime totals")?;
        Ok(LifetimeTotals {
            input_tokens: row.try_get("input_tokens")?,
            output_tokens: row.try_get("output_tokens")?,
            cache_read_tokens: row.try_get("cache_read_tokens")?,
            cache_creation_tokens: row.try_get("cache_creation_tokens")?,
            cache_measured_tokens: row.try_get("cache_measured_tokens")?,
            cost_micros: row.try_get("cost_micros")?,
            unknown_cost_sessions: row.try_get("unknown_cost_sessions")?,
        })
    }
}

#[cfg(test)]
mod derived_tests {
    use super::*;

    fn day(julian_day: i64, tokens: i64) -> DailyUsage {
        DailyUsage {
            day: format!("day-{julian_day}"),
            julian_day,
            weekday: (julian_day % 7) as u8,
            turns: 1,
            sessions: 1,
            input_tokens: tokens,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cache_measured_tokens: 0,
            cost_micros: 0,
            savings_micros: 0,
            cold_turns: 0,
            warm_eligible_turns: 0,
        }
    }

    fn dashboard(today_julian: i64, days: Vec<DailyUsage>) -> UsageDashboard {
        UsageDashboard {
            today: LocalToday {
                day: "today".to_string(),
                julian_day: today_julian,
                weekday: 0,
            },
            days,
            models: Vec::new(),
            top_sessions: Vec::new(),
            session_stats: SessionStats::default(),
            projects: Vec::new(),
            status_counts: Vec::new(),
            tools: Vec::new(),
            self_review: Default::default(),
            lifetime: LifetimeTotals::default(),
        }
    }

    #[test]
    fn streak_counts_consecutive_days_ending_today() {
        let dash = dashboard(100, vec![day(97, 1), day(98, 1), day(99, 1), day(100, 1)]);
        assert_eq!(dash.current_streak_days(), 4);
    }

    #[test]
    fn streak_survives_inactive_today() {
        let dash = dashboard(100, vec![day(98, 1), day(99, 1)]);
        assert_eq!(dash.current_streak_days(), 2);
    }

    #[test]
    fn streak_breaks_on_gap() {
        let dash = dashboard(100, vec![day(95, 1), day(96, 1), day(100, 1)]);
        assert_eq!(dash.current_streak_days(), 1);
        let stale = dashboard(100, vec![day(97, 1), day(98, 1)]);
        assert_eq!(stale.current_streak_days(), 0);
    }

    #[test]
    fn streak_empty_history_is_zero() {
        assert_eq!(dashboard(100, Vec::new()).current_streak_days(), 0);
    }

    #[test]
    fn peak_day_prefers_highest_total() {
        let dash = dashboard(100, vec![day(98, 5), day(99, 12), day(100, 3)]);
        assert_eq!(dash.peak_day().map(|d| d.julian_day), Some(99));
    }

    #[test]
    fn window_days_is_inclusive_of_today() {
        let dash = dashboard(100, vec![day(70, 1), day(71, 1), day(99, 1), day(100, 1)]);
        let last_30: Vec<i64> = dash.window_days(30).map(|d| d.julian_day).collect();
        assert_eq!(last_30, vec![71, 99, 100]);
    }
}
