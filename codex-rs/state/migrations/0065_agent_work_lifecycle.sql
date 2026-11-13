-- Ordered ordinary answers survive native checkpoints and conversation resets.
CREATE INDEX agent_deliveries_native_pending ON agent_deliveries(root_id,recipient_id,sequence)
    WHERE consumed=0 AND claim_id IS NULL AND dispatch_id IS NULL
      AND (kind='message' OR work_actor_id<>recipient_id);
CREATE TABLE agent_work_turns (
    root_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    PRIMARY KEY(root_id,actor_id,turn_id),
    FOREIGN KEY(root_id,actor_id,work_id) REFERENCES agent_work(root_id,actor_id,work_id)
);
CREATE TABLE agent_work_answers (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    root_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    content TEXT NOT NULL,
    UNIQUE(root_id, actor_id, work_id, item_id),
    FOREIGN KEY(root_id, actor_id, work_id) REFERENCES agent_work(root_id, actor_id, work_id)
);
CREATE INDEX agent_work_answers_order ON agent_work_answers(root_id,actor_id,work_id,sequence);

CREATE TABLE agent_work_completion (
    root_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    delivery_id TEXT NOT NULL UNIQUE,
    recipient_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK(disposition IN ('completed','failed','interrupted')),
    detail TEXT,
    capture_ready INTEGER NOT NULL DEFAULT 0,
    capture TEXT,
    execution_settled INTEGER NOT NULL DEFAULT 1 CHECK (execution_settled IN (0, 1)),
    PRIMARY KEY(root_id,actor_id,work_id),
    FOREIGN KEY(root_id,actor_id,work_id) REFERENCES agent_work(root_id,actor_id,work_id)
);
CREATE TABLE agent_completion_blockers (
    root_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    blocker_actor_id TEXT NOT NULL,
    blocker_work_id TEXT NOT NULL,
    PRIMARY KEY(root_id,actor_id,work_id,blocker_actor_id,blocker_work_id),
    FOREIGN KEY(root_id,actor_id,work_id) REFERENCES agent_work_completion(root_id,actor_id,work_id)
        ON DELETE CASCADE
);
CREATE INDEX agent_completion_affected ON agent_completion_blockers(root_id,blocker_actor_id,blocker_work_id);
