use nomifun_api_types::{PreviewHistoryTargetDto, PreviewSnapshotInfoDto};
use nomifun_common::{ConversationId, PreviewContentType, PreviewSnapshotId};
use nomifun_office::SnapshotService;

fn make_target(content_type: PreviewContentType, file_path: Option<&str>) -> PreviewHistoryTargetDto {
    PreviewHistoryTargetDto {
        content_type,
        file_path: file_path.map(String::from),
        workspace: None,
        file_name: None,
        title: None,
        language: None,
        conversation_id: None,
    }
}

fn make_target_full(
    content_type: PreviewContentType,
    file_path: Option<&str>,
    workspace: Option<&str>,
    conversation_id: Option<ConversationId>,
) -> PreviewHistoryTargetDto {
    PreviewHistoryTargetDto {
        content_type,
        file_path: file_path.map(String::from),
        workspace: workspace.map(String::from),
        file_name: None,
        title: None,
        language: None,
        conversation_id,
    }
}

// SH-1: Save snapshot
#[tokio::test]
async fn sh1_save_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());
    let target = make_target(PreviewContentType::Markdown, Some("/a.md"));

    let info = svc.save(&target, "# Hello").await.unwrap();

    assert!(!info.snapshot_id.is_empty(), "snapshot_id must not be empty");
    assert!(info.created_at > 0, "createdAt must be current timestamp");
    assert_eq!(info.size, 7, "size must be content byte count");
    assert_eq!(info.content_type, PreviewContentType::Markdown);
    assert!(!info.label.is_empty(), "label must not be empty");
}

// SH-2: List snapshots (ordered by createdAt)
#[tokio::test]
async fn sh2_list_snapshots_ordered() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());
    let target = make_target(PreviewContentType::Markdown, Some("/a.md"));

    let s1 = svc.save(&target, "content-1").await.unwrap();
    let s2 = svc.save(&target, "content-2").await.unwrap();
    let s3 = svc.save(&target, "content-3").await.unwrap();

    let list = svc.list(&target).await.unwrap();
    assert_eq!(list.len(), 3);

    assert_eq!(list[0].snapshot_id, s1.snapshot_id);
    assert_eq!(list[1].snapshot_id, s2.snapshot_id);
    assert_eq!(list[2].snapshot_id, s3.snapshot_id);
    assert!(list[0].created_at <= list[1].created_at);
    assert!(list[1].created_at <= list[2].created_at);
}

// SH-3: Get snapshot content
#[tokio::test]
async fn sh3_get_snapshot_content() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());
    let target = make_target(PreviewContentType::Markdown, Some("/a.md"));

    let info = svc.save(&target, "# Hello").await.unwrap();
    let resp = svc.get_content(&target, &info.snapshot_id).await.unwrap();

    assert!(resp.is_some());
    let resp = resp.unwrap();
    assert_eq!(resp.content, "# Hello");
    assert_eq!(resp.snapshot.snapshot_id, info.snapshot_id);
    assert_eq!(resp.snapshot.size, info.size);
    assert_eq!(resp.snapshot.content_type, PreviewContentType::Markdown);
}

// SH-4: Get nonexistent snapshot returns None
#[tokio::test]
async fn sh4_get_nonexistent_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());
    let target = make_target(PreviewContentType::Markdown, Some("/a.md"));

    let nonexistent = PreviewSnapshotId::new();
    let resp = svc.get_content(&target, &nonexistent).await.unwrap();
    assert!(resp.is_none());
}

// SH-4b: Get nonexistent snapshot from a target with existing snapshots
#[tokio::test]
async fn sh4b_get_nonexistent_snapshot_with_existing() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());
    let target = make_target(PreviewContentType::Markdown, Some("/a.md"));

    svc.save(&target, "some content").await.unwrap();
    let nonexistent = PreviewSnapshotId::new();
    let resp = svc.get_content(&target, &nonexistent).await.unwrap();
    assert!(resp.is_none());
}

// SH-5: Trim snapshots over limit (50)
#[tokio::test]
async fn sh5_trim_over_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());
    let target = make_target(PreviewContentType::Code, Some("/c.rs"));

    let mut first_ids: Vec<PreviewSnapshotId> = Vec::new();
    for i in 0..51 {
        let info = svc.save(&target, &format!("content-{i}")).await.unwrap();
        if i == 0 {
            first_ids.push(info.snapshot_id.clone());
        }
    }

    let list = svc.list(&target).await.unwrap();
    assert_eq!(list.len(), 50, "must trim to 50 snapshots");

    assert!(
        !list.iter().any(|s| s.snapshot_id == first_ids[0]),
        "oldest snapshot must be removed"
    );
}

