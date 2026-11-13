CREATE TABLE agent_waits (
    wait_id TEXT PRIMARY KEY,
    root_id TEXT NOT NULL,
    caller_id TEXT NOT NULL,
    caller_work_id TEXT NOT NULL,
    runtime_id TEXT NOT NULL,
    origin TEXT NOT NULL DEFAULT 'cell' CHECK (origin IN ('cell', 'turn', 'external_tool')),
    turn_id TEXT NOT NULL,
    turn_reason TEXT CHECK (turn_reason IN ('descendant_recovery','provider_capacity','provider_usage_limit')),
    item_id TEXT,
    window_id TEXT,
    cell_id TEXT,
    mode TEXT NOT NULL CHECK (mode IN ('any', 'all')),
    phase TEXT NOT NULL CHECK (phase IN
        ('registered', 'claimed', 'dispatched', 'accepted', 'consumed', 'cancelled')),
    claim_id TEXT UNIQUE,
    claim_wake TEXT CHECK (claim_wake IN ('events', 'deadline', 'progress')),
    claim_metrics TEXT,
    dispatch_id TEXT UNIQUE,
    accepted_receipt TEXT,
    timeout_requested_ms INTEGER,
    timeout_effective_ms INTEGER,
    deadline_unix_ms INTEGER,
    progress_interval_ms INTEGER,
    compact INTEGER NOT NULL DEFAULT 0 CHECK (compact IN (0, 1)),
    reload_selection TEXT,
    reset_state TEXT NOT NULL DEFAULT 'not_requested'
        CHECK (reset_state IN ('not_requested', 'pending', 'running', 'completed', 'failed')),
    reset_diagnostic TEXT,
    reload_text TEXT,
    cancellation TEXT,
    FOREIGN KEY (root_id, caller_id, caller_work_id)
        REFERENCES agent_work(root_id, actor_id, work_id),
    CHECK ((origin='cell' AND item_id IS NOT NULL AND window_id IS NOT NULL AND cell_id IS NOT NULL AND turn_reason IS NULL)
        OR (origin='external_tool' AND item_id IS NOT NULL AND window_id IS NULL AND cell_id IS NULL AND turn_reason IS NULL)
        OR (origin='turn' AND item_id IS NULL AND window_id IS NULL AND cell_id IS NULL AND turn_reason IS NOT NULL)),
    CHECK (origin IN ('cell','external_tool') OR (compact=0 AND reload_selection IS NULL AND progress_interval_ms IS NULL
        AND timeout_requested_ms IS NULL AND timeout_effective_ms IS NULL AND reset_state='not_requested')),
    CHECK (accepted_receipt IS NULL OR dispatch_id IS NOT NULL),
    CHECK ((claim_id IS NULL) = (claim_wake IS NULL))
);
CREATE UNIQUE INDEX agent_waits_active_caller ON agent_waits(root_id, caller_id)
    WHERE phase NOT IN ('consumed', 'cancelled');
CREATE UNIQUE INDEX agent_waits_cell_identity
    ON agent_waits(root_id,caller_id,caller_work_id,turn_id,item_id,window_id,cell_id) WHERE origin='cell';
CREATE UNIQUE INDEX agent_waits_turn_identity
    ON agent_waits(root_id,caller_id,caller_work_id,turn_id,turn_reason)
    WHERE origin='turn' AND phase NOT IN ('consumed', 'cancelled');
CREATE UNIQUE INDEX agent_waits_external_identity
    ON agent_waits(root_id,caller_id,caller_work_id,turn_id,item_id) WHERE origin='external_tool';
CREATE INDEX agent_waits_consumed_observer ON agent_waits(root_id,caller_id,wait_id) WHERE phase='consumed';

CREATE TABLE agent_wait_selection (
    wait_id TEXT NOT NULL REFERENCES agent_waits(wait_id),
    ordinal INTEGER NOT NULL,
    root_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    claimed INTEGER NOT NULL DEFAULT 0 CHECK (claimed IN (0, 1)),
    claimed_disposition TEXT CHECK (claimed_disposition IN ('completed','failed','interrupted','paused')),
    claimed_ordinal INTEGER,
    PRIMARY KEY (wait_id, actor_id, work_id),
    FOREIGN KEY (root_id, actor_id, work_id)
        REFERENCES agent_work(root_id, actor_id, work_id)
);
CREATE INDEX agent_wait_selection_work
    ON agent_wait_selection(root_id, actor_id, work_id, wait_id);

CREATE TABLE agent_wait_deliveries (
    wait_id TEXT NOT NULL REFERENCES agent_waits(wait_id),
    delivery_id TEXT NOT NULL REFERENCES agent_deliveries(delivery_id),
    PRIMARY KEY (wait_id, delivery_id)
);

CREATE INDEX agent_work_wait_candidates ON agent_work(root_id, actor_id, disposition);
CREATE INDEX agent_deliveries_wait_messages ON agent_deliveries(root_id, recipient_id, sequence)
    WHERE kind='message' AND consumed=0 AND claim_id IS NULL AND dispatch_id IS NULL;
