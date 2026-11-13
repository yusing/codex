-- Coordination is not conversation history. Logical actor UUIDs need neither a
-- threads row nor a Codex rollout (external executors use the same identities).
CREATE TABLE agent_work (
    root_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    runtime_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    disposition TEXT NOT NULL CHECK (disposition IN
        ('pending_start', 'running', 'waiting', 'paused', 'uncertain',
         'completed', 'failed', 'interrupted')),
    terminal_delivery_id TEXT,
    capacity_retry_seconds INTEGER NOT NULL DEFAULT 0
        CHECK (capacity_retry_seconds BETWEEN 0 AND 30),
    PRIMARY KEY (root_id, actor_id, work_id),
    CHECK ((disposition IN ('completed', 'failed', 'interrupted')) =
           (terminal_delivery_id IS NOT NULL))
);

-- This is a current-work index, not another membership/topology authority.
CREATE TABLE agent_current_work (
    root_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    PRIMARY KEY (root_id, actor_id),
    FOREIGN KEY (root_id, actor_id, work_id)
        REFERENCES agent_work(root_id, actor_id, work_id)
);

CREATE TABLE agent_deliveries (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    delivery_id TEXT NOT NULL UNIQUE,
    root_id TEXT NOT NULL,
    sender_id TEXT,
    recipient_id TEXT NOT NULL,
    work_actor_id TEXT,
    work_id TEXT,
    kind TEXT NOT NULL CHECK (kind IN ('message', 'terminal')),
    payload TEXT NOT NULL,
    claim_id TEXT,
    dispatch_id TEXT,
    accepted_receipt TEXT,
    refused_reason TEXT,
    consumed INTEGER NOT NULL DEFAULT 0 CHECK (consumed IN (0, 1)),
    CHECK ((work_actor_id IS NULL) = (work_id IS NULL)),
    CHECK (kind != 'terminal' OR work_id IS NOT NULL),
    CHECK (accepted_receipt IS NULL OR dispatch_id IS NOT NULL),
    CHECK (consumed = 0 OR accepted_receipt IS NOT NULL OR (claim_id IS NOT NULL AND dispatch_id IS NULL))
);
CREATE INDEX agent_deliveries_unread
    ON agent_deliveries(root_id, recipient_id, sequence) WHERE consumed = 0;
CREATE INDEX agent_deliveries_work
    ON agent_deliveries(root_id, work_actor_id, work_id, sequence);
CREATE INDEX agent_deliveries_dispatch
    ON agent_deliveries(dispatch_id) WHERE dispatch_id IS NOT NULL;