// SH-5b: Verify snapshot file is also deleted
#[tokio::test]
async fn sh5b_trim_deletes_files() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());
    let target = make_target(PreviewContentType::Code, Some("/c.rs"));

    let first = svc.save(&target, "first").await.unwrap();
    for i in 1..51 {
        svc.save(&target, &format!("content-{i}")).await.unwrap();
    }

    let resp = svc.get_content(&target, &first.snapshot_id).await.unwrap();
    assert!(resp.is_none(), "trimmed snapshot file must not be readable");
}

// SH-6: Different targets are isolated
#[tokio::test]
async fn sh6_different_targets_isolated() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());
    let t1 = make_target(PreviewContentType::Markdown, Some("/a.md"));
    let t2 = make_target(PreviewContentType::Markdown, Some("/b.md"));

    svc.save(&t1, "content-a").await.unwrap();
    svc.save(&t2, "content-b1").await.unwrap();
    svc.save(&t2, "content-b2").await.unwrap();

    let list1 = svc.list(&t1).await.unwrap();
    let list2 = svc.list(&t2).await.unwrap();
    assert_eq!(list1.len(), 1);
    assert_eq!(list2.len(), 2);
}

// SH-6b: Different content types are isolated
#[tokio::test]
async fn sh6b_different_content_types_isolated() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());
    let t1 = make_target(PreviewContentType::Markdown, Some("/a.md"));
    let t2 = make_target(PreviewContentType::Html, Some("/a.md"));

    svc.save(&t1, "md content").await.unwrap();
    svc.save(&t2, "html content").await.unwrap();

    let list1 = svc.list(&t1).await.unwrap();
    let list2 = svc.list(&t2).await.unwrap();
    assert_eq!(list1.len(), 1);
    assert_eq!(list2.len(), 1);
}

// SH-7: Target field combination produces different SHA-1 directories
#[tokio::test]
async fn sh7_target_field_combination_different_hash() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());

    let t1 = make_target(PreviewContentType::Markdown, Some("/a.md"));
    let t2 = make_target_full(
        PreviewContentType::Markdown,
        Some("/a.md"),
        Some("/ws"),
        Some(
            ConversationId::try_from("0190f5fe-7c00-7a00-8abc-012345678901")
                .unwrap(),
        ),
    );

    svc.save(&t1, "content-1").await.unwrap();
    svc.save(&t2, "content-2").await.unwrap();

    let list1 = svc.list(&t1).await.unwrap();
    let list2 = svc.list(&t2).await.unwrap();
    assert_eq!(list1.len(), 1);
    assert_eq!(list2.len(), 1);

    let r1 = svc
        .get_content(&t1, &list1[0].snapshot_id)
        .await
        .unwrap()
        .unwrap();
    let r2 = svc
        .get_content(&t2, &list2[0].snapshot_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r1.content, "content-1");
    assert_eq!(r2.content, "content-2");
}

// SH-7b: Verify SHA-1 directory naming by inspecting filesystem
#[tokio::test]
async fn sh7b_sha1_directory_naming() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());
    let target = make_target(PreviewContentType::Markdown, Some("/a.md"));

    svc.save(&target, "test").await.unwrap();

    let history_dir = tmp.path().join("preview-history");
    let mut entries = std::fs::read_dir(&history_dir).unwrap();
    let dir_entry = entries.next().unwrap().unwrap();
    let dir_name = dir_entry.file_name().to_string_lossy().to_string();

    assert_eq!(dir_name.len(), 40, "SHA-1 hex must be 40 characters");
    assert!(
        dir_name.chars().all(|c| c.is_ascii_hexdigit()),
        "directory name must be hex"
    );

    let index = std::fs::read_to_string(dir_entry.path().join("index.json")).unwrap();
    let snapshots: Vec<PreviewSnapshotInfoDto> = serde_json::from_str(&index).unwrap();
    assert_eq!(snapshots.len(), 1);
}

// Extra: List on empty directory returns empty vec
#[tokio::test]
async fn list_empty_returns_empty_vec() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());
    let target = make_target(PreviewContentType::Pdf, Some("/doc.pdf"));

    let list = svc.list(&target).await.unwrap();
    assert!(list.is_empty());
}

// Extra: Save preserves file_name and file_path from target
#[tokio::test]
async fn save_preserves_target_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let svc = SnapshotService::new(tmp.path());
    let target = PreviewHistoryTargetDto {
        content_type: PreviewContentType::Word,
        file_path: Some("/docs/report.docx".into()),
        workspace: None,
        file_name: Some("report.docx".into()),
        title: None,
        language: None,
        conversation_id: None,
    };

    let info = svc.save(&target, "word content").await.unwrap();
    assert_eq!(info.file_path.as_deref(), Some("/docs/report.docx"));
    assert_eq!(info.file_name.as_deref(), Some("report.docx"));
}
