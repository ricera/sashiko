-- Which read-only reference repositories a review had attached, why, at which
-- commit, and how many tool reads it served from each, as JSON. Recorded per
-- review because the per-patch log line is the only other record and the
-- deployment keeps its logs for about an hour.
ALTER TABLE reviews ADD COLUMN reference_repos TEXT;
