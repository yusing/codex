-- The derived count describes the same processed prefix as the projection checkpoint.
-- Older projections did not retain completeness evidence; their count stays unavailable.
ALTER TABLE thread_history_projection_state ADD COLUMN completed_compactions INTEGER;

CREATE TRIGGER thread_items_compaction_insert AFTER INSERT ON thread_items
WHEN NEW.item_type = 'contextCompaction' AND NEW.completed_at_ms IS NOT NULL
BEGIN
    UPDATE thread_history_projection_state
    SET completed_compactions = completed_compactions + 1
    WHERE thread_id = NEW.thread_id;
END;

CREATE TRIGGER thread_items_compaction_complete AFTER UPDATE ON thread_items
WHEN NEW.item_type = 'contextCompaction' AND NEW.completed_at_ms IS NOT NULL
 AND (OLD.item_type != 'contextCompaction' OR OLD.completed_at_ms IS NULL)
BEGIN
    UPDATE thread_history_projection_state
    SET completed_compactions = completed_compactions + 1
    WHERE thread_id = NEW.thread_id;
END;
