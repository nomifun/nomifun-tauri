//! Fail-closed reset support for Nomi's on-disk conversation sessions.
//!
//! `nomi-agent::SessionManager` keys a persisted transcript by conversation ID.
//! The conversation row's `created_at` is stamped into `Session::owner_token`
//! and is therefore the stable identity of the exact conversation generation.
//! A reset must verify both values before changing the file: the conversation
//! ID alone is reusable derived-state identity and is not sufficient authority.

use std::io::Write;
use std::path::{Path, PathBuf};

use nomi_agent::session::{Session, SessionIndex, SessionMeta, session_belongs_to};
use nomifun_common::{AppError, ConversationId};

/// Result of clearing one exact persisted Nomi conversation generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NomiSessionResetOutcome {
    /// The owned session existed and its resumable state was durably cleared.
    Cleared,
    /// Neither an index entry nor a transcript exists for this conversation.
    AlreadyAbsent,
    /// A stale index entry with no transcript was removed.
    RepairedStaleIndex,
}

/// Owns the filesystem location used by Nomi's [`SessionManager`].
///
/// The reset operation is synchronous because it performs a small bounded
/// amount of local file I/O. [`crate::runtime_registry::AgentRuntimeRegistry`]
/// runs it on a blocking worker after the live runtime has exited.
#[derive(Debug, Clone)]
pub struct NomiSessionPersistence {
    session_directory: PathBuf,
}

impl NomiSessionPersistence {
    /// Bind to an exact Nomi session directory (normally
    /// `{app_data_dir}/nomi-sessions`).
    pub fn new(session_directory: PathBuf) -> Self {
        Self { session_directory }
    }

    pub fn session_directory(&self) -> &Path {
        &self.session_directory
    }

    /// Atomically clear the resumable state belonging to the exact conversation
    /// generation identified by `conversation_id` + `conversation_created_at`.
    ///
    /// Successful return guarantees a fresh `SessionManager` load cannot
    /// observe any pre-reset messages, usage, or deferred-tool activations.
    /// Owner mismatch, ambiguous files, malformed JSON, and every I/O failure
    /// are errors; callers must not commit the authoritative DB reset then.
    pub fn reset_owned_session(
        &self,
        conversation_id: &str,
        conversation_created_at: i64,
    ) -> Result<NomiSessionResetOutcome, AppError> {
        ConversationId::parse(conversation_id).map_err(|error| {
            AppError::BadRequest(format!(
                "invalid conversation id for Nomi session reset: {error}"
            ))
        })?;
        if conversation_created_at <= 0 {
            return Err(AppError::BadRequest(
                "conversation_created_at must be positive for Nomi session reset".to_owned(),
            ));
        }

        let directory_metadata = match std::fs::symlink_metadata(&self.session_directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(NomiSessionResetOutcome::AlreadyAbsent);
            }
            Err(error) => {
                return Err(io_error(
                    "inspect Nomi session directory",
                    &self.session_directory,
                    error,
                ));
            }
        };
        if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
            return Err(AppError::Internal(format!(
                "Nomi session directory is not a real directory: {}",
                self.session_directory.display()
            )));
        }

        let index_path = self.session_directory.join("index.json");
        let index = load_optional_index(&index_path)?;
        let indexed_matches: Vec<SessionMeta> = index
            .as_ref()
            .map(|index| {
                index
                    .sessions
                    .iter()
                    .filter(|meta| meta.id == conversation_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if indexed_matches.len() > 1 {
            return Err(AppError::Internal(format!(
                "Nomi session index contains duplicate entries for conversation {conversation_id}"
            )));
        }

        let candidates = session_candidates(&self.session_directory, conversation_id)?;
        if candidates.len() > 1 {
            return Err(AppError::Internal(format!(
                "Nomi session storage contains ambiguous transcript files for conversation {conversation_id}"
            )));
        }

        let Some(path) = candidates.first() else {
            if indexed_matches.is_empty() {
                return Ok(NomiSessionResetOutcome::AlreadyAbsent);
            }

            // A prior interrupted cleanup may have removed the transcript
            // before its index entry. Repair only that exact metadata entry so
            // SessionManager::create can establish a genuinely fresh session.
            let mut repaired = index.expect("indexed_matches requires an index");
            repaired
                .sessions
                .retain(|meta| meta.id != conversation_id);
            save_json_atomic(&index_path, &repaired)?;
            return Ok(NomiSessionResetOutcome::RepairedStaleIndex);
        };

        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| io_error("inspect Nomi session transcript", path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AppError::Internal(format!(
                "Nomi session transcript is not a real file: {}",
                path.display()
            )));
        }

        let raw = std::fs::read(path)
            .map_err(|error| io_error("read Nomi session transcript", path, error))?;
        let mut session: Session = serde_json::from_slice(&raw).map_err(|error| {
            AppError::Internal(format!(
                "parse Nomi session transcript {}: {error}",
                path.display()
            ))
        })?;
        if session.id != conversation_id {
            return Err(AppError::Internal(format!(
                "Nomi session transcript {} contains unexpected id {}",
                path.display(),
                session.id
            )));
        }

        if let Some(meta) = indexed_matches.first()
            && meta.created_at != session.created_at
        {
            return Err(AppError::Internal(format!(
                "Nomi session index timestamp does not match transcript for conversation {conversation_id}"
            )));
        }

        let expected_owner = conversation_created_at.to_string();
        if !session_belongs_to(
            session.owner_token.as_deref(),
            session.created_at.timestamp_millis(),
            &expected_owner,
            conversation_created_at,
        ) {
            return Err(AppError::Conflict(format!(
                "persisted Nomi session does not belong to the current generation of conversation {conversation_id}"
            )));
        }

        session.messages.clear();
        session.total_usage = Default::default();
        session.activated_deferred_tools.clear();
        session.updated_at = chrono::Utc::now();
        save_json_atomic(path, &session)?;

        // The transcript is the resumable source of truth and is committed
        // first. The index is then brought to the same empty state. If this
        // second atomic write fails, the caller fails closed and a retry safely
        // repairs the already-cleared transcript's metadata.
        let mut repaired_index = index.unwrap_or(SessionIndex {
            sessions: Vec::new(),
        });
        if let Some(meta) = repaired_index
            .sessions
            .iter_mut()
            .find(|meta| meta.id == conversation_id)
        {
            meta.updated_at = session.updated_at;
            meta.model = session.model.clone();
            meta.summary.clear();
            meta.message_count = 0;
        } else {
            repaired_index.sessions.push(SessionMeta {
                id: session.id.clone(),
                created_at: session.created_at,
                updated_at: session.updated_at,
                model: session.model.clone(),
                summary: String::new(),
                message_count: 0,
            });
        }
        save_json_atomic(&index_path, &repaired_index)?;

        Ok(NomiSessionResetOutcome::Cleared)
    }
}

