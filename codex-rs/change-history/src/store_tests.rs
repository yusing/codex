use std::fs;
use std::path::Path;
use std::str::FromStr;

use pretty_assertions::assert_eq;
use tempfile::TempDir;

use crate::ChangeHistoryError;
use crate::ChangeHistoryStore;
use crate::ChangeId;
use crate::ChangeMetadata;
use crate::ChangeSourceKind;
use crate::CoverageStatus;
use crate::FileSnapshot;
use crate::FileState;
use crate::MutationDirection;
use crate::PendingFileChange;
use crate::compose_diff;
use crate::compose_summary;
use crate::expand_id_spec;
use crate::stream_name_from_index;

fn test_metadata(
    workspace: &Path,
    thread_id: &str,
    call_id: &str,
    source_kind: ChangeSourceKind,
) -> ChangeMetadata {
    ChangeMetadata {
        thread_id: thread_id.to_string(),
        session_id: "sess-1".to_string(),
        turn_id: "turn-1".to_string(),
        call_id: call_id.to_string(),
        author: None,
        source_kind,
        workspace_root: workspace.to_path_buf(),
        created_at: 1_700_000_000,
        exit_code: Some(0),
        summary: None,
        claim_key: None,
    }
}

#[test]
fn identity_parsing_and_range_expansion() {
    assert_eq!(stream_name_from_index(0), "a");
    assert_eq!(stream_name_from_index(1), "b");
    assert_eq!(stream_name_from_index(25), "z");
    assert_eq!(stream_name_from_index(26), "aa");

    let id = ChangeId::new("a", 1).expect("valid id");
    assert_eq!(id.as_str(), "amber1");
    assert_eq!(ChangeId::from_str("amber1").expect("parse"), id);

    let range = expand_id_spec("amber1..3").expect("abbreviated range");
    assert_eq!(
        range.iter().map(ChangeId::as_str).collect::<Vec<_>>(),
        vec!["amber1", "amber2", "amber3"]
    );

    let full_range = expand_id_spec("apple2..apple4").expect("full range");
    assert_eq!(
        full_range.iter().map(ChangeId::as_str).collect::<Vec<_>>(),
        vec!["apple2", "apple3", "apple4"]
    );

    assert!(expand_id_spec("amber1..apple2").is_err());
    assert!(expand_id_spec("amber3..1").is_err());
    assert!(ChangeId::from_str("amber0").is_err());
    assert!(ChangeId::from_str("unknown1").is_err());
}

