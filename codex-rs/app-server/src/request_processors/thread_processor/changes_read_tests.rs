use std::path::Path;

use codex_change_history::ChangeMetadata;
use codex_change_history::ChangeScope;
use codex_change_history::ChangeSourceKind;
use codex_change_history::CoverageStatus;
use codex_change_history::FileSnapshot;
use codex_change_history::PendingFileChange;
use pretty_assertions::assert_eq;

use super::CAPTURE_PAGE_BYTES;
use super::CAPTURE_PATCH_BYTES;
use super::ChangeHistoryStore;
use super::MAX_RESPONSE_BYTES;
use super::ThreadChangesReadParams;
use super::parse_cursor;
use super::read_changes;

fn record(store: &ChangeHistoryStore, workspace: &Path, name: &str, files: Vec<(&str, String)>) {
    store
        .record_change(
            ChangeMetadata {
                thread_id: "thread".into(),
                session_id: "session".into(),
                turn_id: "turn".into(),
                call_id: name.into(),
                author: None,
                source_kind: ChangeSourceKind::ShellWrite,
                workspace_root: workspace.to_path_buf(),
                created_at: 0,
                exit_code: Some(0),
                summary: None,
                claim_key: None,
            },
            files
                .into_iter()
                .map(|(path, text)| PendingFileChange {
                    rel_path: path.into(),
                    abs_path: workspace.join(path),
                    moved_from: None,
                    before: FileSnapshot::Missing,
                    after: FileSnapshot::Present(text.into_bytes()),
                    coverage: CoverageStatus::Complete,
                    scope: ChangeScope::Workspace,
                })
                .collect(),
        )
        .expect("record captured files");
}

#[test]
fn command_receipts_page_only_requested_captures_without_patch_text() {
    let workspace = tempfile::tempdir().expect("temp workspace");
    let dir = workspace.path().join("history");
    let store = ChangeHistoryStore::open(dir.clone()).expect("open history");
    for (item, path) in [("first", "a"), ("omitted", "b"), ("last", "c")] {
        record(
            &store,
            workspace.path(),
            item,
            vec![(path, "one\ntwo\n".into())],
        );
    }
    let params = ThreadChangesReadParams {
        thread_id: "thread".into(),
        include_external: None,
        include_ephemeral: None,
        caller_thread_id: None,
        item_id: None,
        receipt_item_ids: Some(vec!["first".into(), "last".into()]),
        per_capture: false,
        limit: Some(1),
        cursor: None,
    };
    let mut cursor = None;
    for (item, path) in [("last", "c"), ("first", "a")] {
        let page = read_changes(
            dir.clone(),
            None,
            Some("thread".into()),
            params.clone(),
            cursor,
        )
        .expect("read receipts");
        assert_eq!(page.file_count, 2);
        assert_eq!(page.changes.len(), 1);
        assert_eq!(page.files.len(), 1);
        assert_eq!(
            page.files[0].change_ids,
            vec![page.changes[0].change_id.clone()]
        );
        assert!(page.files[0].unified_diff.is_empty());
        assert_eq!(
            (
                page.changes[0].item_id.as_str(),
                page.files[0].path.as_str(),
                page.files[0].additions
            ),
            (item, path, 2),
        );
        cursor = page
            .next_cursor
            .as_deref()
            .map(parse_cursor)
            .transpose()
            .expect("parse cursor");
    }
    assert!(cursor.is_none());
}

