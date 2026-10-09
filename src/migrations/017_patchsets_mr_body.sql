-- A pull request's description, stored per revision in patchsets.mr_body.
--
-- The column is added from Rust, only when it is missing, because ALTER TABLE
-- has no IF NOT EXISTS and databases written by the fork already have it. This
-- file holds only what is idempotent as written, so it runs on every database
-- that reaches version 17, including one that has the column but not the index.

-- A pull request's revisions share mr_url, so it is the key that gathers every
-- revision of one pull request.
CREATE INDEX IF NOT EXISTS idx_patchsets_mr_url ON patchsets(mr_url);