#[test]
fn monotonic_ids_per_thread_and_claim_deduplication() {
    let dir = TempDir::new().expect("tempdir");
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).expect("ws");
    let store = ChangeHistoryStore::open(dir.path().join("store")).expect("open");

    let r1 = store
        .record_change(
            test_metadata(&ws, "thread-a", "call-1", ChangeSourceKind::ApplyPatch),
            vec![PendingFileChange {
                rel_path: "a.txt".to_string(),
                abs_path: ws.join("a.txt"),
                moved_from: None,
                before: FileSnapshot::Missing,
                after: FileSnapshot::Present(b"hello\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("record r1");
    assert_eq!(r1.id.as_str(), "amber1");

    // Duplicate recording for the same (thread, call_id) must not double-count.
    let dup = store
        .record_change(
            test_metadata(&ws, "thread-a", "call-1", ChangeSourceKind::ShellWrite),
            vec![PendingFileChange {
                rel_path: "a.txt".to_string(),
                abs_path: ws.join("a.txt"),
                moved_from: None,
                before: FileSnapshot::Missing,
                after: FileSnapshot::Present(b"hello\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("dedup record");
    assert_eq!(dup.id, r1.id);
    assert_eq!(store.list_entries().expect("list").len(), 1);

    // Second thread gets its own stream (`apple1`).
    let r2 = store
        .record_change(
            test_metadata(&ws, "thread-b", "call-2", ChangeSourceKind::ApplyPatch),
            vec![PendingFileChange {
                rel_path: "b.txt".to_string(),
                abs_path: ws.join("b.txt"),
                moved_from: None,
                before: FileSnapshot::Missing,
                after: FileSnapshot::Present(b"world\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("record r2");
    assert_eq!(r2.id.as_str(), "apple1");
}

#[test]
fn missing_or_corrupt_blob_fails_explicitly() {
    let dir = TempDir::new().expect("tempdir");
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).expect("ws");
    let store = ChangeHistoryStore::open(dir.path().join("store")).expect("open");

    let rec = store
        .record_change(
            test_metadata(&ws, "thread-a", "call-1", ChangeSourceKind::ApplyPatch),
            vec![PendingFileChange {
                rel_path: "a.txt".to_string(),
                abs_path: ws.join("a.txt"),
                moved_from: None,
                before: FileSnapshot::Missing,
                after: FileSnapshot::Present(b"original\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("record");

    let blob_hash = match &rec.files[0].after {
        FileState::Present { blob_hash, .. } => blob_hash.clone(),
        FileState::Missing => panic!("expected present state"),
    };
    let blob_path = store.root().join("blobs").join(&blob_hash);

    // Corrupt the blob bytes.
    fs::write(&blob_path, b"tampered\n").expect("corrupt");
    let err = compose_diff(&store, std::slice::from_ref(&rec), &[]).expect_err("must fail");
    assert!(matches!(err, ChangeHistoryError::CorruptEvidence { .. }));

    // Remove the blob file entirely.
    fs::remove_file(&blob_path).expect("remove");
    let err = compose_diff(&store, std::slice::from_ref(&rec), &[]).expect_err("must fail");
    assert!(matches!(err, ChangeHistoryError::MissingEvidence { .. }));
}

#[test]
fn composed_diff_and_summary_across_multiple_records() {
    let dir = TempDir::new().expect("tempdir");
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).expect("ws");
    let store = ChangeHistoryStore::open(dir.path().join("store")).expect("open");

    let r1 = store
        .record_change(
            test_metadata(&ws, "t1", "c1", ChangeSourceKind::ApplyPatch),
            vec![PendingFileChange {
                rel_path: "f.txt".to_string(),
                abs_path: ws.join("f.txt"),
                moved_from: None,
                before: FileSnapshot::Present(b"line1\nline2\n".to_vec()),
                after: FileSnapshot::Present(b"line1\nline2-v2\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("r1");
    let r2 = store
        .record_change(
            test_metadata(&ws, "t1", "c2", ChangeSourceKind::ApplyPatch),
            vec![PendingFileChange {
                rel_path: "f.txt".to_string(),
                abs_path: ws.join("f.txt"),
                moved_from: None,
                before: FileSnapshot::Present(b"line1\nline2-v2\n".to_vec()),
                after: FileSnapshot::Present(b"line1\nline2-v3\nline3\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("r2");

    let summary = compose_summary(&store, &[r1.clone(), r2.clone()], &[]).expect("summary");
    assert_eq!(summary.files.len(), 1);
    assert_eq!(summary.files[0].status, 'M');
    assert_eq!(summary.total_additions, 2);
    assert_eq!(summary.total_deletions, 1);

    let diffs = compose_diff(&store, &[r1, r2], &[]).expect("diff");
    assert!(diffs[0].unified_diff.contains("-line2\n"));
    assert!(diffs[0].unified_diff.contains("+line2-v3\n"));
    assert!(diffs[0].unified_diff.contains("+line3\n"));
}

#[test]
fn path_scoped_revert_preserves_unrelated_edits_and_fails_on_drift_without_markers() {
    let dir = TempDir::new().expect("tempdir");
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).expect("ws");
    let store = ChangeHistoryStore::open(dir.path().join("store")).expect("open");

    let a_path = ws.join("a.txt");
    let b_path = ws.join("b.txt");
    let a_before = b"alpha\nbeta\ngamma\ndelta\n";
    let a_after = b"alpha-edited\nbeta\ngamma\ndelta\n";
    let b_before = b"one\ntwo\n";
    let b_after = b"one\ntwo-edited\n";

    let rec = store
        .record_change(
            test_metadata(&ws, "t1", "c1", ChangeSourceKind::ApplyPatch),
            vec![
                PendingFileChange {
                    rel_path: "a.txt".to_string(),
                    abs_path: a_path.clone(),
                    moved_from: None,
                    before: FileSnapshot::Present(a_before.to_vec()),
                    after: FileSnapshot::Present(a_after.to_vec()),
                    coverage: CoverageStatus::Complete,
                    scope: crate::model::ChangeScope::Workspace,
                },
                PendingFileChange {
                    rel_path: "b.txt".to_string(),
                    abs_path: b_path.clone(),
                    moved_from: None,
                    before: FileSnapshot::Present(b_before.to_vec()),
                    after: FileSnapshot::Present(b_after.to_vec()),
                    coverage: CoverageStatus::Complete,
                    scope: crate::model::ChangeScope::Workspace,
                },
            ],
        )
        .expect("record");

    // Add an unrelated edit on the last line of `a.txt` and keep `b.txt` edited.
    fs::write(&a_path, b"alpha-edited\nbeta\ngamma\ndelta-unrelated\n").expect("write a");
    fs::write(&b_path, b_after).expect("write b");

    // Path-scoped revert of only `a.txt` must revert line 1, preserve line 4, and leave `b.txt` untouched.
    let outcome = store
        .mutate_workspace(
            MutationDirection::Revert,
            std::slice::from_ref(&rec.id),
            &["a.txt".to_string()],
            "t1",
            "sess-1",
        )
        .expect("revert a.txt");
    assert_eq!(outcome.affected_paths, vec!["a.txt"]);
    assert_eq!(
        fs::read_to_string(&a_path).expect("read a"),
        "alpha\nbeta\ngamma\ndelta-unrelated\n"
    );
    assert_eq!(fs::read(&b_path).expect("read b"), b_after);

    // Reapply `a.txt` restores `alpha-edited` while still preserving `delta-unrelated`.
    store
        .mutate_workspace(
            MutationDirection::Apply,
            std::slice::from_ref(&rec.id),
            &["a.txt".to_string()],
            "t1",
            "sess-1",
        )
        .expect("reapply a.txt");
    assert_eq!(
        fs::read_to_string(&a_path).expect("read a"),
        "alpha-edited\nbeta\ngamma\ndelta-unrelated\n"
    );

    // Overlapping drift on line 1 must fail explicitly without writing conflict markers.
    let drifted = "alpha-CONFLICTING\nbeta\ngamma\ndelta-unrelated\n";
    fs::write(&a_path, drifted).expect("write drifted");
    let err = store
        .mutate_workspace(
            MutationDirection::Revert,
            std::slice::from_ref(&rec.id),
            &["a.txt".to_string()],
            "t1",
            "sess-1",
        )
        .expect_err("must reject overlapping drift");
    assert!(matches!(err, ChangeHistoryError::DriftConflict { .. }));
    let on_disk = fs::read_to_string(&a_path).expect("read after failed revert");
    assert_eq!(on_disk, drifted);
    assert!(!on_disk.contains("<<<<<<<"));
}

#[test]
fn revert_and_apply_handle_creation_deletion_and_deletion_restoration() {
    let dir = TempDir::new().expect("tempdir");
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).expect("ws");
    let store = ChangeHistoryStore::open(dir.path().join("store")).expect("open");

    let created_path = ws.join("new.txt");
    let deleted_path = ws.join("old.txt");

    let rec = store
        .record_change(
            test_metadata(&ws, "t1", "c1", ChangeSourceKind::ApplyPatch),
            vec![
                PendingFileChange {
                    rel_path: "new.txt".to_string(),
                    abs_path: created_path.clone(),
                    moved_from: None,
                    before: FileSnapshot::Missing,
                    after: FileSnapshot::Present(b"created\n".to_vec()),
                    coverage: CoverageStatus::Complete,
                    scope: crate::model::ChangeScope::Workspace,
                },
                PendingFileChange {
                    rel_path: "old.txt".to_string(),
                    abs_path: deleted_path.clone(),
                    moved_from: None,
                    before: FileSnapshot::Present(b"deleted\n".to_vec()),
                    after: FileSnapshot::Missing,
                    coverage: CoverageStatus::Complete,
                    scope: crate::model::ChangeScope::Workspace,
                },
            ],
        )
        .expect("record");

    fs::write(&created_path, b"created\n").expect("write created");
    assert!(!deleted_path.exists());

    // Revert: deletes created file and restores deleted file.
    store
        .mutate_workspace(
            MutationDirection::Revert,
            std::slice::from_ref(&rec.id),
            &[],
            "t1",
            "sess-1",
        )
        .expect("revert");
    assert!(!created_path.exists());
    assert_eq!(
        fs::read_to_string(&deleted_path).expect("restored"),
        "deleted\n"
    );

    // Reapply: recreates new.txt and deletes old.txt.
    store
        .mutate_workspace(
            MutationDirection::Apply,
            std::slice::from_ref(&rec.id),
            &[],
            "t1",
            "sess-1",
        )
        .expect("apply");
    assert_eq!(
        fs::read_to_string(&created_path).expect("recreated"),
        "created\n"
    );
    assert!(!deleted_path.exists());
}

#[test]
fn persist_before_publish_leaves_index_clean_on_storage_failure() {
    let dir = TempDir::new().expect("tempdir");
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).expect("ws");
    let store_dir = dir.path().join("store");
    let store = ChangeHistoryStore::open(&store_dir).expect("open");

    // Replace blobs/ directory with a regular file so blob persistence fails.
    fs::remove_dir_all(store_dir.join("blobs")).expect("remove blobs dir");
    fs::write(store_dir.join("blobs"), b"not a directory").expect("block blobs");

    let err = store
        .record_change(
            test_metadata(&ws, "t1", "c1", ChangeSourceKind::ApplyPatch),
            vec![PendingFileChange {
                rel_path: "a.txt".to_string(),
                abs_path: ws.join("a.txt"),
                moved_from: None,
                before: FileSnapshot::Missing,
                after: FileSnapshot::Present(b"hello\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect_err("storage failure must surface explicitly");
    assert!(matches!(err, ChangeHistoryError::Storage(_)));
    // Nothing was published to index.json.
    assert!(store.list_entries().expect("list").is_empty());
}

#[test]
fn mutate_rejects_discontinuous_non_contiguous_selection() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    fs::create_dir_all(&ws).expect("mkdir ws");
    let file = ws.join("a.txt");
    let store = ChangeHistoryStore::open(tmp.path().join("store")).expect("open");

    let r1 = store
        .record_change(
            test_metadata(&ws, "t1", "c1", ChangeSourceKind::ApplyPatch),
            vec![PendingFileChange {
                rel_path: "a.txt".to_string(),
                abs_path: file.clone(),
                moved_from: None,
                before: FileSnapshot::Present(b"v0\n".to_vec()),
                after: FileSnapshot::Present(b"v1\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("r1");
    let _r2 = store
        .record_change(
            test_metadata(&ws, "t1", "c2", ChangeSourceKind::ApplyPatch),
            vec![PendingFileChange {
                rel_path: "a.txt".to_string(),
                abs_path: file.clone(),
                moved_from: None,
                before: FileSnapshot::Present(b"v1\n".to_vec()),
                after: FileSnapshot::Present(b"v2\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("r2");
    let r3 = store
        .record_change(
            test_metadata(&ws, "t1", "c3", ChangeSourceKind::ApplyPatch),
            vec![PendingFileChange {
                rel_path: "a.txt".to_string(),
                abs_path: file.clone(),
                moved_from: None,
                before: FileSnapshot::Present(b"v2\n".to_vec()),
                after: FileSnapshot::Present(b"v3\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("r3");

    fs::write(&file, b"v3\n").expect("write live v3");

    let err = store
        .mutate_workspace(MutationDirection::Revert, &[r1.id, r3.id], &[], "t1", "s1")
        .expect_err("non-contiguous selection must be rejected");
    assert!(
        matches!(err, ChangeHistoryError::DriftConflict { .. }),
        "expected DriftConflict, got {err:?}"
    );
    assert_eq!(fs::read(&file).expect("read"), b"v3\n");
}

#[test]
fn same_rel_path_different_abs_paths_do_not_collide() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let a_dir = ws.join("pkg/a");
    let b_dir = ws.join("pkg/b");
    fs::create_dir_all(&a_dir).expect("mkdir a");
    fs::create_dir_all(&b_dir).expect("mkdir b");
    let a_readme = a_dir.join("README.md");
    let b_readme = b_dir.join("README.md");
    let store = ChangeHistoryStore::open(tmp.path().join("store")).expect("open");

    let r1 = store
        .record_change(
            test_metadata(&ws, "t1", "c1", ChangeSourceKind::ShellWrite),
            vec![PendingFileChange {
                rel_path: "README.md".to_string(),
                abs_path: a_readme.clone(),
                moved_from: None,
                before: FileSnapshot::Present(b"a-old\n".to_vec()),
                after: FileSnapshot::Present(b"a-new\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("r1");
    let r2 = store
        .record_change(
            test_metadata(&ws, "t1", "c2", ChangeSourceKind::ShellWrite),
            vec![PendingFileChange {
                rel_path: "README.md".to_string(),
                abs_path: b_readme.clone(),
                moved_from: None,
                before: FileSnapshot::Present(b"b-old\n".to_vec()),
                after: FileSnapshot::Present(b"b-new\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("r2");

    let diffs = compose_diff(&store, &[r1.clone(), r2.clone()], &[]).expect("diff");
    assert_eq!(diffs.len(), 2, "both README.md files must be preserved");

    fs::write(&a_readme, b"a-new\n").expect("write a");
    fs::write(&b_readme, b"b-new\n").expect("write b");

    store
        .mutate_workspace(MutationDirection::Revert, &[r1.id, r2.id], &[], "t1", "s1")
        .expect("revert both");
    assert_eq!(fs::read(&a_readme).expect("read a"), b"a-old\n");
    assert_eq!(fs::read(&b_readme).expect("read b"), b"b-old\n");
}

#[test]
fn revert_rename_filtered_by_destination_restores_source() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    fs::create_dir_all(&ws).expect("mkdir ws");
    let old_path = ws.join("old.txt");
    let new_path = ws.join("new.txt");
    let store = ChangeHistoryStore::open(tmp.path().join("store")).expect("open");

    let rec = store
        .record_change(
            test_metadata(&ws, "t1", "c1", ChangeSourceKind::ApplyPatch),
            vec![
                PendingFileChange {
                    rel_path: "old.txt".to_string(),
                    abs_path: old_path.clone(),
                    moved_from: None,
                    before: FileSnapshot::Present(b"original\n".to_vec()),
                    after: FileSnapshot::Missing,
                    coverage: CoverageStatus::Complete,
                    scope: crate::model::ChangeScope::Workspace,
                },
                PendingFileChange {
                    rel_path: "new.txt".to_string(),
                    abs_path: new_path.clone(),
                    moved_from: Some("old.txt".to_string()),
                    before: FileSnapshot::Missing,
                    after: FileSnapshot::Present(b"renamed\n".to_vec()),
                    coverage: CoverageStatus::Complete,
                    scope: crate::model::ChangeScope::Workspace,
                },
            ],
        )
        .expect("record rename");

    fs::write(&new_path, b"renamed\n").expect("write new");
    assert!(!old_path.exists());

    store
        .mutate_workspace(
            MutationDirection::Revert,
            &[rec.id],
            &["new.txt".to_string()],
            "t1",
            "s1",
        )
        .expect("revert rename by destination");

    assert!(!new_path.exists(), "destination should be removed");
    assert_eq!(
        fs::read(&old_path).expect("source must be restored"),
        b"original\n"
    );
}

#[test]
fn rename_revert_filter_does_not_leak_across_working_directories() {
    let ws = TempDir::new().expect("ws");
    let pkg_a = ws.path().join("pkg/a");
    let pkg_b = ws.path().join("pkg/b");
    fs::create_dir_all(&pkg_a).expect("mkdir pkg/a");
    fs::create_dir_all(&pkg_b).expect("mkdir pkg/b");
    let store = ChangeHistoryStore::open(ws.path().join("store")).expect("open");

    let a_old = pkg_a.join("old.txt");
    let a_new = pkg_a.join("new.txt");
    let b_old = pkg_b.join("old.txt");

    let rec_a = store
        .record_change(
            test_metadata(&pkg_a, "t1", "c1", ChangeSourceKind::ApplyPatch),
            vec![
                PendingFileChange {
                    rel_path: "old.txt".to_string(),
                    abs_path: a_old.clone(),
                    moved_from: None,
                    before: FileSnapshot::Present(b"a-orig\n".to_vec()),
                    after: FileSnapshot::Missing,
                    coverage: CoverageStatus::Complete,
                    scope: crate::model::ChangeScope::Workspace,
                },
                PendingFileChange {
                    rel_path: "new.txt".to_string(),
                    abs_path: a_new.clone(),
                    moved_from: Some("old.txt".to_string()),
                    before: FileSnapshot::Missing,
                    after: FileSnapshot::Present(b"a-renamed\n".to_vec()),
                    coverage: CoverageStatus::Complete,
                    scope: crate::model::ChangeScope::Workspace,
                },
            ],
        )
        .expect("record rename in pkg/a");

    let rec_b = store
        .record_change(
            test_metadata(&pkg_b, "t1", "c2", ChangeSourceKind::ShellWrite),
            vec![PendingFileChange {
                rel_path: "old.txt".to_string(),
                abs_path: b_old.clone(),
                moved_from: None,
                before: FileSnapshot::Present(b"b-before\n".to_vec()),
                after: FileSnapshot::Present(b"b-after\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("record edit in pkg/b");

    fs::write(&a_new, b"a-renamed\n").expect("write a_new");
    fs::write(&b_old, b"b-after\n").expect("write b_old");

    store
        .mutate_workspace(
            MutationDirection::Revert,
            &[rec_a.id, rec_b.id],
            &["new.txt".to_string()],
            "t1",
            "s1",
        )
        .expect("revert new.txt");

    assert!(!a_new.exists(), "pkg/a/new.txt should be removed");
    assert_eq!(fs::read(&a_old).expect("read a_old"), b"a-orig\n");
    assert_eq!(
        fs::read(&b_old).expect("read b_old"),
        b"b-after\n",
        "pkg/b/old.txt must not be reverted by pkg/a rename filter"
    );
}

#[cfg(unix)]
#[test]
fn revert_refuses_to_follow_replacement_symlink() {
    let ws = TempDir::new().expect("ws");
    let store = ChangeHistoryStore::open(ws.path().join("store")).expect("open");
    let config = ws.path().join("config");
    let shared = ws.path().join("shared-config");

    let rec = store
        .record_change(
            test_metadata(ws.path(), "t1", "c1", ChangeSourceKind::ApplyPatch),
            vec![PendingFileChange {
                rel_path: "config".to_string(),
                abs_path: config.clone(),
                moved_from: None,
                before: FileSnapshot::Present(b"old\n".to_vec()),
                after: FileSnapshot::Present(b"new\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("record");

    fs::write(&shared, b"new\n").expect("write shared");
    std::os::unix::fs::symlink(&shared, &config).expect("symlink config -> shared");

    let err = store
        .mutate_workspace(MutationDirection::Revert, &[rec.id], &[], "t1", "s1")
        .expect_err("revert must refuse replacement symlink");
    assert!(matches!(err, ChangeHistoryError::DriftConflict { .. }));
    assert_eq!(
        fs::read(&shared).expect("read shared"),
        b"new\n",
        "symlink target must remain untouched"
    );
}

#[test]
fn user_shell_source_kind_is_authored_and_roundtrips() {
    assert!(ChangeSourceKind::UserShell.is_authored());
    assert_eq!(ChangeSourceKind::UserShell.label(), "user_shell");
    let json = serde_json::to_string(&ChangeSourceKind::UserShell).expect("serialize");
    assert_eq!(json, "\"user_shell\"");
    let parsed: ChangeSourceKind = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(parsed, ChangeSourceKind::UserShell);

    let ws = TempDir::new().expect("ws");
    let store = ChangeHistoryStore::open(ws.path().join("store")).expect("open");
    let path = ws.path().join("note.txt");

    let rec = store
        .record_change(
            test_metadata(
                ws.path(),
                "t-user",
                "call-user",
                ChangeSourceKind::UserShell,
            ),
            vec![PendingFileChange {
                rel_path: "note.txt".to_string(),
                abs_path: path,
                moved_from: None,
                before: FileSnapshot::Missing,
                after: FileSnapshot::Present(b"from user shell\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::model::ChangeScope::Workspace,
            }],
        )
        .expect("record user_shell");

    let entries = store.list_entries().expect("list entries");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].source_kind, ChangeSourceKind::UserShell);
    assert!(entries[0].source_kind.is_authored());

    let loaded = store.get_record(&rec.id).expect("get record");
    assert_eq!(loaded.metadata.source_kind, ChangeSourceKind::UserShell);

    let history = crate::render_history(&[loaded], &[]);
    assert!(
        history.contains("[user_shell]"),
        "expected [user_shell] label in history: {history}"
    );
}

fn pending_text(
    ws: &std::path::Path,
    rel: &str,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> PendingFileChange {
    PendingFileChange {
        rel_path: rel.to_string(),
        abs_path: ws.join(rel),
        moved_from: None,
        before: before.map_or(FileSnapshot::Missing, |b| FileSnapshot::Present(b.to_vec())),
        after: after.map_or(FileSnapshot::Missing, |b| FileSnapshot::Present(b.to_vec())),
        coverage: CoverageStatus::Complete,
        scope: crate::model::ChangeScope::Workspace,
    }
}

#[test]
fn compose_diff_and_summary_drop_net_noop_reverted_files() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    fs::create_dir_all(&ws).expect("mkdir ws");
    let store = ChangeHistoryStore::open(tmp.path().join("store")).expect("open");

    let r1 = store
        .record_change(
            test_metadata(&ws, "t1", "c1", ChangeSourceKind::ApplyPatch),
            vec![
                pending_text(
                    &ws,
                    "keep.rs",
                    Some(
                        b"fn a() {}
",
                    ),
                    Some(
                        b"fn a() { 1 }
",
                    ),
                ),
                pending_text(
                    &ws,
                    "reverted.rs",
                    Some(
                        b"fn b() {}
",
                    ),
                    Some(
                        b"fn b() { 2 }
",
                    ),
                ),
                pending_text(
                    &ws,
                    "temp.rs",
                    None,
                    Some(
                        b"// tmp
",
                    ),
                ),
            ],
        )
        .expect("r1");
    let r2 = store
        .record_change(
            test_metadata(&ws, "t1", "c2", ChangeSourceKind::VcsDiagnostic),
            vec![
                pending_text(
                    &ws,
                    "reverted.rs",
                    Some(
                        b"fn b() { 2 }
",
                    ),
                    Some(
                        b"fn b() {}
",
                    ),
                ),
                pending_text(
                    &ws,
                    "temp.rs",
                    Some(
                        b"// tmp
",
                    ),
                    None,
                ),
            ],
        )
        .expect("r2");

    let summary = compose_summary(&store, &[r1.clone(), r2.clone()], &[]).expect("summary");
    assert_eq!(summary.files.len(), 1);
    assert_eq!(summary.files[0].rel_path, "keep.rs");
    assert!(summary.files[0].coverage_notes.is_empty());

    let diffs = compose_diff(&store, &[r1, r2], &[]).expect("diff");
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].stats.rel_path, "keep.rs");
}

#[test]
fn compose_diff_and_summary_flag_discontinuous_baseline_from_external_write() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    fs::create_dir_all(&ws).expect("mkdir ws");
    let store = ChangeHistoryStore::open(tmp.path().join("store")).expect("open");

    let r1 = store
        .record_change(
            test_metadata(&ws, "t1", "c1", ChangeSourceKind::ApplyPatch),
            vec![pending_text(
                &ws,
                "a.rs",
                Some(
                    b"v0
",
                ),
                Some(
                    b"v1
",
                ),
            )],
        )
        .expect("r1");
    // Unobserved external write mutated a.rs (v1 -> v1-ext) before r2 restores v0.
    let r2 = store
        .record_change(
            test_metadata(&ws, "t1", "c2", ChangeSourceKind::ApplyPatch),
            vec![pending_text(
                &ws,
                "a.rs",
                Some(
                    b"v1-ext
",
                ),
                Some(
                    b"v0
",
                ),
            )],
        )
        .expect("r2");

    let summary = compose_summary(&store, &[r1.clone(), r2.clone()], &[]).expect("summary");
    assert_eq!(
        summary.files.len(),
        1,
        "discontinuous transition must not be dropped"
    );
    assert!(
        summary.files[0]
            .coverage_notes
            .iter()
            .any(|n| n.contains("discontinuous baseline"))
    );

    let diffs = compose_diff(&store, &[r1, r2], &[]).expect("diff");
    assert_eq!(diffs.len(), 1);
    assert!(diffs[0].unified_diff.contains("discontinuous baseline"));
}

#[test]
fn mutate_skips_net_noop_added_then_deleted_files() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    fs::create_dir_all(&ws).expect("mkdir ws");
    let store = ChangeHistoryStore::open(tmp.path().join("store")).expect("open");

    let r1 = store
        .record_change(
            test_metadata(&ws, "t1", "c1", ChangeSourceKind::ApplyPatch),
            vec![
                pending_text(
                    &ws,
                    "keep.rs",
                    Some(
                        b"old
",
                    ),
                    Some(
                        b"new
",
                    ),
                ),
                pending_text(
                    &ws,
                    "temp.rs",
                    None,
                    Some(
                        b"scratch
",
                    ),
                ),
            ],
        )
        .expect("r1");
    let r2 = store
        .record_change(
            test_metadata(&ws, "t1", "c2", ChangeSourceKind::ApplyPatch),
            vec![pending_text(
                &ws,
                "temp.rs",
                Some(
                    b"scratch
",
                ),
                None,
            )],
        )
        .expect("r2");

    fs::write(
        ws.join("keep.rs"),
        b"new
",
    )
    .expect("write keep");
    fs::write(
        ws.join("temp.rs"),
        b"recreated-later
",
    )
    .expect("write temp");

    // Path-scoped revert of only the net-zero file succeeds with 0 writes (no false NoMatchingPaths).
    let noop = store
        .mutate_workspace(
            MutationDirection::Revert,
            &[r1.id.clone(), r2.id.clone()],
            &["temp.rs".to_string()],
            "t1",
            "s1",
        )
        .expect("net-zero mutation must succeed without error");
    assert!(noop.affected_paths.is_empty());

    // Range revert skips recreated temp.rs and cleanly reverts keep.rs.
    let outcome = store
        .mutate_workspace(MutationDirection::Revert, &[r1.id, r2.id], &[], "t1", "s1")
        .expect("revert range");
    assert_eq!(outcome.affected_paths, vec!["keep.rs"]);
    assert_eq!(
        fs::read(ws.join("keep.rs")).expect("read keep"),
        b"old
"
    );
    assert_eq!(
        fs::read(ws.join("temp.rs")).expect("read temp"),
        b"recreated-later
"
    );
}

#[test]
fn unattributed_author_allocates_shell_stream_and_ephemeral_only_index_flag() {
    let tmp = TempDir::new().expect("tempdir");
    let ws = tmp.path().join("ws");
    let stmp = tmp.path().join("session-tmp");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&stmp).unwrap();
    let store = ChangeHistoryStore::open(tmp.path().join("store")).unwrap();

    let meta = ChangeMetadata {
        thread_id: "calling-thread".to_string(),
        session_id: "sess-1".to_string(),
        turn_id: "turn-1".to_string(),
        call_id: "call-u1".to_string(),
        author: Some(crate::ChangeAuthor::Unattributed),
        source_kind: ChangeSourceKind::UserShell,
        workspace_root: ws,
        created_at: 1_700_000_000,
        exit_code: Some(0),
        summary: None,
        claim_key: None,
    };
    let rec1 = store
        .record_change(
            meta,
            vec![PendingFileChange {
                rel_path: "scratch.txt".to_string(),
                abs_path: stmp.join("scratch.txt"),
                moved_from: None,
                before: FileSnapshot::Missing,
                after: FileSnapshot::Present(b"tmp\n".to_vec()),
                coverage: CoverageStatus::Complete,
                scope: crate::ChangeScope::Ephemeral,
            }],
        )
        .unwrap();

    assert_eq!(rec1.id.as_str(), "shell1");
    assert!(rec1.id.is_unattributed());
    assert!(rec1.metadata.is_unattributed());
    assert_eq!(rec1.metadata.thread_id, "calling-thread");

    let expanded = expand_id_spec("shell1..2").unwrap();
    assert_eq!(
        expanded.iter().map(ChangeId::as_str).collect::<Vec<_>>(),
        vec!["shell1", "shell2"]
    );

    let entries = store.list_entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].thread_id, "calling-thread");
    assert!(entries[0].is_unattributed());
    assert!(entries[0].ephemeral_only);
}

#[test]
fn retained_worktree_diagnostic_classifier_preserves_mixed_and_delegate_edits() {
    let dir = TempDir::new().expect("temp");
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).expect("ws");
    let make = |source_kind, summary: Option<&str>| crate::ChangeRecord {
        schema_version: 1,
        id: crate::ChangeId::new("amber", 1).expect("id"),
        sequence: 1,
        recorded_at_ms: Some(0),
        metadata: crate::ChangeMetadata {
            summary: summary.map(str::to_string),
            ..test_metadata(&ws, "t1", "c1", source_kind)
        },
        files: vec![],
    };
    assert!(
        make(
            ChangeSourceKind::ShellWrite,
            Some("git worktree add .worktrees/task -b task")
        )
        .is_retained_vcs_diagnostic()
    );
    assert!(
        make(
            ChangeSourceKind::ShellWrite,
            Some("git -C '/Client Project' worktree remove --force .worktrees/task && git branch -D task")
        )
        .is_retained_vcs_diagnostic()
    );
    assert!(
        !make(
            ChangeSourceKind::ShellWrite,
            Some("git -C '/Client Project' worktree add .worktrees/task -b task && echo hi > real.txt")
        )
        .is_retained_vcs_diagnostic()
    );
    for script in [
        "git -C '/Research & Development; (QA)|<draft>' worktree add .worktrees/task",
        r#"git -C "/Research && Notes; \"Draft\"" worktree remove .worktrees/task"#,
        "git worktree add --detach '.worktrees/QA task'",
        r"git worktree add --detach .worktrees/QA\ task",
        r"git -C ~/Research\ \&\ Development\ \[QA\] worktree prune",
        "cd ~/repo\ngit worktree prune # remove stale entries",
    ] {
        assert!(
            make(ChangeSourceKind::ShellWrite, Some(script)).is_retained_vcs_diagnostic(),
            "{script}"
        );
    }
    for script in [
        "git -C '/Research & Development' worktree add .worktrees/task; touch keep.txt",
        r"git worktree add --detach .worktrees/QA\ task && touch keep.txt",
        "git worktree add --detach .worktrees/QA*",
        "git worktree add .worktrees/task | git status",
        "git worktree add .worktrees/task &",
        "git worktree add .worktrees/task || git status",
        "git -C \"$(touch keep.txt)\" worktree add .worktrees/task",
    ] {
        assert!(
            !make(ChangeSourceKind::ShellWrite, Some(script)).is_retained_vcs_diagnostic(),
            "{script}"
        );
    }
    assert!(
        !make(
            ChangeSourceKind::ShellWrite,
            Some(r#"python3 -c 'open(".worktrees/task/a.rs", "w").write("ok")'"#)
        )
        .is_retained_vcs_diagnostic()
    );
    assert!(
        !make(
            ChangeSourceKind::ApplyPatch,
            Some("git worktree add .worktrees/task -b task")
        )
        .is_retained_vcs_diagnostic()
    );
}