#[test]
fn capture_pages_bound_encoded_payload_and_resume_inside_a_patch() {
    let workspace = tempfile::tempdir().expect("temp workspace");
    let dir = workspace.path().join("history");
    let store = ChangeHistoryStore::open(dir.clone()).expect("open history");
    // Escape-heavy text measures encoded bytes, not just unescaped patch size.
    let escaped = "\u{1}".repeat(380_000);
    let large = "界".repeat(CAPTURE_PATCH_BYTES / 2);
    record(
        &store,
        workspace.path(),
        "first",
        vec![
            ("a", escaped.clone()),
            ("b", escaped),
            ("c", large),
            ("d", "final\n".into()),
        ],
    );
    let mut params = ThreadChangesReadParams {
        thread_id: "thread".into(),
        include_external: None,
        include_ephemeral: None,
        caller_thread_id: None,
        item_id: None,
        receipt_item_ids: None,
        per_capture: true,
        limit: None,
        cursor: None,
    };
    let expected =
        read_changes(dir.clone(), None, None, params.clone(), None).expect("unpaged read");
    params.limit = Some(100);
    let mut cursor = None;
    let mut actual: Vec<(String, String)> = Vec::new();
    for page_index in 0..8 {
        let page =
            read_changes(dir.clone(), None, None, params.clone(), cursor).expect("capture page");
        assert!(serde_json::to_vec(&page).expect("encode page").len() < CAPTURE_PAGE_BYTES);
        if page_index == 0 {
            assert_eq!(page.files.len(), 1, "encoded-byte budget ended first page");
            record(
                &store,
                workspace.path(),
                "later",
                vec![("e", "new capture\n".into())],
            );
        }
        for file in page.files {
            if let Some(last) = actual.last_mut().filter(|last| last.0 == file.path) {
                last.1.push_str(&file.unified_diff);
            } else {
                actual.push((file.path, file.unified_diff));
            }
        }
        let Some(next) = page.next_cursor else {
            assert_eq!(
                actual,
                expected
                    .files
                    .into_iter()
                    .map(|file| (file.path, file.unified_diff))
                    .collect::<Vec<_>>()
            );
            return;
        };
        cursor = Some(parse_cursor(&next).expect("valid continuation cursor"));
    }
    panic!("capture pagination never completed");
}

#[test]
fn capture_pages_preserve_origin_paths_and_fill_page_budget_across_chunks() {
    let workspace = tempfile::tempdir().expect("temp workspace");
    let dir = workspace.path().join("history");
    let store = ChangeHistoryStore::open(dir.clone()).expect("open history");
    // ~5 MiB ASCII file spans multiple 512 KiB slices and exceeds one 4 MiB page.
    let big_line = format!("{}\n", "x".repeat(127));
    let big = big_line.repeat(40_960);
    record(
        &store,
        workspace.path(),
        "multi-chunk",
        vec![
            ("a.txt", "before\n".into()),
            ("big.txt", big),
            ("z.txt", "after\n".into()),
        ],
    );
    let mut params = ThreadChangesReadParams {
        thread_id: "thread".into(),
        include_external: None,
        include_ephemeral: None,
        caller_thread_id: None,
        item_id: None,
        receipt_item_ids: None,
        per_capture: true,
        limit: None,
        cursor: None,
    };
    let expected =
        read_changes(dir.clone(), None, None, params.clone(), None).expect("unpaged read");
    params.limit = Some(100);

    let mut cursor = None;
    let mut pages = 0usize;
    let mut total_additions = 0i64;
    let mut total_deletions = 0i64;
    let mut actual: Vec<(String, String)> = Vec::new();
    loop {
        let page =
            read_changes(dir.clone(), None, None, params.clone(), cursor).expect("capture page");
        assert!(serde_json::to_vec(&page).expect("encode page").len() < CAPTURE_PAGE_BYTES);
        assert_eq!(page.changes.len(), 1);
        assert_eq!(
            page.changes[0].paths,
            vec!["a.txt", "big.txt", "z.txt"],
            "page {pages} must preserve complete origin.paths"
        );
        if pages == 0 {
            assert_eq!(page.files.len(), 2);
            assert_eq!(page.files[0].path, "a.txt");
            assert_eq!(page.files[1].path, "big.txt");
            assert!(
                page.files[1].unified_diff.len() >= 3 * CAPTURE_PATCH_BYTES,
                "first page should pack multiple 512 KiB slices within the 4 MiB page budget"
            );
        }
        total_additions += page.total_additions;
        total_deletions += page.total_deletions;
        for file in page.files {
            if let Some(last) = actual.last_mut().filter(|last| last.0 == file.path) {
                last.1.push_str(&file.unified_diff);
            } else {
                actual.push((file.path, file.unified_diff));
            }
        }
        pages += 1;
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(parse_cursor(&next).expect("valid continuation cursor"));
        assert!(pages < 4, "5 MiB capture should complete within 2 pages");
    }
    assert_eq!(pages, 2);
    assert_eq!(total_additions, expected.total_additions);
    assert_eq!(total_deletions, expected.total_deletions);
    assert_eq!(
        actual,
        expected
            .files
            .into_iter()
            .map(|file| (file.path, file.unified_diff))
            .collect::<Vec<_>>()
    );
}