fn load_optional_index(path: &Path) -> Result<Option<SessionIndex>, AppError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("inspect Nomi session index", path, error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::Internal(format!(
            "Nomi session index is not a real file: {}",
            path.display()
        )));
    }
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) => return Err(io_error("read Nomi session index", path, error)),
    };
    serde_json::from_slice(&raw).map(Some).map_err(|error| {
        AppError::Internal(format!(
            "parse Nomi session index {}: {error}",
            path.display()
        ))
    })
}

fn session_candidates(directory: &Path, conversation_id: &str) -> Result<Vec<PathBuf>, AppError> {
    let suffix = format!("_{conversation_id}.json");
    let entries = std::fs::read_dir(directory)
        .map_err(|error| io_error("list Nomi session directory", directory, error))?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry
            .map_err(|error| io_error("read Nomi session directory entry", directory, error))?;
        let file_name = entry.file_name();
        let file_name = file_name.to_str().ok_or_else(|| {
            AppError::Internal(format!(
                "Nomi session directory contains a non-Unicode file name: {}",
                entry.path().display()
            ))
        })?;
        if file_name.ends_with(&suffix) {
            candidates.push(entry.path());
        }
    }
    candidates.sort();
    Ok(candidates)
}

fn save_json_atomic(path: &Path, value: &impl serde::Serialize) -> Result<(), AppError> {
    let directory = path.parent().ok_or_else(|| {
        AppError::Internal(format!(
            "Nomi session path has no parent directory: {}",
            path.display()
        ))
    })?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        AppError::Internal(format!(
            "serialize Nomi session state {}: {error}",
            path.display()
        ))
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.json");
    let mut temp = tempfile::Builder::new()
        .prefix(&format!(".{file_name}.reset."))
        .tempfile_in(directory)
        .map_err(|error| io_error("create Nomi session reset tempfile", directory, error))?;
    temp.write_all(&bytes)
        .and_then(|_| temp.as_file_mut().sync_all())
        .map_err(|error| io_error("write Nomi session reset tempfile", temp.path(), error))?;
    let temp_path = temp.into_temp_path();
    replace_file_atomic(temp_path.as_ref(), path)
        .map_err(|error| io_error("commit Nomi session reset", path, error))?;
    sync_directory(directory)
        .map_err(|error| io_error("sync Nomi session directory", directory, error))
}

