-- The profile's "My comments" list (`myComments`) reads one user's comments newest
-- first. `comments` was only indexed by target and parent, so that was a full scan.
CREATE INDEX IF NOT EXISTS idx_comments_user ON comments(user_id, created_at DESC);
