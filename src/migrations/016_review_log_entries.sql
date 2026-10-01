-- Scratch space for a running review's conversation, so it can be read before
-- the review ends and its complete log lands in reviews.logs.
--
-- stage is a stage NAME, hence TEXT. An earlier build of this feature created
-- the table at runtime with stage INTEGER, and SQLite's type affinity hid the
-- disagreement; Database::migrate drops a table of that shape before this runs.
-- message is a serialised AiMessage, the type the finished log stores, and is
-- compressed the way reviews.logs is. Rows are deleted when the review completes
-- (Database::complete_review) and swept at startup for reviews no longer
-- running, so nothing here outlives the review it belongs to.
CREATE TABLE IF NOT EXISTS review_log_entries (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL,
    seq INTEGER NOT NULL,          -- position in the review's conversation; a rerun of the row continues past the last run's
    stage TEXT,                    -- stage name; NULL for the system message
    role TEXT NOT NULL,            -- AiRole as serde writes it: system, user, assistant, tool
    message TEXT NOT NULL,         -- the serialised AiMessage
    created_at INTEGER NOT NULL    -- unix seconds
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_review_log_entries_review ON review_log_entries(review_id, seq);