#[test]
fn capture_pages_fallback_when_origin_paths_exceed_page_budget() {
    let workspace = tempfile::tempdir().expect("temp workspace");
    let dir = workspace.path().join("history");
    let store = ChangeHistoryStore::open(dir.clone()).expect("open history");
    // 20,000 files with ~220-byte paths -> >4.4 MiB JSON for origin.paths alone.
    let prefix = "nested/".repeat(28);
    let files: Vec<(String, String)> = (0..20_000)
        .map(|i| (format!("{prefix}file_{i:05}.txt"), "x\n".into()))
        .collect();
    record(
        &store,
        workspace.path(),
        "bulk-paths",
        files
            .iter()
            .map(|(path, body)| (path.as_str(), body.clone()))
            .collect(),
    );
    let params = ThreadChangesReadParams {
        thread_id: "thread".into(),
        include_external: None,
        include_ephemeral: None,
        caller_thread_id: None,
        item_id: None,
        receipt_item_ids: None,
        per_capture: true,
        limit: Some(100),
        cursor: None,
    };
    let page = read_changes(dir.clone(), None, None, params.clone(), None)
        .expect("oversized origin.paths should fall back instead of failing");
    assert!(serde_json::to_vec(&page).expect("encode page").len() < CAPTURE_PAGE_BYTES);
    assert_eq!(page.files.len(), 100);
    assert_eq!(page.changes.len(), 1);
    assert_eq!(page.changes[0].paths.len(), 100);
    assert!(page.next_cursor.is_some());
    let next = parse_cursor(page.next_cursor.as_deref().expect("cursor")).expect("valid cursor");
    let page2 = read_changes(dir, None, None, params, Some(next)).expect("second page");
    assert!(serde_json::to_vec(&page2).expect("encode page 2").len() < CAPTURE_PAGE_BYTES);
    assert_eq!(page2.files.len(), 100);
    assert_eq!(page2.changes.len(), 1);
    assert_eq!(page2.changes[0].paths.len(), 100);
    assert_ne!(page.changes[0].paths, page2.changes[0].paths);
}

#[test]
fn capture_pages_paginate_large_rename_with_long_source_path() {
    let workspace = tempfile::tempdir().expect("temp workspace");
    let dir = workspace.path().join("history");
    let store = ChangeHistoryStore::open(dir.clone()).expect("open history");
    let long_source = format!("{}original.txt", "nested_directory_segment/".repeat(100));
    assert!(long_source.len() > 2000);
    let big_line = format!("{}\n", "y".repeat(127));
    let big = big_line.repeat(40_960);
    store
        .record_change(
            ChangeMetadata {
                thread_id: "thread".into(),
                session_id: "session".into(),
                turn_id: "turn".into(),
                call_id: "large-rename".into(),
                author: None,
                source_kind: ChangeSourceKind::ApplyPatch,
                workspace_root: workspace.path().to_path_buf(),
                created_at: 0,
                exit_code: Some(0),
                summary: None,
                claim_key: None,
            },
            vec![PendingFileChange {
                rel_path: "a.txt".into(),
                abs_path: workspace.path().join("a.txt"),
                moved_from: Some(long_source.clone()),
                before: FileSnapshot::Present(b"old\n".to_vec()),
                after: FileSnapshot::Present(big.into_bytes()),
                coverage: CoverageStatus::Complete,
                scope: ChangeScope::Workspace,
            }],
        )
        .expect("record large rename");
    let mut params = ThreadChangesReadParams {
        thread_id: "thread".into(),
        include_external: None,
        include_ephemeral: None,
        caller_thread_id: None,
        item_id: None,
        receipt_item_ids: None,
        per_capture: true,
        limit: None,
        cursor: None,
    };
    let expected =
        read_changes(dir.clone(), None, None, params.clone(), None).expect("unpaged read");
    params.limit = Some(100);

    let mut cursor = None;
    let mut pages = 0usize;
    let mut combined_diff = String::new();
    loop {
        let page =
            read_changes(dir.clone(), None, None, params.clone(), cursor).expect("rename page");
        assert!(serde_json::to_vec(&page).expect("encode page").len() < CAPTURE_PAGE_BYTES);
        assert_eq!(page.files.len(), 1);
        assert_eq!(page.files[0].path, "a.txt");
        assert_eq!(
            page.files[0].moved_from.as_deref(),
            Some(long_source.as_str())
        );
        combined_diff.push_str(&page.files[0].unified_diff);
        pages += 1;
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(parse_cursor(&next).expect("valid continuation cursor"));
        assert!(pages < 4);
    }
    assert_eq!(pages, 2);
    assert_eq!(combined_diff, expected.files[0].unified_diff);
}

