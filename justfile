# Native Mekugi runtime maintenance. SDKs, upstream clients, Bazel packaging,
# and upstream release automation are intentionally outside this fork's scope.
set working-directory := "codex-rs"
set positional-arguments
export CODEX_REPO_ROOT := justfile_directory()

help:
    just -l

build *args:
    cargo build --locked {{ args }}

code-mode-host *args:
    cargo run --bin codex-code-mode-host -- {{ args }}

fmt:
    python3 ../scripts/format.py

fmt-check:
    python3 ../scripts/format.py --check

fix *args:
    cargo clippy --fix --tests --allow-dirty {{ args }}

clippy *args:
    cargo clippy --tests {{ args }}

test *args:
    RUST_MIN_STACK=8388608 NEXTEST_PROFILE=local cargo nextest run --no-fail-fast {{ args }}

write-config-schema:
    cargo run -p codex-config-schema --bin codex-write-config-schema

# Regenerate only the native protocol artifacts, without either SDK.
write-app-server-schema *args:
    python3 app-server-protocol/scripts/write_schema_fixtures.py {{ args }}

write-hooks-schema:
    cargo run -p codex-hooks --bin write_hooks_schema_fixtures
