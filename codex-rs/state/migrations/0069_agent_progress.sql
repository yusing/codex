-- Progress is a third, lower-priority wake, never a claimed mailbox event.
CREATE TABLE agent_progress_batches (
    wait_id TEXT PRIMARY KEY REFERENCES agent_waits(wait_id),
    batch_id TEXT NOT NULL UNIQUE,
    context_id TEXT NOT NULL,
    frozen TEXT,
    output TEXT
);
CREATE TABLE agent_progress_slots (
    batch_id TEXT NOT NULL REFERENCES agent_progress_batches(batch_id) ON DELETE CASCADE,
    actor_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    started INTEGER NOT NULL DEFAULT 0 CHECK (started IN (0,1)),
    result TEXT,
    PRIMARY KEY (batch_id,actor_id,work_id)
);
CREATE INDEX agent_progress_pending_actor ON agent_progress_slots(actor_id,work_id) WHERE result IS NULL;
CREATE TABLE agent_capture_cursors (
    root_id TEXT NOT NULL,
    recipient_id TEXT NOT NULL,
    context_id TEXT NOT NULL,
    capture_key TEXT NOT NULL,
    through_chunk INTEGER NOT NULL CHECK (through_chunk >= 0),
    PRIMARY KEY (root_id,recipient_id,context_id,capture_key)
);
