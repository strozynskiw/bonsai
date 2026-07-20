-- The 1.0 schema baseline, squashed from the pre-release migration series.
-- Pre-release databases are not upgradable: delete ~/.bonsai/bonsai.db.
--
-- This file is frozen: sqlx checksums applied migrations, so editing it
-- bricks every existing database with "migration 1 was previously applied
-- but has been modified". Every schema change after 1.0 is a new additive
-- migrations/*.sql file.

CREATE TABLE user_preferences (
  key TEXT PRIMARY KEY NOT NULL,
  value TEXT NOT NULL
);

CREATE TABLE provider_settings (
  provider_id TEXT PRIMARY KEY NOT NULL,
  base_url TEXT NOT NULL,
  model TEXT NOT NULL,
  reasoning_json TEXT NOT NULL,
  model_reasoning_json TEXT NOT NULL,
  account_id TEXT NOT NULL DEFAULT '',
  is_fedramp_account INTEGER NOT NULL DEFAULT 0,
  authorized_at_ms INTEGER,
  context_window INTEGER
);

CREATE TABLE provider_credentials (
  provider_id TEXT PRIMARY KEY NOT NULL REFERENCES provider_settings(provider_id) ON DELETE CASCADE,
  source TEXT NOT NULL DEFAULT 'none',
  reference TEXT NOT NULL DEFAULT ''
);

CREATE TABLE projects (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  path TEXT NOT NULL UNIQUE,
  display_name TEXT NOT NULL
);

CREATE TABLE sessions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  provider_id TEXT NOT NULL,
  model TEXT NOT NULL,
  reasoning_json TEXT NOT NULL,
  prompt_token_count INTEGER NOT NULL DEFAULT 0,
  completion_token_count INTEGER NOT NULL DEFAULT 0,
  cache_read_input_token_count INTEGER NOT NULL DEFAULT 0,
  cache_creation_input_token_count INTEGER NOT NULL DEFAULT 0,
  cache_measured_input_token_count INTEGER NOT NULL DEFAULT 0,
  cost_micros INTEGER NOT NULL DEFAULT 0,
  summary TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'active',
  started_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  ended_at_ms INTEGER,
  last_heartbeat_ms INTEGER,
  busy INTEGER NOT NULL DEFAULT 0,
  terminal_reason TEXT,
  active_run_ms INTEGER NOT NULL DEFAULT 0 CHECK (active_run_ms >= 0),
  active_run_started_at_ms INTEGER
    CHECK (active_run_started_at_ms IS NULL OR active_run_started_at_ms >= 0),
  -- Preserves OpenAI-family prompt-cache routing across process resumes and
  -- provider/model rebuilds.
  conversation_cache_key TEXT NOT NULL DEFAULT '',
  -- Cumulative what-if-no-cache baseline, kept exact across resumes.
  no_cache_cost_micros INTEGER NOT NULL DEFAULT 0,
  source_plan_id INTEGER REFERENCES saved_plans(id) ON DELETE SET NULL
);

CREATE INDEX idx_sessions_project_updated ON sessions(project_id, updated_at_ms DESC);
CREATE INDEX idx_sessions_status ON sessions(status);
CREATE INDEX idx_sessions_project_status ON sessions(project_id, status);
CREATE UNIQUE INDEX idx_sessions_conversation_cache_key ON sessions(conversation_cache_key);
CREATE INDEX idx_sessions_source_plan_id ON sessions(source_plan_id);

CREATE TABLE messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  role TEXT NOT NULL,
  content TEXT NOT NULL,
  UNIQUE(session_id, seq)
);

CREATE VIRTUAL TABLE messages_fts USING fts5(
  content,
  role UNINDEXED,
  session_id UNINDEXED,
  content='messages',
  content_rowid='id'
);

CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
  INSERT INTO messages_fts(rowid, content, role, session_id)
  VALUES (new.id, new.content, new.role, new.session_id);
END;

CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, content, role, session_id)
  VALUES ('delete', old.id, old.content, old.role, old.session_id);
END;

CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
  INSERT INTO messages_fts(messages_fts, rowid, content, role, session_id)
  VALUES ('delete', old.id, old.content, old.role, old.session_id);
  INSERT INTO messages_fts(rowid, content, role, session_id)
  VALUES (new.id, new.content, new.role, new.session_id);
END;

CREATE TABLE context_messages (
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  raw_json TEXT NOT NULL,
  stable_id TEXT NOT NULL,
  PRIMARY KEY(session_id, seq)
);

CREATE UNIQUE INDEX idx_context_messages_session_stable_id
  ON context_messages(session_id, stable_id);

CREATE TABLE transcript_blocks (
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL,
  title TEXT NOT NULL DEFAULT '',
  body TEXT NOT NULL DEFAULT '',
  metadata_json TEXT NOT NULL DEFAULT '{}',
  PRIMARY KEY(session_id, seq)
);

CREATE TABLE tool_calls (
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  call_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  name TEXT NOT NULL,
  args_json TEXT NOT NULL DEFAULT '{}',
  result_json TEXT,
  diff_json TEXT,
  duration_ms INTEGER,
  status TEXT NOT NULL,
  started_at_ms INTEGER,
  finished_at_ms INTEGER,
  PRIMARY KEY(session_id, call_id)
);

CREATE INDEX idx_tool_calls_session_seq ON tool_calls(session_id, seq);

-- A session's live plan snapshot. Library entries are frozen separately in
-- saved_plans; the two share no rows.
CREATE TABLE plans (
  session_id INTEGER PRIMARY KEY NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  title TEXT NOT NULL DEFAULT '',
  revision INTEGER NOT NULL DEFAULT 0,
  updated_at_ms INTEGER NOT NULL
);

CREATE TABLE plan_sections (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER NOT NULL REFERENCES plans(session_id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  heading TEXT NOT NULL,
  body TEXT NOT NULL DEFAULT '',
  UNIQUE(session_id, seq)
);

CREATE TABLE plan_tasks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER NOT NULL REFERENCES plans(session_id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  text TEXT NOT NULL,
  done INTEGER NOT NULL DEFAULT 0,
  phase_seq INTEGER,
  UNIQUE(session_id, seq)
);

CREATE TABLE plan_questions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER NOT NULL REFERENCES plans(session_id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  text TEXT NOT NULL,
  UNIQUE(session_id, seq)
);

CREATE TABLE plan_phases (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER NOT NULL REFERENCES plans(session_id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  name TEXT NOT NULL,
  UNIQUE(session_id, seq)
);

CREATE TABLE plan_findings (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER NOT NULL REFERENCES plans(session_id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  severity TEXT NOT NULL,
  file TEXT,
  line INTEGER,
  issue TEXT NOT NULL,
  required_fix TEXT NOT NULL,
  acceptance_tests TEXT NOT NULL DEFAULT '[]',
  source_ids TEXT NOT NULL DEFAULT '[]',
  task TEXT,
  resolved INTEGER NOT NULL DEFAULT 0,
  UNIQUE(session_id, seq)
);

-- Library entries frozen at save time: the full PlanDoc lives in doc_json and
-- later live-plan edits never touch it. Saving again is the only update path.
CREATE TABLE saved_plans (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  title TEXT NOT NULL DEFAULT '',
  source_session_id INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
  branch TEXT,
  status TEXT NOT NULL DEFAULT 'draft',
  execution_session_id INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
  saved_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  section_count INTEGER NOT NULL DEFAULT 0,
  task_count INTEGER NOT NULL DEFAULT 0,
  doc_json TEXT NOT NULL
);

CREATE INDEX idx_saved_plans_project_saved
  ON saved_plans(project_id, saved_at_ms DESC);

CREATE TABLE todos (
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  content TEXT NOT NULL,
  status TEXT NOT NULL,
  PRIMARY KEY(session_id, seq)
);

CREATE TABLE permission_rules (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id INTEGER REFERENCES projects(id) ON DELETE CASCADE,
  pattern TEXT NOT NULL,
  decision TEXT NOT NULL,
  scope TEXT NOT NULL DEFAULT 'project',
  kind TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL
);

CREATE UNIQUE INDEX idx_permission_rules_unique
  ON permission_rules (kind, scope, pattern, COALESCE(project_id, -1));

CREATE TABLE context_controls (
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  node_id TEXT NOT NULL,
  state_json TEXT NOT NULL,
  PRIMARY KEY(session_id, node_id)
);

CREATE TABLE context_sources (
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  node_id TEXT NOT NULL,
  messages_json TEXT NOT NULL,
  PRIMARY KEY(session_id, node_id)
);

CREATE TABLE compaction_events (
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  occurred_at_ms INTEGER NOT NULL,
  before_tokens INTEGER NOT NULL,
  after_tokens INTEGER NOT NULL,
  messages_omitted INTEGER NOT NULL,
  tool_outputs_stubbed INTEGER NOT NULL,
  summary_available INTEGER NOT NULL,
  repack_id TEXT NOT NULL DEFAULT '',
  repack_reason TEXT NOT NULL DEFAULT '',
  prefix_hash_before TEXT NOT NULL DEFAULT '',
  prefix_hash_after TEXT NOT NULL DEFAULT '',
  cacheable_prefix_tokens_before INTEGER,
  cacheable_prefix_tokens_after INTEGER,
  PRIMARY KEY(session_id, seq)
);

CREATE TABLE usage_turns (
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  lane_kind TEXT NOT NULL CHECK (lane_kind IN ('parent', 'subagent', 'self_review', 'compaction')),
  lane_id TEXT NOT NULL CHECK (length(lane_id) > 0),
  lane_seq INTEGER NOT NULL CHECK (lane_seq > 0),
  parent_tool_call_id TEXT,
  launch_group_id TEXT,
  status TEXT NOT NULL CHECK (status IN ('reported', 'missing', 'interrupted')),
  finish_reason TEXT,
  reasoning_chars INTEGER NOT NULL,
  provider_attempts_json TEXT NOT NULL,
  provider_id TEXT,
  model TEXT,
  prompt_tokens INTEGER,
  completion_tokens INTEGER,
  cache_read_input_tokens INTEGER,
  cache_creation_input_tokens INTEGER,
  cache_measured_input_tokens INTEGER,
  turn_cost_micros INTEGER,
  no_cache_cost_micros INTEGER,
  estimated_prompt_tokens INTEGER,
  estimate_source TEXT,
  estimate_confidence TEXT,
  tool_schema_tokens INTEGER,
  tool_schema_hash TEXT,
  tool_schema_names_json TEXT NOT NULL,
  request_body_bytes INTEGER,
  request_body_hash TEXT,
  cache_mechanism TEXT,
  cache_route_fingerprint TEXT,
  expected_cacheable_percent INTEGER,
  actual_cache_read_percent INTEGER,
  local_reusable_prefix_tokens INTEGER,
  local_reusable_prefix_percent INTEGER,
  cacheable_prefix_tokens INTEGER,
  volatile_tail_tokens INTEGER,
  context_window_tokens INTEGER,
  rewrite_kind TEXT NOT NULL CHECK (rewrite_kind IN ('none', 'gc', 'compaction', 'manual', 'episode')),
  rewrite_saved_tokens INTEGER,
  created_at_ms INTEGER NOT NULL,
  latency_ms INTEGER,
  ttft_ms INTEGER,
  prefix_hash TEXT,
  inspection_executed INTEGER NOT NULL,
  inspection_reused INTEGER NOT NULL,
  inspection_rejected INTEGER NOT NULL,
  inspection_returned_chars INTEGER NOT NULL,
  inspection_avoided_chars INTEGER NOT NULL,
  delegated_parent_overlap INTEGER NOT NULL,
  episode_seq INTEGER,
  -- Request-local reasoning selection; NULL marks rows recorded before exact
  -- attribution existed.
  effective_reasoning TEXT,
  PRIMARY KEY(session_id, seq),
  UNIQUE(session_id, lane_kind, lane_id, lane_seq)
);

CREATE INDEX idx_usage_turns_session_lane
  ON usage_turns(session_id, lane_kind, lane_id, lane_seq);

-- Peer messaging: delivery is leased independently for the UI transcript and
-- agent context. A consumer acknowledges only after its durable snapshot is
-- committed; an expired lease can be claimed again after a crash.
CREATE TABLE agent_messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  from_session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  to_session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  kind TEXT NOT NULL,
  body TEXT NOT NULL,
  hop_count INTEGER NOT NULL DEFAULT 0,
  created_at_ms INTEGER NOT NULL,
  delivered_ui_at_ms INTEGER,
  delivered_agent_at_ms INTEGER,
  ui_lease_token TEXT,
  ui_lease_expires_at_ms INTEGER,
  agent_lease_token TEXT,
  agent_lease_expires_at_ms INTEGER,
  -- Done notices retain the exact wake subscription that authorized the
  -- resume; NULL for ordinary messages.
  wake_subscription_id INTEGER
    REFERENCES peer_wake_subscriptions(id) ON DELETE SET NULL,
  send_operation_id INTEGER
    REFERENCES peer_send_operations(id) ON DELETE SET NULL
);

CREATE INDEX idx_agent_messages_inbox
  ON agent_messages(to_session_id, delivered_ui_at_ms);

CREATE INDEX idx_agent_messages_ui_delivery
  ON agent_messages(to_session_id, delivered_ui_at_ms, ui_lease_expires_at_ms);

CREATE INDEX idx_agent_messages_agent_delivery
  ON agent_messages(to_session_id, delivered_agent_at_ms, agent_lease_expires_at_ms);

CREATE INDEX idx_agent_messages_wake_subscription
  ON agent_messages(wake_subscription_id);

CREATE INDEX idx_agent_messages_send_operation
  ON agent_messages(send_operation_id);

-- A replayed peer send must return the original committed fan-out without
-- duplicating recipients that were already written.
CREATE UNIQUE INDEX idx_agent_messages_send_operation_recipient
  ON agent_messages(send_operation_id, to_session_id)
  WHERE send_operation_id IS NOT NULL;

-- One wake subscription may produce at most one no-reply FYI. Done notices
-- use the same subscription link but a different kind and remain unaffected.
CREATE UNIQUE INDEX idx_unique_wake_request_message
  ON agent_messages(wake_subscription_id)
  WHERE wake_subscription_id IS NOT NULL AND kind = 'wake_request';

CREATE TABLE session_file_changes (
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  path TEXT NOT NULL,
  last_changed_at_ms INTEGER NOT NULL,
  PRIMARY KEY(session_id, path)
);

-- hop_count carries the anti-loop chain of the turn that created the
-- subscription into the done notice it eventually produces.
CREATE TABLE peer_wake_subscriptions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  requester_session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  target_session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  note TEXT NOT NULL DEFAULT '',
  created_at_ms INTEGER NOT NULL,
  fired_at_ms INTEGER,
  hop_count INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_wake_subs_target
  ON peer_wake_subscriptions(target_session_id, fired_at_ms);

-- A requester holds at most one outstanding wake for a target.
CREATE UNIQUE INDEX idx_unique_pending_peer_wake
  ON peer_wake_subscriptions(requester_session_id, target_session_id)
  WHERE fired_at_ms IS NULL;

-- A tool call is the durable identity of a peer send. The operation row is
-- inserted before any recipient rows, so SQLite serializes concurrent retries
-- and a replay can return the original committed fan-out.
CREATE TABLE peer_send_operations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  from_session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  idempotency_key TEXT NOT NULL,
  audience TEXT NOT NULL,
  kind TEXT NOT NULL,
  body TEXT NOT NULL,
  hop_count INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL,
  UNIQUE(from_session_id, idempotency_key)
);

-- Wake attempts need their own operation record: a repeated tool call may
-- have joined an already-pending subscription or been refused by the reverse
-- deadlock guard, and both outcomes must remain stable if the original
-- subscription fires before the call is replayed.
CREATE TABLE peer_wake_operations (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  requester_session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  idempotency_key TEXT NOT NULL,
  target_session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  note TEXT NOT NULL,
  hop_count INTEGER NOT NULL,
  outcome TEXT NOT NULL DEFAULT 'registering',
  subscription_id INTEGER REFERENCES peer_wake_subscriptions(id) ON DELETE CASCADE,
  fyi_message_id INTEGER REFERENCES agent_messages(id) ON DELETE SET NULL,
  created_at_ms INTEGER NOT NULL,
  UNIQUE(requester_session_id, idempotency_key)
);

CREATE TABLE peer_claims (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  claim TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  released_at_ms INTEGER,
  UNIQUE(session_id, claim)
);

CREATE TABLE memory_entries (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  tier TEXT NOT NULL,
  project_id INTEGER NOT NULL DEFAULT 0,
  name TEXT NOT NULL,
  entry_type TEXT NOT NULL,
  description TEXT NOT NULL,
  body TEXT NOT NULL,
  content_hash TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  UNIQUE(tier, project_id, name)
);

CREATE INDEX idx_memory_entries_project ON memory_entries(project_id);

CREATE VIRTUAL TABLE memory_fts USING fts5(
  name,
  description,
  body,
  project_id UNINDEXED,
  tier UNINDEXED,
  content='memory_entries',
  content_rowid='id',
  tokenize='porter unicode61'
);

CREATE TRIGGER memory_ai AFTER INSERT ON memory_entries BEGIN
  INSERT INTO memory_fts(rowid, name, description, body, project_id, tier)
  VALUES (new.id, new.name, new.description, new.body, new.project_id, new.tier);
END;

CREATE TRIGGER memory_ad AFTER DELETE ON memory_entries BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, name, description, body, project_id, tier)
  VALUES ('delete', old.id, old.name, old.description, old.body, old.project_id, old.tier);
END;

CREATE TRIGGER memory_au AFTER UPDATE ON memory_entries BEGIN
  INSERT INTO memory_fts(memory_fts, rowid, name, description, body, project_id, tier)
  VALUES ('delete', old.id, old.name, old.description, old.body, old.project_id, old.tier);
  INSERT INTO memory_fts(rowid, name, description, body, project_id, tier)
  VALUES (new.id, new.name, new.description, new.body, new.project_id, new.tier);
END;

CREATE TABLE memory_embeddings (
  content_hash TEXT NOT NULL,
  model_id TEXT NOT NULL,
  dims INTEGER NOT NULL,
  vector BLOB NOT NULL,
  PRIMARY KEY(content_hash, model_id)
);

CREATE TABLE workspace_locks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  path TEXT NOT NULL DEFAULT '.',
  mode TEXT NOT NULL,
  owner_session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  owner_pid INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL
);

CREATE INDEX idx_workspace_locks_project_path
  ON workspace_locks(project_id, path, expires_at_ms);

CREATE INDEX idx_workspace_locks_owner
  ON workspace_locks(owner_session_id, owner_pid);

CREATE TABLE read_evidence (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  lane_kind TEXT NOT NULL,
  lane_id TEXT NOT NULL,
  source_id TEXT NOT NULL,
  provenance TEXT NOT NULL CHECK (provenance IN ('parent_visible', 'mention_visible')),
  target_message_id TEXT NOT NULL,
  target_content_digest TEXT NOT NULL,
  target_tool_call_id TEXT,
  tool_name TEXT,
  tool_arguments TEXT,
  target_live INTEGER NOT NULL,
  target_stubbed INTEGER NOT NULL,
  canonical_path TEXT NOT NULL,
  display_path TEXT NOT NULL,
  requested_offset INTEGER NOT NULL,
  requested_limit INTEGER NOT NULL,
  displayed_start_line INTEGER NOT NULL,
  displayed_end_line INTEGER,
  total_lines INTEGER,
  coverage TEXT NOT NULL CHECK (coverage IN ('full', 'partial')),
  visible_digest TEXT NOT NULL,
  visible_chars INTEGER NOT NULL,
  file_digest_at_read TEXT,
  baseline_len INTEGER NOT NULL,
  baseline_modified_ms INTEGER,
  baseline_file_digest TEXT,
  baseline_status TEXT NOT NULL CHECK (baseline_status IN ('fresh', 'stale', 'deleted', 'unknown')),
  observation_is_current INTEGER NOT NULL,
  admission_outcome TEXT NOT NULL,
  admission_reason TEXT NOT NULL,
  requested_chars INTEGER NOT NULL,
  returned_chars INTEGER NOT NULL,
  avoided_chars INTEGER NOT NULL,
  UNIQUE(session_id, seq),
  UNIQUE(session_id, source_id)
);

CREATE INDEX idx_read_evidence_session_path
  ON read_evidence(session_id, canonical_path, displayed_start_line);

CREATE INDEX idx_read_evidence_session_target
  ON read_evidence(session_id, target_message_id, target_tool_call_id);

CREATE TABLE inspection_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  lane_kind TEXT NOT NULL,
  lane_id TEXT NOT NULL,
  call_id TEXT NOT NULL,
  target_message_id TEXT NOT NULL,
  target_content_digest TEXT NOT NULL,
  tool_name TEXT NOT NULL,
  tool_arguments TEXT NOT NULL,
  target_live INTEGER NOT NULL,
  target_stubbed INTEGER NOT NULL,
  outcome TEXT NOT NULL CHECK (outcome IN ('executed', 'reused', 'rejected')),
  reason TEXT NOT NULL CHECK (reason IN (
    'fresh_visible_coverage', 'no_fresh_visible_coverage',
    'not_reusable', 'tool_failed', 'repeated_fresh_reuse'
  )),
  reuse_target_tool_call_id TEXT,
  requested_chars INTEGER NOT NULL,
  returned_chars INTEGER NOT NULL,
  avoided_chars INTEGER NOT NULL,
  UNIQUE(session_id, seq),
  UNIQUE(session_id, call_id)
);

CREATE INDEX idx_inspection_events_session_outcome
  ON inspection_events(session_id, outcome, seq);

CREATE TABLE authorization_decisions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  tool_call_id TEXT,
  surface TEXT NOT NULL,
  subject TEXT NOT NULL,
  effects_json TEXT NOT NULL,
  risk_tier TEXT NOT NULL,
  rule_source TEXT NOT NULL,
  autonomy_level TEXT NOT NULL,
  sandbox_posture TEXT NOT NULL,
  decision TEXT NOT NULL,
  reason TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL
);

CREATE INDEX idx_authorization_decisions_session_created
  ON authorization_decisions(session_id, created_at_ms DESC, id DESC);

CREATE TABLE verification_runs (
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  kind TEXT NOT NULL,
  status TEXT NOT NULL,
  started_at_ms INTEGER NOT NULL,
  finished_at_ms INTEGER,
  observed_final_workspace INTEGER,
  workspace_changes_json TEXT NOT NULL DEFAULT '[]',
  repair_attempts INTEGER NOT NULL DEFAULT 0,
  terminal_reason TEXT,
  -- Which efforts the failed-check recovery ladder climbed through.
  reasoning_escalations_json TEXT NOT NULL DEFAULT '[]',
  PRIMARY KEY(session_id, seq)
);

CREATE TABLE verification_checks (
  session_id INTEGER NOT NULL,
  run_seq INTEGER NOT NULL,
  seq INTEGER NOT NULL,
  name TEXT NOT NULL,
  command TEXT NOT NULL,
  status TEXT NOT NULL,
  tool_call_id TEXT,
  exit_code INTEGER,
  completed_at_ms INTEGER,
  attempt_count INTEGER NOT NULL DEFAULT 0,
  last_failure_signature TEXT,
  PRIMARY KEY(session_id, run_seq, seq),
  FOREIGN KEY(session_id, run_seq)
    REFERENCES verification_runs(session_id, seq) ON DELETE CASCADE
);

CREATE INDEX idx_verification_runs_session_started
  ON verification_runs(session_id, started_at_ms DESC);

CREATE TABLE self_review_runs (
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL,
  started_at_ms INTEGER NOT NULL,
  mode TEXT NOT NULL,
  scope TEXT NOT NULL,
  diff_line_count INTEGER NOT NULL,
  reviewer_duration_ms INTEGER NOT NULL,
  reviewer_prompt_tokens INTEGER NOT NULL,
  reviewer_completion_tokens INTEGER NOT NULL,
  reviewer_cost_micros INTEGER,
  blocker_count INTEGER NOT NULL,
  major_count INTEGER NOT NULL,
  minor_count INTEGER NOT NULL,
  nit_count INTEGER NOT NULL,
  disposition TEXT,
  PRIMARY KEY(session_id, seq)
);

CREATE INDEX idx_self_review_runs_started
  ON self_review_runs(started_at_ms DESC);

-- Episodes: task-scoped context lifecycle.
--
-- `episodes` partitions a session's parent-lane timeline into contiguous topic
-- spans delimited by stable context-message ids. `episode_archive` stores the
-- raw model-facing messages an eviction removed from live context; it is
-- empty while an episode is merely tracked. Both tables are snapshot-replaced
-- per session in one transaction, mirroring the other agent-state snapshots.
--
-- Status lifecycle: active -> closed -> evicted -> restored. The CHECK below
-- pins the field combinations each status permits so a crash mid-transition
-- can never persist a contradictory row.
CREATE TABLE episodes (
  session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  seq INTEGER NOT NULL CHECK (seq > 0),                   -- 1-based per session
  title TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL CHECK (status IN ('active','closed','evicted','restored')),
  goal TEXT NOT NULL DEFAULT '',                          -- opening user message, one line
  card_md TEXT NOT NULL DEFAULT '',                       -- rendered card (also marker body)
  close_reason TEXT NOT NULL DEFAULT '',                  -- title_change|hard_boundary|manual
  start_stable_id TEXT NOT NULL CHECK (start_stable_id <> ''),
  end_stable_id TEXT,
  marker_stable_id TEXT,                                  -- msg-N of the spliced card marker
  files_touched_json TEXT NOT NULL DEFAULT '[]',
  opened_at_ms INTEGER NOT NULL,
  closed_at_ms INTEGER,
  evicted_at_ms INTEGER,
  evicted_tokens INTEGER,                                 -- estimator delta of the evict rewrite
  recall_count INTEGER NOT NULL DEFAULT 0 CHECK (recall_count >= 0),
  completable INTEGER NOT NULL DEFAULT 0 CHECK (completable IN (0,1)),
  CHECK (closed_at_ms IS NULL OR closed_at_ms >= opened_at_ms),
  CHECK (
    (status = 'active' AND closed_at_ms IS NULL AND end_stable_id IS NULL
      AND close_reason = '' AND marker_stable_id IS NULL AND evicted_at_ms IS NULL)
    OR
    (status = 'closed' AND closed_at_ms IS NOT NULL AND end_stable_id IS NOT NULL
      AND close_reason <> '' AND marker_stable_id IS NULL AND evicted_at_ms IS NULL)
    OR
    (status = 'evicted' AND closed_at_ms IS NOT NULL AND end_stable_id IS NOT NULL
      AND close_reason <> '' AND marker_stable_id IS NOT NULL AND evicted_at_ms IS NOT NULL)
    OR
    (status = 'restored' AND closed_at_ms IS NOT NULL AND end_stable_id IS NOT NULL
      AND close_reason <> '' AND marker_stable_id IS NULL AND evicted_at_ms IS NOT NULL)
  ),
  PRIMARY KEY(session_id, seq)
);

CREATE TABLE episode_archive (
  session_id INTEGER NOT NULL,
  episode_seq INTEGER NOT NULL,
  item_seq INTEGER NOT NULL CHECK (item_seq >= 0),
  stable_id TEXT NOT NULL CHECK (stable_id <> ''),
  raw_json TEXT NOT NULL,                                 -- one ChatCompletionRequestMessage
  PRIMARY KEY(session_id, episode_seq, item_seq),
  UNIQUE(session_id, episode_seq, stable_id),
  FOREIGN KEY(session_id, episode_seq) REFERENCES episodes(session_id, seq) ON DELETE CASCADE
);

CREATE TABLE recovery_points (
  id TEXT PRIMARY KEY NOT NULL,
  session_id INTEGER REFERENCES sessions(id) ON DELETE SET NULL,
  project_path TEXT NOT NULL,
  repository_path TEXT NOT NULL,
  worktree_path TEXT,
  baseline_ref TEXT NOT NULL,
  result_ref TEXT,
  source_index_tree TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN (
    'active', 'ready', 'merged', 'kept', 'discarded', 'failed'
  )),
  branch_name TEXT,
  error TEXT,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);

CREATE INDEX idx_recovery_points_project_created
  ON recovery_points(project_path, created_at_ms DESC);

CREATE INDEX idx_recovery_points_state_updated
  ON recovery_points(state, updated_at_ms DESC);

CREATE TABLE builtin_subagent_settings (
  subagent_id TEXT PRIMARY KEY NOT NULL,
  enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
  primary_model TEXT,
  primary_effort TEXT,
  fallback_model TEXT,
  fallback_effort TEXT,
  updated_at_ms INTEGER NOT NULL,
  CHECK (primary_model IS NOT NULL OR primary_effort IS NULL),
  CHECK (fallback_model IS NOT NULL OR fallback_effort IS NULL)
);
