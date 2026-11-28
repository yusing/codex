use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::FileChange;
use pretty_assertions::assert_eq;

use super::CodeModeEditProjector;
use super::ExecCommandArgumentDiffConsumer;
use super::JsonCmdExtractor;
use super::ShellEditProjector;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tools::handlers::live_diff::ArgumentPreview;
use crate::tools::registry::ToolArgumentDiffConsumer;

fn project_chunks(
    cwd: &str,
    chunks: &[&str],
) -> Vec<std::collections::HashMap<PathBuf, FileChange>> {
    let mut proj = ShellEditProjector::new(Path::new(cwd));
    let mut snapshots = Vec::new();
    for chunk in chunks {
        proj.push(chunk);
        if proj.take_dirty() {
            let snap = proj.snapshot();
            if !snap.is_empty() {
                snapshots.push(snap);
            }
        }
    }
    proj.finish();
    if proj.take_dirty() {
        snapshots.push(proj.snapshot());
    }
    snapshots
}

#[test]
fn streams_cat_heredoc_incrementally_before_eof() {
    let snaps = project_chunks(
        "/repo",
        &[
            "cat <<'EOF' > src/hello.rs\n",
            "fn main() {\n",
            "    println!(\"hi\");\n",
            "}\n",
            "EOF\n",
        ],
    );
    assert!(
        snaps.len() >= 3,
        "expected incremental snapshots, got {snaps:?}"
    );
    assert_eq!(
        snaps[0].get(Path::new("/repo/src/hello.rs")),
        Some(&FileChange::Add {
            content: "fn main() {\n".to_string(),
        })
    );
    let last = snaps.last().expect("final snapshot");
    assert_eq!(
        last.get(Path::new("/repo/src/hello.rs")),
        Some(&FileChange::Add {
            content: "fn main() {\n    println!(\"hi\");\n}\n".to_string(),
        })
    );
}

#[test]
fn streams_cat_heredoc_with_adjoining_redirects_and_overwrite_vs_append() {
    let snaps = project_chunks(
        "/repo",
        &[
            "cat >notes.txt<<'EOF'\ndraft\nEOF\n",
            "cat >notes.txt<<'EOF'\nfinal\nEOF\n",
            "cat >>notes.txt<<'EOF'\nmore\nEOF\n",
        ],
    );
    let last = snaps.last().expect("final snapshot");
    assert_eq!(
        last.get(Path::new("/repo/notes.txt")),
        Some(&FileChange::Add {
            content: "final\nmore\n".to_string(),
        })
    );
}

#[test]
fn streams_cat_heredoc_after_cd_and_multiple_files() {
    let snaps = project_chunks(
        "/repo",
        &[
            "cd pkg && cat <<'EOF' > a.txt\nfirst\nEOF\n",
            "tee b.txt <<'END'\nsecond\nEND\n",
        ],
    );
    let last = snaps.last().expect("final snapshot");
    assert_eq!(
        last.get(Path::new("/repo/pkg/a.txt")),
        Some(&FileChange::Add {
            content: "first\n".to_string(),
        })
    );
    assert_eq!(
        last.get(Path::new("/repo/pkg/b.txt")),
        Some(&FileChange::Add {
            content: "second\n".to_string(),
        })
    );
}

#[test]
fn trailing_workdir_rebases_already_streamed_relative_paths() {
    let mut ext = JsonCmdExtractor::default();
    let mut proj = ShellEditProjector::new(Path::new("/repo"));
    for chunk in [
        "{\"cmd\":\"cat <<'EOF' > out.txt\\nhello\\nEOF\\n\"",
        ",\"workdir\":\"sub\"}",
    ] {
        let (cmd, workdir) = ext.push(chunk);
        if let Some(dir) = workdir {
            proj.set_workdir(Path::new(&dir));
        }
        if !cmd.is_empty() {
            proj.push(&cmd);
        }
    }
    proj.finish();
    let snap = proj.snapshot();
    assert!(snap.get(Path::new("/repo/out.txt")).is_none());
    assert_eq!(
        snap.get(Path::new("/repo/sub/out.txt")),
        Some(&FileChange::Add {
            content: "hello\n".to_string(),
        })
    );
}

