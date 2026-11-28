//! Descriptions for Mekugi commands reachable through the native shell tool.

use std::ffi::OsStr;
use std::fmt::Write;
use std::path::Path;

const HELPERS: &[(&str, &str)] = &[
    (
        "mfinspect",
        "Outline source structure as START-END KIND NAME. Use `mfinspect \
         [--json] PATH ...` before reading specific ranges. Output and errors \
         are per file.",
    ),
    (
        "mcat",
        "Read UTF-8 files or inclusive line ranges. Use `mcat [--number] \
         PATH [START:END ...] ...`, e.g. `mcat src/main.rs 100:150`. Batch up \
         to 16 reads. `-n N` limits rows in a single-file read; `--tail` \
         selects the last rows and requires `-n`.",
    ),
    (
        "msymbol",
        "Resolve definitions and references with `msymbol [--workspace ROOT] \
         (def|refs) PATH [LINE] SYMBOL [N] ...`. Supply LINE for locals or \
         ambiguous names; N chooses a token occurrence. Batch independent \
         lookups. Rust analysis is offline and can report incomplete coverage.",
    ),
    (
        "mrun",
        "Keep a bounded window from one foreground command: `mrun (-n N|\
         --max-tokens N) [--head|--tail] [--] COMMAND [ARG...]`. The default \
         is the tail. Data outside the window is discarded, not recoverable. \
         Use native waits for background or interactive commands.",
    ),
    (
        "mchanges",
        "Review captured edits, not Git or current workspace state. Use `mchanges \
         --list` to find your IDs or `mchanges ID[..ID] ... [PATH ...]` for a \
         composed diff. Handoffs need explicit IDs or same-agent ranges and scope. \
         `--summary` shows file statuses; `--history` shows individual attempts. \
         `mchanges revert|apply ID ...` preserves unrelated edits and reports \
         conflicts.",
    ),
];

pub(crate) const CHILD_REPLY_SUMMARY: &str = "For substantial messages and final replies to your parent, start the same reply \
     with `## Summary`: give the outcome or blocker and material uncertainty in one \
     to three sentences, then use headings for supporting detail. Keep short replies \
     plain and honor higher-priority exact-output requirements.";

pub(crate) const SOURCE_REFERENCE_SYNTAX: &str = "Source references (used in `new_context` `reload.sources`, `spawn_agent` `preload.sources`, \
     and messages) accept `@path`, `@path#Lstart-Lend,Lstart2-Lend2` (inclusive line ranges; quote \
     paths with spaces as `@\"path with spaces\"#L1-L20`), `$skill` or `$skill/references/file.md`, \
     and `mchanges:ID` or `mchanges:ID..ID`. Bare paths without `@` are invalid.";

pub(crate) fn shell_guidance(path: &OsStr) -> String {
    let directories: Vec<_> = std::env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .collect();
    let mut helpers = HELPERS
        .iter()
        .filter(|(name, _)| {
            directories
                .iter()
                .any(|directory| executable(&directory.join(name)))
        })
        .peekable();
    if helpers.peek().is_none() {
        return String::new();
    }

    let mut description = String::from("\n\n### Session shell helpers\n\n");
    description.push_str(include_str!("shell_helpers.md").trim_end());
    if cfg!(target_os = "macos") {
        description.push_str(
            "\n- On macOS, set `shell` to `bash` unless the task requires another shell.",
        );
    }
    for (name, help) in helpers {
        write!(description, "\n\n#### `{name}`\n\n{help}")
            .expect("writing to a String cannot fail");
    }
    write!(
        description,
        "\n\n### Source references\n\n{SOURCE_REFERENCE_SYNTAX}"
    )
    .expect("writing to a String cannot fail");
    description
}

fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

#[cfg(all(test, unix))]
#[path = "guidance_tests.rs"]
mod tests;
