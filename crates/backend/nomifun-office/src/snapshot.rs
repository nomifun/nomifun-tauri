use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nomifun_api_types::{PreviewHistoryTargetDto, PreviewSnapshotInfoDto, SnapshotContentResponse};
use nomifun_common::PreviewSnapshotId;
use sha1::{Digest, Sha1};
use tracing::warn;

use crate::error::OfficeError;

const MAX_SNAPSHOTS: usize = 50;
const SNAPSHOT_EXT: &str = ".md";
const INDEX_FILE: &str = "index.json";

pub struct SnapshotService {
    base_dir: PathBuf,
}

impl SnapshotService {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            base_dir: data_dir.join("preview-history"),
        }
    }

    pub async fn list(&self, target: &PreviewHistoryTargetDto) -> Result<Vec<PreviewSnapshotInfoDto>, OfficeError> {
        let dir = self.target_dir(target);
        let index_path = dir.join(INDEX_FILE);

        if !index_path.exists() {
            return Ok(Vec::new());
        }

        let content = tokio::fs::read_to_string(&index_path).await?;
        let mut snapshots: Vec<PreviewSnapshotInfoDto> = serde_json::from_str(&content)?;
        snapshots.sort_by_key(|a| a.created_at);
        Ok(snapshots)
    }

    pub async fn save(
        &self,
        target: &PreviewHistoryTargetDto,
        content: &str,
    ) -> Result<PreviewSnapshotInfoDto, OfficeError> {
        let dir = self.target_dir(target);
        tokio::fs::create_dir_all(&dir).await?;

        let now_ms = current_timestamp_ms();
        let snapshot_id = PreviewSnapshotId::new();
        let file_name = format!("{snapshot_id}{SNAPSHOT_EXT}");
        let file_path = dir.join(&file_name);

        tokio::fs::write(&file_path, content.as_bytes()).await?;

        let label = format_label(now_ms);
        let info = PreviewSnapshotInfoDto {
            snapshot_id,
            label,
            created_at: now_ms,
            size: content.len() as u64,
            content_type: target.content_type,
            file_name: target.file_name.clone(),
            file_path: target.file_path.clone(),
        };

        let mut snapshots: Vec<PreviewSnapshotInfoDto> = self.read_index(&dir).await;
        snapshots.push(info.clone());

        self.trim_and_write_index(&dir, &mut snapshots).await?;

        Ok(info)
    }

    pub async fn get_content(
        &self,
        target: &PreviewHistoryTargetDto,
        snapshot_id: &PreviewSnapshotId,
    ) -> Result<Option<SnapshotContentResponse>, OfficeError> {
        let dir = self.target_dir(target);
        let snapshots: Vec<PreviewSnapshotInfoDto> = self.read_index(&dir).await;

        let Some(info) = snapshots
            .into_iter()
            .find(|s| s.snapshot_id == *snapshot_id)
        else {
            return Ok(None);
        };

        let file_path = dir.join(format!("{snapshot_id}{SNAPSHOT_EXT}"));
        if !file_path.exists() {
            return Ok(None);
        }

        let content = tokio::fs::read_to_string(&file_path).await?;
        Ok(Some(SnapshotContentResponse {
            snapshot: info,
            content,
        }))
    }

    fn target_dir(&self, target: &PreviewHistoryTargetDto) -> PathBuf {
        let hash = compute_target_hash(target);
        self.base_dir.join(hash)
    }

    async fn read_index(&self, dir: &Path) -> Vec<PreviewSnapshotInfoDto> {
        let index_path = dir.join(INDEX_FILE);
        let Ok(content) = tokio::fs::read_to_string(&index_path).await else {
            return Vec::new();
        };
        serde_json::from_str(&content).unwrap_or_default()
    }

    async fn trim_and_write_index(
        &self,
        dir: &Path,
        snapshots: &mut Vec<PreviewSnapshotInfoDto>,
    ) -> Result<(), OfficeError> {
        snapshots.sort_by_key(|a| a.created_at);

        while snapshots.len() > MAX_SNAPSHOTS {
            if let Some(oldest) = snapshots.first() {
                let file_path = dir.join(format!("{}{SNAPSHOT_EXT}", oldest.snapshot_id));
                if let Err(e) = tokio::fs::remove_file(&file_path).await {
                    warn!(path = %file_path.display(), error = %e, "failed to remove old snapshot file");
                }
            }
            snapshots.remove(0);
        }

        let index_path = dir.join(INDEX_FILE);
        let json = serde_json::to_string_pretty(snapshots)?;
        tokio::fs::write(&index_path, json.as_bytes()).await?;
        Ok(())
    }
}