#[test]
fn composed_review_pages_keep_totals_and_provenance_under_the_connector_limit() {
    let workspace = tempfile::tempdir().expect("temp workspace");
    let dir = workspace.path().join("history");
    let store = ChangeHistoryStore::open(dir.clone()).expect("open history");
    let escaped = "\u{1}".repeat(6 << 20);
    record(
        &store,
        workspace.path(),
        "last-page-command",
        vec![("z", "final\n".into())],
    );
    record(
        &store,
        workspace.path(),
        "first-page-command",
        vec![
            ("a", escaped.clone()),
            ("b", escaped),
            ("too-large", "\u{1}".repeat(11 << 20)),
        ],
    );
    for caller in [None, Some("thread".to_string())] {
        let params = ThreadChangesReadParams {
            thread_id: "thread".into(),
            include_external: None,
            include_ephemeral: None,
            caller_thread_id: caller,
            item_id: None,
            receipt_item_ids: None,
            per_capture: false,
            limit: Some(20),
            cursor: None,
        };
        let mut cursor = None;
        for (index, expected_paths) in [vec!["a"], vec!["b"], vec!["too-large", "z"]]
            .into_iter()
            .enumerate()
        {
            let page =
                read_changes(dir.clone(), None, None, params.clone(), cursor).expect("review page");
            assert!(serde_json::to_vec(&page).expect("encode page").len() < MAX_RESPONSE_BYTES);
            assert_eq!(page.file_count, 4);
            assert_eq!((page.total_additions, page.total_deletions), (4, 0));
            assert_eq!(
                page.files
                    .iter()
                    .map(|file| file.path.as_str())
                    .collect::<Vec<_>>(),
                expected_paths
            );
            assert_eq!(
                page.changes
                    .iter()
                    .map(|change| change.item_id.as_str())
                    .collect::<Vec<_>>(),
                if index < 2 {
                    vec!["first-page-command"]
                } else {
                    vec!["last-page-command", "first-page-command"]
                }
            );
            assert_eq!(
                page.files[0].change_ids,
                [page.changes.last().expect("file origin").change_id.as_str()]
            );
            if index < 2 {
                assert!(page.files[0].coverage_notes.is_empty());
                assert!(page.files[0].unified_diff.contains('\u{1}'));
                let next = page.next_cursor.expect("continued review");
                cursor = Some(parse_cursor(&next).expect("valid review continuation"));
            } else {
                assert_eq!(
                    page.files[0].coverage_notes,
                    ["Patch exceeds the Saved Diff display limit."]
                );
                assert!(page.files[0].unified_diff.is_empty());
                assert!(page.files[1].unified_diff.contains("+final"));
                assert_eq!(
                    page.files[1].change_ids,
                    [page.changes[0].change_id.as_str()]
                );
                assert_eq!(page.next_cursor, None);
            }
        }
    }
}
