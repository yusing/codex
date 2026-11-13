-- A count belongs to a logical thread and a decoded history boundary, not to
-- the current file representation or to an app-server attachment.
ALTER TABLE thread_history_projection_state ADD COLUMN compaction_owner_id TEXT;
ALTER TABLE thread_history_projection_state ADD COLUMN compaction_byte_offset INTEGER;
ALTER TABLE thread_history_projection_state ADD COLUMN compaction_next_ordinal INTEGER;
ALTER TABLE thread_history_projection_state ADD COLUMN compaction_revision TEXT;
ALTER TABLE thread_items ADD COLUMN compaction_actor_id TEXT;

DROP TRIGGER thread_items_compaction_insert;
DROP TRIGGER thread_items_compaction_complete;

CREATE TRIGGER thread_items_compaction_insert AFTER INSERT ON thread_items
WHEN NEW.item_type = 'contextCompaction' AND NEW.completed_at_ms IS NOT NULL
 AND NOT EXISTS (
    SELECT 1 FROM thread_items prior
    WHERE prior.thread_id = NEW.thread_id AND prior.item_id = NEW.item_id
      AND prior.turn_id != NEW.turn_id AND prior.completed_at_ms IS NOT NULL
      AND prior.compaction_actor_id = NEW.compaction_actor_id
 )
BEGIN
    UPDATE thread_history_projection_state
    SET completed_compactions = completed_compactions + 1
    WHERE thread_id = NEW.thread_id AND compaction_owner_id = NEW.compaction_actor_id
      AND NEW.updated_at_ordinal >= compaction_next_ordinal;
END;

CREATE TRIGGER thread_items_compaction_complete AFTER UPDATE ON thread_items
WHEN NEW.item_type = 'contextCompaction' AND NEW.completed_at_ms IS NOT NULL
 AND (OLD.item_type != 'contextCompaction' OR OLD.completed_at_ms IS NULL)
 AND NOT EXISTS (
    SELECT 1 FROM thread_items prior
    WHERE prior.thread_id = NEW.thread_id AND prior.item_id = NEW.item_id
      AND prior.turn_id != NEW.turn_id AND prior.completed_at_ms IS NOT NULL
      AND prior.compaction_actor_id = NEW.compaction_actor_id
 )
BEGIN
    UPDATE thread_history_projection_state
    SET completed_compactions = completed_compactions + 1
    WHERE thread_id = NEW.thread_id AND compaction_owner_id = NEW.compaction_actor_id
      AND NEW.updated_at_ordinal >= compaction_next_ordinal;
END;