#[cfg(windows)]
fn replace_file_atomic(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let target: Vec<u16> = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: both paths are owned, NUL-terminated UTF-16 buffers that remain
    // alive for the duration of this synchronous Win32 call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn replace_file_atomic(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(all(not(unix), not(windows)))]
fn replace_file_atomic(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(unix)]
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    std::fs::File::open(directory)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

fn io_error(operation: &str, path: &Path, error: std::io::Error) -> AppError {
    AppError::Internal(format!("{operation} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nomi_agent::session::SessionManager;
    use nomi_types::message::{ContentBlock, Message, Role};

    fn add_context(manager: &SessionManager, session: &mut Session, text: &str, owner: i64) {
        session.owner_token = Some(owner.to_string());
        session.messages.push(Message::new(
            Role::User,
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
        ));
        session.activated_deferred_tools = vec!["knowledge_search".to_owned()];
        manager.save(session).expect("save seeded session");
        manager
            .update_index_for(session)
            .expect("update seeded session index");
    }

    #[test]
    fn exact_generation_is_cleared_and_unrelated_session_survives() {
        let root = tempfile::tempdir().expect("temp root");
        let session_dir = root.path().join("nomi-sessions");
        let manager = SessionManager::new(session_dir.clone(), 100);
        let target_id = ConversationId::new().into_string();
        let sibling_id = ConversationId::new().into_string();
        let owner = chrono::Utc::now().timestamp_millis() - 1_000;
        let sibling_owner = owner - 10_000;

        let mut target = manager
            .create("openai", "model", "/target", Some(&target_id))
            .expect("create target");
        add_context(&manager, &mut target, "old target context", owner);
        let mut sibling = manager
            .create("openai", "model", "/sibling", Some(&sibling_id))
            .expect("create sibling");
        add_context(
            &manager,
            &mut sibling,
            "unrelated context must survive",
            sibling_owner,
        );

        let persistence = NomiSessionPersistence::new(session_dir.clone());
        assert_eq!(
            persistence
                .reset_owned_session(&target_id, owner)
                .expect("reset exact generation"),
            NomiSessionResetOutcome::Cleared
        );

        // A freshly constructed loader is the same read boundary used by the
        // Nomi factory. It cannot observe the old resumable context.
        let fresh_loader = SessionManager::new(session_dir, 100);
        let cleared = fresh_loader.load(&target_id).expect("load cleared target");
        assert!(cleared.messages.is_empty());
        assert!(cleared.activated_deferred_tools.is_empty());
        assert_eq!(cleared.total_usage, Default::default());
        let fresh_index = fresh_loader.list().expect("load repaired session index");
        let cleared_meta = fresh_index
            .iter()
            .find(|meta| meta.id == target_id)
            .expect("cleared target remains indexed");
        assert!(cleared_meta.summary.is_empty());
        assert_eq!(cleared_meta.message_count, 0);
        assert_eq!(cleared_meta.updated_at, cleared.updated_at);

        let preserved = fresh_loader
            .load(&sibling_id)
            .expect("load unrelated sibling");
        assert_eq!(preserved.messages.len(), 1);
        assert!(matches!(
            preserved.messages[0].content.as_slice(),
            [ContentBlock::Text { text }] if text == "unrelated context must survive"
        ));
        assert_eq!(
            preserved.activated_deferred_tools,
            vec!["knowledge_search".to_owned()]
        );
        let sibling_meta = fresh_index
            .iter()
            .find(|meta| meta.id == sibling_id)
            .expect("unrelated sibling remains indexed");
        assert_eq!(sibling_meta.summary, "unrelated context must survive");
        assert_eq!(sibling_meta.message_count, 1);
    }

    #[test]
    fn wrong_generation_is_rejected_without_changing_context() {
        let root = tempfile::tempdir().expect("temp root");
        let session_dir = root.path().join("nomi-sessions");
        let manager = SessionManager::new(session_dir.clone(), 100);
        let conversation_id = ConversationId::new().into_string();
        let owner = chrono::Utc::now().timestamp_millis() - 1_000;
        let mut session = manager
            .create("openai", "model", "/target", Some(&conversation_id))
            .expect("create target");
        add_context(&manager, &mut session, "must remain", owner);

        let result = NomiSessionPersistence::new(session_dir)
            .reset_owned_session(&conversation_id, owner + 1);
        assert!(matches!(result, Err(AppError::Conflict(_))));
        assert_eq!(
            manager
                .load(&conversation_id)
                .expect("wrong-owner session remains")
                .messages
                .len(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_atomic_replace_clears_an_existing_file_for_a_fresh_loader() {
        let root = tempfile::tempdir().expect("temp root");
        let session_dir = root.path().join("nomi-sessions");
        let manager = SessionManager::new(session_dir.clone(), 100);
        let conversation_id = ConversationId::new().into_string();
        let owner = chrono::Utc::now().timestamp_millis() - 1_000;
        let mut session = manager
            .create("openai", "model", r"C:\workspace", Some(&conversation_id))
            .expect("create existing target file");
        add_context(
            &manager,
            &mut session,
            "Windows replacement must erase this",
            owner,
        );

        NomiSessionPersistence::new(session_dir.clone())
            .reset_owned_session(&conversation_id, owner)
            .expect("MoveFileExW replaces the existing transcript");

        let fresh_loader = SessionManager::new(session_dir, 100);
        let loaded = fresh_loader
            .load(&conversation_id)
            .expect("fresh loader reads replacement");
        assert!(loaded.messages.is_empty());
        assert!(loaded.activated_deferred_tools.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_index_commit_failure_is_reported_and_retry_repairs_metadata() {
        let root = tempfile::tempdir().expect("temp root");
        let session_dir = root.path().join("nomi-sessions");
        let manager = SessionManager::new(session_dir.clone(), 100);
        let conversation_id = ConversationId::new().into_string();
        let owner = chrono::Utc::now().timestamp_millis() - 1_000;
        let mut session = manager
            .create("openai", "model", r"C:\workspace", Some(&conversation_id))
            .expect("create target");
        add_context(&manager, &mut session, "must be cleared first", owner);

        let index_path = session_dir.join("index.json");
        let mut permissions = std::fs::metadata(&index_path)
            .expect("index metadata")
            .permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&index_path, permissions).expect("make index read-only");

        let persistence = NomiSessionPersistence::new(session_dir.clone());
        let first = persistence.reset_owned_session(&conversation_id, owner);

        let mut permissions = std::fs::metadata(&index_path)
            .expect("read-only index remains")
            .permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(&index_path, permissions).expect("restore index permissions");

        assert!(
            matches!(first, Err(AppError::Internal(_))),
            "the caller must observe the failed index commit"
        );
        assert!(
            manager
                .load(&conversation_id)
                .expect("transcript was committed before index")
                .messages
                .is_empty(),
            "old transcript context must already be gone"
        );

        assert_eq!(
            persistence
                .reset_owned_session(&conversation_id, owner)
                .expect("retry repairs index"),
            NomiSessionResetOutcome::Cleared
        );
        let repaired = manager
            .list()
            .expect("load repaired index")
            .into_iter()
            .find(|meta| meta.id == conversation_id)
            .expect("target remains indexed");
        assert!(repaired.summary.is_empty());
        assert_eq!(repaired.message_count, 0);
    }

    #[test]
    fn malformed_index_fails_closed() {
        let root = tempfile::tempdir().expect("temp root");
        let session_dir = root.path().join("nomi-sessions");
        std::fs::create_dir_all(&session_dir).expect("create session directory");
        std::fs::write(session_dir.join("index.json"), b"{not-json")
            .expect("write malformed index");

        let result = NomiSessionPersistence::new(session_dir).reset_owned_session(
            &ConversationId::new().into_string(),
            chrono::Utc::now().timestamp_millis(),
        );
        assert!(matches!(result, Err(AppError::Internal(_))));
    }

    #[test]
    fn absent_session_is_idempotent() {
        let root = tempfile::tempdir().expect("temp root");
        let result = NomiSessionPersistence::new(root.path().join("missing"))
            .reset_owned_session(
                &ConversationId::new().into_string(),
                chrono::Utc::now().timestamp_millis(),
            )
            .expect("missing session is already reset");
        assert_eq!(result, NomiSessionResetOutcome::AlreadyAbsent);
    }
}