fn compute_target_hash(target: &PreviewHistoryTargetDto) -> String {
    let mut hasher = Sha1::new();

    hasher.update(
        serde_json::to_value(target.content_type)
            .unwrap_or_default()
            .as_str()
            .unwrap_or_default()
            .as_bytes(),
    );
    hasher.update(b"\0");

    let fields: [Option<&str>; 5] = [
        target.file_path.as_deref(),
        target.workspace.as_deref(),
        target.file_name.as_deref(),
        target.title.as_deref(),
        target.language.as_deref(),
    ];

    for field in &fields {
        if let Some(val) = field {
            hasher.update(val.as_bytes());
        }
        hasher.update(b"\0");
    }
    if let Some(conv_id) = target.conversation_id.as_ref() {
        hasher.update(conv_id.as_str().as_bytes());
    }
    hasher.update(b"\0");

    let result = hasher.finalize();
    result.iter().fold(String::with_capacity(40), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn current_timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn format_label(timestamp_ms: i64) -> String {
    let secs = timestamp_ms / 1000;
    let minutes = (secs / 60) % 60;
    let hours = (secs / 3600) % 24;
    let mut days = secs / 86400;

    let mut year: i64 = 1970;
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let month_days: [i64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0;
    let mut day = days;
    for (i, &md) in month_days.iter().enumerate() {
        let md = if i == 1 && is_leap_year(year) { md + 1 } else { md };
        if day < md {
            month = i + 1;
            break;
        }
        day -= md;
    }

    if month == 0 {
        month = 12;
    }

    format!("{year:04}-{month:02}-{:02} {hours:02}:{minutes:02}", day + 1,)
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomifun_common::PreviewContentType;

    #[test]
    fn compute_hash_deterministic() {
        let target = PreviewHistoryTargetDto {
            content_type: PreviewContentType::Markdown,
            file_path: Some("/a.md".into()),
            workspace: None,
            file_name: None,
            title: None,
            language: None,
            conversation_id: None,
        };
        let h1 = compute_target_hash(&target);
        let h2 = compute_target_hash(&target);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 40);
    }

    #[test]
    fn compute_hash_different_targets() {
        let t1 = PreviewHistoryTargetDto {
            content_type: PreviewContentType::Markdown,
            file_path: Some("/a.md".into()),
            workspace: None,
            file_name: None,
            title: None,
            language: None,
            conversation_id: None,
        };
        let t2 = PreviewHistoryTargetDto {
            content_type: PreviewContentType::Markdown,
            file_path: Some("/b.md".into()),
            workspace: None,
            file_name: None,
            title: None,
            language: None,
            conversation_id: None,
        };
        assert_ne!(compute_target_hash(&t1), compute_target_hash(&t2));
    }

    #[test]
    fn compute_hash_extra_fields_differ() {
        let conversation_id = nomifun_common::ConversationId::try_from(
            "0190f5fe-7c00-7a00-8abc-012345678901",
        )
        .unwrap();
        let t1 = PreviewHistoryTargetDto {
            content_type: PreviewContentType::Code,
            file_path: Some("/x.rs".into()),
            workspace: Some("/ws".into()),
            file_name: None,
            title: None,
            language: None,
            conversation_id: Some(conversation_id),
        };
        let t2 = PreviewHistoryTargetDto {
            content_type: PreviewContentType::Code,
            file_path: Some("/x.rs".into()),
            workspace: None,
            file_name: None,
            title: None,
            language: None,
            conversation_id: None,
        };
        assert_ne!(compute_target_hash(&t1), compute_target_hash(&t2));
    }

    #[test]
    fn timestamp_ms_positive() {
        let ts = current_timestamp_ms();
        assert!(ts > 0);
    }

    #[test]
    fn format_label_known_timestamps() {
        // 2023-11-14 22:13 UTC
        assert_eq!(format_label(1700000000000), "2023-11-14 22:13");
        // 1970-01-01 00:00 UTC (epoch)
        assert_eq!(format_label(0), "1970-01-01 00:00");
        // 2000-02-29 00:00 UTC (leap year)
        assert_eq!(format_label(951782400000), "2000-02-29 00:00");
        // 2000-03-01 00:00 UTC (day after leap)
        assert_eq!(format_label(951868800000), "2000-03-01 00:00");
        // 2024-12-31 23:59 UTC
        assert_eq!(format_label(1735689540000), "2024-12-31 23:59");
    }

    #[test]
    fn compute_hash_field_boundary_collision() {
        // file_path="/foobar" vs file_path="/foo" + workspace="bar"
        let t1 = PreviewHistoryTargetDto {
            content_type: PreviewContentType::Markdown,
            file_path: Some("/foobar".into()),
            workspace: None,
            file_name: None,
            title: None,
            language: None,
            conversation_id: None,
        };
        let t2 = PreviewHistoryTargetDto {
            content_type: PreviewContentType::Markdown,
            file_path: Some("/foo".into()),
            workspace: Some("bar".into()),
            file_name: None,
            title: None,
            language: None,
            conversation_id: None,
        };
        assert_ne!(compute_target_hash(&t1), compute_target_hash(&t2));
    }

    #[test]
    fn compute_hash_field_position_collision() {
        // file_name="x" vs title="x"
        let t1 = PreviewHistoryTargetDto {
            content_type: PreviewContentType::Code,
            file_path: None,
            workspace: None,
            file_name: Some("x".into()),
            title: None,
            language: None,
            conversation_id: None,
        };
        let t2 = PreviewHistoryTargetDto {
            content_type: PreviewContentType::Code,
            file_path: None,
            workspace: None,
            file_name: None,
            title: Some("x".into()),
            language: None,
            conversation_id: None,
        };
        assert_ne!(compute_target_hash(&t1), compute_target_hash(&t2));
    }

    #[tokio::test]
    async fn service_list_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = SnapshotService::new(tmp.path());
        let target = make_target(PreviewContentType::Markdown, Some("/a.md"));
        let result = svc.list(&target).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn service_save_and_list() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = SnapshotService::new(tmp.path());
        let target = make_target(PreviewContentType::Markdown, Some("/a.md"));

        let info = svc.save(&target, "# Hello").await.unwrap();
        assert!(!info.snapshot_id.is_empty());
        assert_eq!(info.size, 7);
        assert_eq!(info.content_type, PreviewContentType::Markdown);

        let list = svc.list(&target).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].snapshot_id, info.snapshot_id);
    }

    #[tokio::test]
    async fn service_get_content() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = SnapshotService::new(tmp.path());
        let target = make_target(PreviewContentType::Html, Some("/b.html"));

        let info = svc.save(&target, "<h1>Hi</h1>").await.unwrap();
        let resp = svc
            .get_content(&target, &info.snapshot_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resp.content, "<h1>Hi</h1>");
        assert_eq!(resp.snapshot.snapshot_id, info.snapshot_id);
    }

    #[tokio::test]
    async fn service_get_content_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = SnapshotService::new(tmp.path());
        let target = make_target(PreviewContentType::Markdown, Some("/a.md"));

        let nonexistent = PreviewSnapshotId::new();
        let resp = svc.get_content(&target, &nonexistent).await.unwrap();
        assert!(resp.is_none());
    }

    #[tokio::test]
    async fn service_trim_over_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let svc = SnapshotService::new(tmp.path());
        let target = make_target(PreviewContentType::Code, Some("/c.rs"));

        for i in 0..52 {
            svc.save(&target, &format!("content-{i}")).await.unwrap();
        }

        let list = svc.list(&target).await.unwrap();
        assert_eq!(list.len(), MAX_SNAPSHOTS);
    }

    #[tokio::test]
    async fn service_different_targets_isolated() {
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
}
