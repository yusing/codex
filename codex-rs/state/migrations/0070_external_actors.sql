-- External execution bindings share native graph identity without a Codex rollout.
CREATE TABLE external_actors (
    actor_id TEXT PRIMARY KEY NOT NULL,
    root_id TEXT NOT NULL,
    parent_id TEXT NOT NULL,
    creation_id TEXT NOT NULL,
    source_json TEXT NOT NULL,
    settings_json TEXT NOT NULL,
    fork_source_session_id TEXT,
    backend_session_id TEXT,
    receipt_reference TEXT,
    UNIQUE(root_id, creation_id)
);
CREATE INDEX external_actors_root ON external_actors(root_id);
