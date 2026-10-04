-- Hourly GC removes only already-read notifications created more than 90 days
-- ago. This partial index bounds each eligibility lookup without scanning unread
-- history. No data is removed during migration; runtime deletes use small batches.
CREATE INDEX idx_notifications_read_created
    ON notifications(created_at, id) WHERE read_at IS NOT NULL;
