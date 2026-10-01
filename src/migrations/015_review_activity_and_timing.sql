-- Last known activity per unit of work, so a patchset stranded by a daemon
-- restart can still explain what it was doing. Live state lives in memory;
-- this is the durable fallback, and rows are deleted when work completes.
--
-- The column ALTERs that go with this migration are applied from Rust, one per
-- missing column, because ALTER TABLE has no IF NOT EXISTS and databases
-- created by an earlier build of this feature already have them.
CREATE TABLE IF NOT EXISTS patchset_activity (
    key TEXT PRIMARY KEY,        -- "patchset:42", "patchset:42/patch:7/stage:locking", "commit:<sha>"
    patchset_id INTEGER,         -- the patchset's id; NULL for commit-keyed fetch work
    phase TEXT NOT NULL,         -- the serialised Phase, as JSON
    description TEXT NOT NULL,   -- human-readable summary
    updated_at INTEGER NOT NULL  -- unix seconds of the last report
);

CREATE INDEX IF NOT EXISTS idx_patchset_activity_patchset ON patchset_activity(patchset_id);
