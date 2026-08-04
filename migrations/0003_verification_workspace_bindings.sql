ALTER TABLE verification_runs ADD COLUMN terminal_reason_kind TEXT;
ALTER TABLE verification_runs ADD COLUMN delivered_binding_json TEXT;

ALTER TABLE verification_checks ADD COLUMN binding_json TEXT;
ALTER TABLE verification_checks ADD COLUMN delivered_binding_json TEXT;
ALTER TABLE verification_checks ADD COLUMN attempt_timestamps_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE verification_checks ADD COLUMN failure_signatures_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE verification_checks ADD COLUMN terminal_reason_kind TEXT;
