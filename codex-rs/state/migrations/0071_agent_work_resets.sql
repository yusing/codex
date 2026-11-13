-- Accepted reload selection belongs to the actor, not a wait or SDK turn.

CREATE TABLE agent_reset_selection (
    root_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    selection TEXT NOT NULL,
    PRIMARY KEY(root_id,actor_id)
);

-- A reset checkpoints existing work without inventing selected descendants.
-- The actual dispatch fence and outcome survive callback loss; cold load is
-- observation-only and never reconstructs execution from these phases.
CREATE TABLE agent_work_resets (
    reset_id TEXT PRIMARY KEY,
    root_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    runtime_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    tool_use_id TEXT NOT NULL,
    requested_reload TEXT,
    reload_selection TEXT NOT NULL,
    phase TEXT NOT NULL CHECK(phase IN ('registered','checkpointed','dispatched','settled','published','cancelled','coalesced')),
    outcome TEXT,
    reload_text TEXT,
    continuation_id TEXT UNIQUE REFERENCES agent_deliveries(delivery_id),
    coalesced_wait_id TEXT UNIQUE REFERENCES agent_waits(wait_id),
    UNIQUE(root_id,actor_id,work_id,turn_id,tool_use_id),
    FOREIGN KEY(root_id,actor_id,work_id) REFERENCES agent_work(root_id,actor_id,work_id)
);
CREATE UNIQUE INDEX agent_work_resets_active ON agent_work_resets(root_id,actor_id,work_id)
    WHERE phase NOT IN ('published','cancelled','coalesced');