#[tokio::test]
async fn completed_shell_edit_waits_for_late_executor_before_reading_baseline() {
    let (_session, turn) = make_session_and_context().await;
    let step = StepContext::for_test(Arc::new(turn));
    let mut consumer = ExecCommandArgumentDiffConsumer::default();
    let provisional = consumer.consume_diff(
        &step,
        "edit".to_string(),
        r#"{"cmd":"cat <<'EOF' > /repo/out.txt\nhello\nEOF\n""#,
    );
    let path = Path::new("/repo/out.txt");
    let [ArgumentPreview::Patch(preview)] = provisional.as_slice() else {
        panic!("expected a provisional patch before the location arrived");
    };
    assert_eq!(preview.changes[path].source, None);

    let _ = consumer.consume_diff(&step, "edit".to_string(), r#","workdir":"/repo""#);
    assert_eq!(
        consumer.projector.as_ref().unwrap().snapshot_entries()[path].source,
        None
    );

    let mut resolved = consumer.consume_diff(
        &step,
        "edit".to_string(),
        r#","environment_id":"secondary"}"#,
    );
    resolved.extend(consumer.finish().expect("finish preview"));
    let Some(ArgumentPreview::Patch(preview)) = resolved.last() else {
        panic!("expected an updated patch for the selected executor");
    };
    assert_eq!(
        preview.changes[path].source,
        Some((Some("secondary".to_string()), path.to_path_buf()))
    );
}

#[test]
fn rejects_non_streaming_shell_copies_and_redirects() {
    for cmd in [
        "cat < large.txt > out.txt\n",
        "cat large.txt > out.txt\n",
        "cp large.txt out.txt\n",
        "mv a.txt b.txt\n",
        "echo hello > out.txt\n",
        "cat <<'EOF' > $DYNAMIC\nhi\nEOF\n",
    ] {
        let snaps = project_chunks("/repo", &[cmd]);
        assert!(
            snaps.is_empty(),
            "expected no live stream for {cmd:?}, got {snaps:?}"
        );
    }
}

#[test]
fn streams_python_heredoc_read_replace_write_text_with_range_headers() {
    let snaps = project_chunks(
        "/repo",
        &[
            "python3 - <<'PY'\n",
            "from pathlib import Path\n",
            "p = Path('app.py')\n",
            "text = p.read_text()\n",
            "text = text.replace(\n    'old_one\\nold_two',\n",
            "    'new_one\\nnew_two'\n)\n",
            "p.write_text(text)\n",
            "PY\n",
        ],
    );
    assert!(
        snaps.len() >= 2,
        "expected incremental snapshots, got {snaps:?}"
    );
    let first = snaps.first().expect("first snapshot");
    assert_eq!(
        first.get(Path::new("/repo/app.py")),
        Some(&FileChange::Update {
            unified_diff: "@@\n-old_one\n-old_two\n".to_string(),
            move_path: None,
        })
    );
    let last = snaps.last().expect("final snapshot");
    assert_eq!(
        last.get(Path::new("/repo/app.py")),
        Some(&FileChange::Update {
            unified_diff: "@@\n-old_one\n-old_two\n+new_one\n+new_two\n".to_string(),
            move_path: None,
        })
    );
}

#[test]
fn streams_python_open_read_replace_and_inline_minus_c() {
    let snaps = project_chunks(
        "/repo",
        &[
            "python3 -c \"with open('notes.txt', 'r') as f: s = f.read()\n",
            "s = s.replace('before', 'after')\n",
            "with open('notes.txt', 'w') as f: f.write(s)\"",
        ],
    );
    let last = snaps.last().expect("final snapshot");
    assert_eq!(
        last.get(Path::new("/repo/notes.txt")),
        Some(&FileChange::Update {
            unified_diff: "@@\n-before\n+after\n".to_string(),
            move_path: None,
        })
    );
}

#[test]
fn python_print_replace_does_not_emit_false_file_edit() {
    let snaps = project_chunks(
        "/repo",
        &[
            "python3 - <<'PY'\n",
            "from pathlib import Path\n",
            "text = Path('notes.txt').read_text()\n",
            "print(text.replace('old', 'new'))\n",
            "PY\n",
        ],
    );
    assert!(
        snaps.is_empty(),
        "expected no live stream for print(replace), got {snaps:?}"
    );

    // Even an assigned replace with no write retracts on finish.
    let mut proj = ShellEditProjector::new(Path::new("/repo"));
    proj.push(
        "python3 - <<'PY'\nfrom pathlib import Path\ntext = Path('notes.txt').read_text()\nout = text.replace('old', 'new')\nprint(out)\nPY\n",
    );
    proj.finish();
    assert!(
        proj.snapshot().is_empty(),
        "expected retracted empty final snapshot when no write occurs, got {:?}",
        proj.snapshot()
    );
}

#[test]
fn python_write_text_with_replace_targets_destination_path() {
    let snaps = project_chunks(
        "/repo",
        &[
            "python3 - <<'PY'\n",
            "from pathlib import Path\n",
            "text = Path('src.txt').read_text()\n",
            "Path('dst.txt').write_text(text.replace('a', 'b'))\n",
            "PY\n",
        ],
    );
    let last = snaps.last().expect("final snapshot");
    assert!(last.get(Path::new("/repo/src.txt")).is_none());
    assert_eq!(
        last.get(Path::new("/repo/dst.txt")),
        Some(&FileChange::Update {
            unified_diff: "@@\n-a\n+b\n".to_string(),
            move_path: None,
        })
    );
}

#[test]
fn python_cross_file_replace_disables_context_for_existing_and_new_destinations() {
    let mut projector = ShellEditProjector::new(Path::new("/repo"));
    projector.set_workdir(Path::new("/repo"));
    projector.resolve_location();
    projector.push(
        "python3 - <<'PY'\nfrom pathlib import Path\n\
         dst = Path('dst')\ndst.write_text(dst.read_text().replace('a', 'b'))\n",
    );
    let dst = Path::new("/repo/dst");
    assert!(projector.snapshot_entries()[dst].source.is_some());
    projector.push(
        "source = Path('src').read_text()\n\
         dst.write_text(source.replace('a', 'c'))\n\
         Path('new').write_text(source.replace('a', 'd'))\nPY\n",
    );
    let edits = projector.snapshot_entries();
    assert!(edits[dst].source.is_none());
    assert!(edits[Path::new("/repo/new")].source.is_none());
}

#[test]
fn streams_python_literal_write_text_and_decodes_unicode_hex_escapes() {
    let snaps = project_chunks(
        "/repo",
        &[
            "python3 - <<'PY'\n",
            "from pathlib import Path\n",
            "Path('created.txt').write_text('''caf\\u00e9\\x21\n",
            "line2\n''')\n",
            "PY\n",
        ],
    );
    assert!(
        snaps.len() >= 2,
        "expected streaming write_text snapshots, got {snaps:?}"
    );
    let last = snaps.last().expect("final snapshot");
    assert_eq!(
        last.get(Path::new("/repo/created.txt")),
        Some(&FileChange::Add {
            content: "café!\nline2\n".to_string(),
        })
    );
}

#[test]
fn rejects_non_streaming_python_copies() {
    for cmd in [
        "python3 -c \"import shutil; shutil.copy('a.txt', 'b.txt')\"",
        "python3 -c \"import shutil; shutil.copy2('a.txt', 'b.txt')\"",
        "python3 -c \"from pathlib import Path; Path('b.txt').write_text(Path('a.txt').read_text())\"",
        "python3 - <<'PY'\nimport shutil\nshutil.copyfile('a', 'b')\nPY\n",
    ] {
        let snaps = project_chunks("/repo", &[cmd]);
        assert!(
            snaps.is_empty(),
            "expected no live stream for {cmd:?}, got {snaps:?}"
        );
    }
}

#[test]
fn streams_code_mode_nested_exec_command_and_apply_patch_before_finish() {
    let mut cm = CodeModeEditProjector::default();
    let cwd = Path::new("/repo");
    let mut emitted = Vec::new();
    for chunk in [
        "await tools.exec_command({ cmd: \"cat > notes.txt <<'EOF'\\n",
        "hello from code mode\\n",
        "EOF\" });\n",
        "await tools.apply_patch(`*** Begin Patch\n*** Add File: extra.txt\n+from patch\n*** End Patch`);\n",
    ] {
        if let Some(ev) = cm
            .consume_diff(cwd, "call_cm".to_string(), chunk)
            .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
        {
            emitted.push(ev);
        }
    }
    assert!(
        !emitted.is_empty(),
        "expected PatchApplyUpdated before finish() for nested Code Mode edits"
    );
    if let Some(ev) = cm
        .finish()
        .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
    {
        emitted.push(ev);
    }
    let Some(EventMsg::PatchApplyUpdated(final_ev)) = emitted.last() else {
        panic!("expected PatchApplyUpdated, got {emitted:?}");
    };
    assert_eq!(final_ev.call_id, "call_cm");
    assert_eq!(
        final_ev.changes.get(Path::new("/repo/notes.txt")),
        Some(&FileChange::Add {
            content: "hello from code mode\n".to_string(),
        })
    );
    assert_eq!(
        final_ev.changes.get(Path::new("/repo/extra.txt")),
        Some(&FileChange::Add {
            content: "from patch\n".to_string(),
        })
    );
}

#[test]
fn long_heredoc_line_streams_in_linear_time() {
    let long_line = "x".repeat(64 * 1024);
    let mut proj = ShellEditProjector::new(Path::new("/repo"));
    proj.push("cat <<'EOF' > big.txt\n");
    proj.push(&long_line);
    assert!(proj.take_dirty());
    let snap = proj.snapshot();
    assert_eq!(
        snap.get(Path::new("/repo/big.txt")),
        Some(&FileChange::Add { content: long_line })
    );
}

#[test]
fn json_cmd_extractor_streams_across_chunk_boundaries() {
    let mut ext = JsonCmdExtractor::default();
    let mut cmd = String::new();
    let mut workdir = None;
    for chunk in [
        "{\"work",
        "dir\":\"/repo/sub\",\"cm",
        "d\":\"cat <<'EOF' > f.txt\\",
        "nhi \\\"there\\\" \\u0041\\nEOF\",\"tty\":false}",
    ] {
        let (c, w) = ext.push(chunk);
        cmd.push_str(&c);
        if w.is_some() {
            workdir = w;
        }
    }
    assert_eq!(workdir.as_deref(), Some("/repo/sub"));
    assert_eq!(cmd, "cat <<'EOF' > f.txt\nhi \"there\" A\nEOF");
}

#[test]
fn code_mode_keeps_baseline_provisional_until_the_nested_call_resolves() {
    let mut projector = CodeModeEditProjector::default();
    let cwd = Path::new("/repo");
    let path = Path::new("/repo/out.txt");
    let call = "edit".to_string();
    let provisional = projector
        .consume_diff(
            cwd,
            call.clone(),
            r#"await tools.exec_command({ cmd: "cat > /repo/out.txt <<'EOF'\nhello\nEOF\n", workdir: "/repo""#,
        )
        .expect("editable preview before the call closes");
    assert_eq!(provisional.changes[path].source, None);

    let resolved = projector
        .consume_diff(cwd, call, r#", environment_id: "secondary" });"#)
        .or_else(|| projector.finish())
        .expect("editable preview for the selected executor");
    assert_eq!(
        resolved.changes[path].source,
        Some((Some("secondary".to_string()), path.to_path_buf()))
    );
}

#[test]
fn code_mode_isolates_workdir_per_invocation() {
    let mut cm = CodeModeEditProjector::default();
    let cwd = Path::new("/repo");
    let mut last = None;
    for chunk in [
        "await tools.exec_command({ cmd: \"cat > one.txt <<'EOF'\\nfirst\\nEOF\", workdir: 'docs' });\n",
        "await tools.exec_command({ cmd: \"cat > two.txt <<'EOF'\\nsecond\\nEOF\" });\n",
    ] {
        if let Some(EventMsg::PatchApplyUpdated(ev)) = cm
            .consume_diff(cwd, "call_wd".to_string(), chunk)
            .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
        {
            last = Some(ev.changes);
        }
    }
    if let Some(EventMsg::PatchApplyUpdated(ev)) = cm
        .finish()
        .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
    {
        last = Some(ev.changes);
    }
    let changes = last.expect("expected snapshot");
    assert_eq!(
        changes.get(Path::new("/repo/docs/one.txt")),
        Some(&FileChange::Add {
            content: "first\n".to_string(),
        })
    );
    assert_eq!(
        changes.get(Path::new("/repo/two.txt")),
        Some(&FileChange::Add {
            content: "second\n".to_string(),
        })
    );
    assert!(!changes.contains_key(Path::new("/repo/docs/two.txt")));
}

#[test]
fn code_mode_merges_shell_and_patch_in_source_order() {
    let mut cm = CodeModeEditProjector::default();
    let cwd = Path::new("/repo");
    let mut last = None;
    for chunk in [
        "await tools.exec_command({ cmd: \"cat > notes.txt <<'EOF'\\nhello\\nEOF\" });\n",
        "await tools.apply_patch(`*** Begin Patch\n*** Delete File: notes.txt\n*** End Patch`);\n",
    ] {
        if let Some(EventMsg::PatchApplyUpdated(ev)) = cm
            .consume_diff(cwd, "call_order".to_string(), chunk)
            .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
        {
            last = Some(ev.changes);
        }
    }
    if let Some(EventMsg::PatchApplyUpdated(ev)) = cm
        .finish()
        .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
    {
        last = Some(ev.changes);
    }
    let changes = last.expect("expected snapshot");
    assert!(
        matches!(
            changes.get(Path::new("/repo/notes.txt")),
            Some(FileChange::Delete { .. })
        ),
        "later apply_patch should overwrite earlier shell preview, got {changes:?}"
    );
}

#[test]
fn code_mode_ignores_quotes_inside_js_comments() {
    let mut cm = CodeModeEditProjector::default();
    let cwd = Path::new("/repo");
    let mut last = None;
    for chunk in [
        "// Don't edit other files\n",
        "/* block comment with 'quote' *",
        "/ await tools.exec_command({ cmd: \"cat > notes.txt <<'EOF'\\nhi\\nEOF\" });\n",
    ] {
        if let Some(EventMsg::PatchApplyUpdated(ev)) = cm
            .consume_diff(cwd, "call_comment".to_string(), chunk)
            .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
        {
            last = Some(ev.changes);
        }
    }
    if let Some(EventMsg::PatchApplyUpdated(ev)) = cm
        .finish()
        .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
    {
        last = Some(ev.changes);
    }
    let changes = last.expect("expected snapshot");
    assert_eq!(
        changes.get(Path::new("/repo/notes.txt")),
        Some(&FileChange::Add {
            content: "hi\n".to_string(),
        })
    );
}

#[test]
fn empty_overwrite_heredoc_replaces_earlier_preview_with_zero_bytes() {
    let mut proj = ShellEditProjector::new(Path::new("/repo"));
    proj.push("cat > notes.txt <<'EOF'\ndraft\nEOF\n");
    assert!(proj.take_dirty());
    assert_eq!(
        proj.snapshot().get(Path::new("/repo/notes.txt")),
        Some(&FileChange::Add {
            content: "draft\n".to_string(),
        })
    );
    proj.push("cat > notes.txt <<'EOF'\nEOF\n");
    assert!(
        proj.take_dirty(),
        "empty overwrite must mark projector dirty"
    );
    assert_eq!(
        proj.snapshot().get(Path::new("/repo/notes.txt")),
        Some(&FileChange::Add {
            content: String::new(),
        })
    );
}

#[test]
fn code_mode_suppresses_interpolated_template_and_preserves_escaped_dollar() {
    let mut cm = CodeModeEditProjector::default();
    let cwd = Path::new("/repo");
    let mut emitted = Vec::new();
    for chunk in [
        "const title = 'Notes';\n",
        "await tools.exec_command({ cmd: `cat > interp.txt <<'EOF'\n",
        "header\n",
        "${title}\nEOF` });\n",
    ] {
        if let Some(EventMsg::PatchApplyUpdated(ev)) = cm
            .consume_diff(cwd, "call_interp".to_string(), chunk)
            .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
        {
            emitted.push(ev.changes);
        }
    }
    if let Some(EventMsg::PatchApplyUpdated(ev)) = cm
        .finish()
        .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
    {
        emitted.push(ev.changes);
    }
    let final_changes = emitted.last().cloned().unwrap_or_default();
    assert!(
        final_changes.is_empty(),
        "interpolated template should withdraw preview, got {final_changes:?}"
    );

    let mut cm_escaped = CodeModeEditProjector::default();
    let mut escaped_last = None;
    let script = "await tools.exec_command({ cmd: `cat > lit.txt <<'EOF'\n\\${title}\nEOF` });\n";
    if let Some(EventMsg::PatchApplyUpdated(ev)) = cm_escaped
        .consume_diff(cwd, "call_esc".to_string(), script)
        .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
    {
        escaped_last = Some(ev.changes);
    }
    if let Some(EventMsg::PatchApplyUpdated(ev)) = cm_escaped
        .finish()
        .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
    {
        escaped_last = Some(ev.changes);
    }
    assert_eq!(
        escaped_last
            .expect("expected escaped snapshot")
            .get(Path::new("/repo/lit.txt")),
        Some(&FileChange::Add {
            content: "${title}\n".to_string(),
        })
    );
}

#[test]
fn code_mode_composes_appends_across_shell_calls_and_replaces_on_overwrite() {
    let mut cm = CodeModeEditProjector::default();
    let cwd = Path::new("/repo");
    let mut last = None;
    for chunk in [
        "await tools.exec_command({ cmd: \"cat > notes.txt <<'EOF'\\nfirst\\nEOF\" });\n",
        "await tools.exec_command({ cmd: \"cat >> notes.txt <<'EOF'\\nsecond\\nEOF\" });\n",
    ] {
        if let Some(EventMsg::PatchApplyUpdated(ev)) = cm
            .consume_diff(cwd, "call_append".to_string(), chunk)
            .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
        {
            last = Some(ev.changes);
        }
    }
    if let Some(EventMsg::PatchApplyUpdated(ev)) = cm
        .finish()
        .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
    {
        last = Some(ev.changes);
    }
    assert_eq!(
        last.expect("expected append snapshot")
            .get(Path::new("/repo/notes.txt")),
        Some(&FileChange::Add {
            content: "first\nsecond\n".to_string(),
        })
    );

    let mut cm_overwrite = CodeModeEditProjector::default();
    let mut ow_last = None;
    for chunk in [
        "await tools.exec_command({ cmd: \"cat > notes.txt <<'EOF'\\nfirst\\nEOF\" });\n",
        "await tools.exec_command({ cmd: \"cat > notes.txt <<'EOF'\\nsecond\\nEOF\" });\n",
    ] {
        if let Some(EventMsg::PatchApplyUpdated(ev)) = cm_overwrite
            .consume_diff(cwd, "call_ow".to_string(), chunk)
            .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
        {
            ow_last = Some(ev.changes);
        }
    }
    if let Some(EventMsg::PatchApplyUpdated(ev)) = cm_overwrite
        .finish()
        .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
    {
        ow_last = Some(ev.changes);
    }
    assert_eq!(
        ow_last
            .expect("expected overwrite snapshot")
            .get(Path::new("/repo/notes.txt")),
        Some(&FileChange::Add {
            content: "second\n".to_string(),
        })
    );
}

#[test]
fn code_mode_handles_regex_literals_containing_slash_star() {
    let mut cm = CodeModeEditProjector::default();
    let cwd = Path::new("/repo");
    let mut last = None;
    for chunk in [
        "const dir = path.replace(/\\/*$/, '');\n",
        "const re = /[/*]+/g;\n",
        "await tools.exec_command({ cmd: \"cat > out.txt <<'EOF'\\nok\\nEOF\" });\n",
    ] {
        if let Some(EventMsg::PatchApplyUpdated(ev)) = cm
            .consume_diff(cwd, "call_regex".to_string(), chunk)
            .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
        {
            last = Some(ev.changes);
        }
    }
    if let Some(EventMsg::PatchApplyUpdated(ev)) = cm
        .finish()
        .map(|e| EventMsg::PatchApplyUpdated(e.provisional()))
    {
        last = Some(ev.changes);
    }
    assert_eq!(
        last.expect("expected regex snapshot")
            .get(Path::new("/repo/out.txt")),
        Some(&FileChange::Add {
            content: "ok\n".to_string(),
        })
    );
}
