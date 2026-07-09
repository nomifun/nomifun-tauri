use std::path::Path;
use std::sync::Arc;

use nomifun_api_types::ToolType;

use crate::error::ShellError;
use crate::opener::ISystemOpener;

const ALLOWED_URL_SCHEMES: &[&str] = &["http", "https", "mailto"];

pub struct ShellService {
    opener: Arc<dyn ISystemOpener>,
}

impl ShellService {
    pub fn new(opener: Arc<dyn ISystemOpener>) -> Self {
        Self { opener }
    }

    pub async fn open_file(&self, file_path: &str) -> Result<(), ShellError> {
        let path = validate_file_exists(file_path)?;
        self.opener.open_detached(&path.to_string_lossy())
    }

    pub async fn show_item_in_folder(&self, file_path: &str) -> Result<(), ShellError> {
        let path = validate_path_exists(file_path)?;
        if cfg!(target_os = "macos") {
            self.opener.run_command("open", &["-R", &path.to_string_lossy()]).await
        } else if cfg!(target_os = "windows") {
            let parent = path.parent().unwrap_or(&path);
            self.opener.run_command("explorer", &[&parent.to_string_lossy()]).await
        } else {
            let parent = path.parent().unwrap_or(&path);
            self.open_linux_path(parent).await
        }
    }

    pub async fn open_external(&self, url: &str) -> Result<(), ShellError> {
        validate_url(url)?;
        self.opener.open_detached(url)
    }

    /// Launch a URL, file, folder, or application (by name or path) via the OS
    /// shell (ShellExecute on Windows). Unlike `open_external`/`open_file`, this
    /// accepts any target — app names like `msedge`, arbitrary paths — so an
    /// agent can reliably open browsers/apps WITHOUT the fragile `cmd /c start`
    /// window-title-argument quirk. `app` optionally launches the target with a
    /// specific application (e.g. open a URL in a named browser). The target is
    /// guarded against the empty / bare-path-separator inputs (e.g. `\\`) that
    /// otherwise surface a Windows "cannot find '\\'" ShellExecute dialog.
    pub async fn launch(&self, target: &str, app: Option<&str>) -> Result<(), ShellError> {
        validate_launch_target(target)?;
        match app {
            Some(app) => self.opener.open_with_detached(target, app),
            None => self.opener.open_detached(target),
        }
    }

    pub async fn check_tool_installed(&self, tool: ToolType) -> bool {
        match tool {
            ToolType::Terminal | ToolType::Explorer => true,
            ToolType::Vscode => self.detect_vscode(),
        }
    }

    pub async fn open_folder_with(&self, folder_path: &str, tool: ToolType) -> Result<(), ShellError> {
        let path = validate_directory_exists(folder_path)?;
        match tool {
            ToolType::Vscode => self.open_folder_vscode(&path).await,
            ToolType::Terminal => self.open_folder_terminal(&path).await,
            ToolType::Explorer => self.open_folder_explorer(&path).await,
        }
    }

    fn detect_vscode(&self) -> bool {
        if self.opener.is_tool_available("code") {
            return true;
        }
        if cfg!(target_os = "macos") {
            let app_path = "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code";
            return Path::new(app_path).exists();
        }
        false
    }

    async fn open_folder_vscode(&self, path: &Path) -> Result<(), ShellError> {
        if !self.detect_vscode() {
            return Err(ShellError::ToolNotInstalled("vscode".to_owned()));
        }
        self.opener.run_command("code", &[&path.to_string_lossy()]).await
    }

    async fn open_folder_terminal(&self, path: &Path) -> Result<(), ShellError> {
        let path_str = path.to_string_lossy();
        if cfg!(target_os = "macos") {
            self.opener.run_command("open", &["-a", "Terminal", &path_str]).await
        } else if cfg!(target_os = "windows") {
            // `start "" /D <dir> cmd`: the empty first argument is the window
            // title — without it, `start` treats a quoted path (any path with
            // spaces) as the title instead of the command. `/D` sets the
            // startup directory as a discrete argument, so no `cd /d` string
            // splicing is needed.
            self.opener
                .run_command("cmd", &["/c", "start", "", "/D", &path_str, "cmd"])
                .await
        } else {
            self.try_linux_terminal(&path_str).await
        }
    }

    async fn open_folder_explorer(&self, path: &Path) -> Result<(), ShellError> {
        let path_str = path.to_string_lossy();
        if cfg!(target_os = "macos") {
            self.opener.run_command("open", &[&path_str]).await
        } else if cfg!(target_os = "windows") {
            self.opener.run_command("explorer", &[&path_str]).await
        } else {
            self.open_linux_path(path).await
        }
    }

    async fn try_linux_terminal(&self, path: &str) -> Result<(), ShellError> {
        let candidates = linux_terminal_candidates(path);
        let mut last_error: Option<ShellError> = None;

        for (term, args) in candidates {
            if self.opener.is_tool_available(term) {
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                match self.opener.run_command(term, &arg_refs).await {
                    Ok(()) => return Ok(()),
                    Err(error) => last_error = Some(error),
                }
            }
        }

        Err(last_error.unwrap_or_else(|| ShellError::ToolNotInstalled("terminal emulator".to_owned())))
    }

    async fn open_linux_path(&self, path: &Path) -> Result<(), ShellError> {
        let path_str = path.to_string_lossy();

        if self.opener.open_detached(&path_str).is_ok() {
            return Ok(());
        }

        let mut last_error: Option<ShellError> = None;
        for (program, args) in linux_file_manager_candidates(&path_str) {
            if !self.opener.is_tool_available(program) {
                continue;
            }

            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            match self.opener.run_command(program, &arg_refs).await {
                Ok(()) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }

        Err(last_error
            .unwrap_or_else(|| ShellError::ToolNotInstalled("Linux file manager opener".to_owned())))
    }
}

fn linux_file_manager_candidates(path: &str) -> Vec<(&'static str, Vec<String>)> {
    vec![
        ("xdg-open", vec![path.to_owned()]),
        ("gio", vec!["open".to_owned(), path.to_owned()]),
        ("kde-open", vec![path.to_owned()]),
        ("kde-open5", vec![path.to_owned()]),
        ("exo-open", vec![path.to_owned()]),
    ]
}

fn linux_terminal_candidates(path: &str) -> Vec<(&'static str, Vec<String>)> {
    let shell_cd = "cd \"$1\" && exec \"${SHELL:-sh}\" -l";

    vec![
        (
            "gnome-terminal",
            vec!["--working-directory".to_owned(), path.to_owned()],
        ),
        (
            "kgx",
            vec!["--working-directory".to_owned(), path.to_owned()],
        ),
        ("konsole", vec!["--workdir".to_owned(), path.to_owned()]),
        (
            "xfce4-terminal",
            vec!["--working-directory".to_owned(), path.to_owned()],
        ),
        (
            "mate-terminal",
            vec!["--working-directory".to_owned(), path.to_owned()],
        ),
        (
            "terminator",
            vec!["--working-directory".to_owned(), path.to_owned()],
        ),
        (
            "tilix",
            vec!["--working-directory".to_owned(), path.to_owned()],
        ),
        ("kitty", vec!["--directory".to_owned(), path.to_owned()]),
        (
            "wezterm",
            vec!["start".to_owned(), "--cwd".to_owned(), path.to_owned()],
        ),
        (
            "alacritty",
            vec!["--working-directory".to_owned(), path.to_owned()],
        ),
        (
            "x-terminal-emulator",
            vec![
                "-e".to_owned(),
                "sh".to_owned(),
                "-lc".to_owned(),
                shell_cd.to_owned(),
                "sh".to_owned(),
                path.to_owned(),
            ],
        ),
        (
            "xterm",
            vec![
                "-e".to_owned(),
                "sh".to_owned(),
                "-lc".to_owned(),
                shell_cd.to_owned(),
                "sh".to_owned(),
                path.to_owned(),
            ],
        ),
    ]
}

fn validate_file_exists(file_path: &str) -> Result<std::path::PathBuf, ShellError> {
    let path = Path::new(file_path);
    let canonical = path
        .canonicalize()
        .map_err(|_| ShellError::FileNotFound(file_path.to_owned()))?;
    if !canonical.is_file() {
        return Err(ShellError::FileNotFound(file_path.to_owned()));
    }
    Ok(canonical)
}

fn validate_path_exists(file_path: &str) -> Result<std::path::PathBuf, ShellError> {
    let path = Path::new(file_path);
    let canonical = path
        .canonicalize()
        .map_err(|_| ShellError::FileNotFound(file_path.to_owned()))?;
    if !canonical.exists() {
        return Err(ShellError::FileNotFound(file_path.to_owned()));
    }
    Ok(canonical)
}

fn validate_directory_exists(dir_path: &str) -> Result<std::path::PathBuf, ShellError> {
    let path = Path::new(dir_path);
    let canonical = path
        .canonicalize()
        .map_err(|_| ShellError::DirectoryNotFound(dir_path.to_owned()))?;
    if !canonical.is_dir() {
        return Err(ShellError::DirectoryNotFound(dir_path.to_owned()));
    }
    Ok(canonical)
}

fn validate_url(url: &str) -> Result<(), ShellError> {
    let parsed = reqwest::Url::parse(url).map_err(|_| ShellError::InvalidUrl(url.to_owned()))?;
    if !ALLOWED_URL_SCHEMES.contains(&parsed.scheme()) {
        return Err(ShellError::InvalidUrl(format!(
            "scheme '{}' is not allowed",
            parsed.scheme()
        )));
    }
    Ok(())
}

/// Reject launch targets the OS shell cannot meaningfully open and that surface
/// a "Windows cannot find 'X'" ShellExecute dialog: an empty/whitespace target,
/// or a string consisting ENTIRELY of path separators (`\` / `/`) — e.g. a bare
/// UNC root `\\`. Real URLs, paths, and app names contain non-separator
/// characters and pass.
fn validate_launch_target(target: &str) -> Result<(), ShellError> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return Err(ShellError::InvalidTarget(
            "empty target — provide a URL, file/folder path, or application name".to_owned(),
        ));
    }
    // A target made up entirely of path separators (e.g. a bare UNC root `\\`)
    // is not something ShellExecute can open; passing it through surfaces the
    // "Windows cannot find '\\'" dialog. Reject it with a clear message.
    if trimmed.chars().all(|c| c == '\\' || c == '/') {
        return Err(ShellError::InvalidTarget(format!(
            "{target:?} is only path separators (e.g. a bare UNC root); provide a real URL, \
             path, or application name"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::opener::NoopSystemOpener;
    use std::fs;

    #[test]
    fn validate_file_exists_succeeds_for_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();
        let result = validate_file_exists(file_path.to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_file_exists_fails_for_missing_file() {
        let result = validate_file_exists("/nonexistent/file.txt");
        assert!(matches!(result, Err(ShellError::FileNotFound(_))));
    }

    #[test]
    fn validate_file_exists_fails_for_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = validate_file_exists(dir.path().to_str().unwrap());
        assert!(matches!(result, Err(ShellError::FileNotFound(_))));
    }

    #[test]
    fn validate_path_exists_succeeds_for_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();
        let result = validate_path_exists(file_path.to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_path_exists_succeeds_for_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = validate_path_exists(dir.path().to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_path_exists_fails_for_nonexistent() {
        let result = validate_path_exists("/nonexistent/path");
        assert!(matches!(result, Err(ShellError::FileNotFound(_))));
    }

    #[test]
    fn validate_directory_exists_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let result = validate_directory_exists(dir.path().to_str().unwrap());
        assert!(result.is_ok());
    }

    #[test]
    fn validate_directory_exists_fails_for_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();
        let result = validate_directory_exists(file_path.to_str().unwrap());
        assert!(matches!(result, Err(ShellError::DirectoryNotFound(_))));
    }

    #[test]
    fn validate_directory_exists_fails_for_nonexistent() {
        let result = validate_directory_exists("/nonexistent/dir");
        assert!(matches!(result, Err(ShellError::DirectoryNotFound(_))));
    }

    #[test]
    fn validate_url_accepts_http() {
        assert!(validate_url("http://example.com").is_ok());
    }

    #[test]
    fn validate_url_accepts_https() {
        assert!(validate_url("https://example.com/path?q=1").is_ok());
    }

    #[test]
    fn validate_url_accepts_mailto() {
        assert!(validate_url("mailto:user@example.com").is_ok());
    }

    #[test]
    fn validate_url_rejects_file_scheme() {
        let result = validate_url("file:///etc/passwd");
        assert!(matches!(result, Err(ShellError::InvalidUrl(msg)) if msg.contains("scheme")));
    }

    #[test]
    fn validate_url_rejects_ftp_scheme() {
        let result = validate_url("ftp://example.com");
        assert!(matches!(result, Err(ShellError::InvalidUrl(msg)) if msg.contains("scheme")));
    }

    #[test]
    fn validate_url_rejects_javascript_scheme() {
        let result = validate_url("javascript:alert(1)");
        assert!(matches!(result, Err(ShellError::InvalidUrl(msg)) if msg.contains("scheme")));
    }

    #[test]
    fn validate_url_rejects_invalid_url() {
        let result = validate_url("; rm -rf /");
        assert!(matches!(result, Err(ShellError::InvalidUrl(_))));
    }

    #[test]
    fn validate_url_rejects_empty_string() {
        let result = validate_url("");
        assert!(matches!(result, Err(ShellError::InvalidUrl(_))));
    }

    // --- launch target validation (reliable open tool; the `\\` dialog guard) ---

    #[test]
    fn validate_launch_target_accepts_url_path_and_app() {
        assert!(validate_launch_target("https://www.baidu.com/s?wd=x").is_ok());
        assert!(validate_launch_target("C:\\Users\\rika0\\file.txt").is_ok());
        assert!(validate_launch_target("/usr/bin/firefox").is_ok());
        assert!(validate_launch_target("msedge").is_ok());
        assert!(validate_launch_target("notepad.exe").is_ok());
    }

    #[test]
    fn validate_launch_target_rejects_empty_and_blank() {
        assert!(matches!(validate_launch_target(""), Err(ShellError::InvalidTarget(_))));
        assert!(matches!(validate_launch_target("   "), Err(ShellError::InvalidTarget(_))));
    }

    #[test]
    fn validate_launch_target_rejects_bare_separators() {
        // The exact failure mode: a bare UNC root / lone separators that
        // ShellExecute cannot open (each Rust literal below: "\\\\" == two
        // backslashes == the `\\` the user saw).
        for t in ["\\", "\\\\", "/", "//", "\\\\\\\\", " \\\\ ", "\\/"] {
            assert!(
                matches!(validate_launch_target(t), Err(ShellError::InvalidTarget(_))),
                "should reject {t:?}"
            );
        }
    }

    #[tokio::test]
    async fn launch_rejects_bare_backslash_before_opening() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        assert!(matches!(
            svc.launch("\\\\", None).await,
            Err(ShellError::InvalidTarget(_))
        ));
    }

    #[tokio::test]
    async fn launch_accepts_url_with_and_without_app() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        assert!(svc.launch("https://www.baidu.com", None).await.is_ok());
        assert!(svc.launch("https://www.baidu.com", Some("msedge")).await.is_ok());
    }

    #[tokio::test]
    async fn check_tool_terminal_always_true() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        assert!(svc.check_tool_installed(ToolType::Terminal).await);
    }

    #[tokio::test]
    async fn check_tool_explorer_always_true() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        assert!(svc.check_tool_installed(ToolType::Explorer).await);
    }

    #[tokio::test]
    async fn open_file_fails_for_missing_file() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        let result = svc.open_file("/nonexistent/file.txt").await;
        assert!(matches!(result, Err(ShellError::FileNotFound(_))));
    }

    #[tokio::test]
    async fn show_item_in_folder_fails_for_missing_path() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        let result = svc.show_item_in_folder("/nonexistent/path").await;
        assert!(matches!(result, Err(ShellError::FileNotFound(_))));
    }

    #[tokio::test]
    async fn open_external_fails_for_invalid_url() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        let result = svc.open_external("; rm -rf /").await;
        assert!(matches!(result, Err(ShellError::InvalidUrl(_))));
    }

    #[tokio::test]
    async fn open_external_fails_for_file_scheme() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        let result = svc.open_external("file:///etc/passwd").await;
        assert!(matches!(result, Err(ShellError::InvalidUrl(_))));
    }

    #[tokio::test]
    async fn open_folder_with_fails_for_missing_dir() {
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        let result = svc.open_folder_with("/nonexistent/dir", ToolType::Explorer).await;
        assert!(matches!(result, Err(ShellError::DirectoryNotFound(_))));
    }

    #[tokio::test]
    async fn open_folder_with_fails_for_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "data").unwrap();
        let svc = ShellService::new(Arc::new(NoopSystemOpener));
        let result = svc
            .open_folder_with(file_path.to_str().unwrap(), ToolType::Explorer)
            .await;
        assert!(matches!(result, Err(ShellError::DirectoryNotFound(_))));
    }

    #[test]
    fn linux_file_manager_candidates_include_common_fallbacks() {
        let candidates = linux_file_manager_candidates("/tmp/project");
        let programs: Vec<&str> = candidates.iter().map(|(program, _)| *program).collect();

        for expected in ["xdg-open", "gio", "kde-open", "exo-open"] {
            assert!(
                programs.contains(&expected),
                "missing file manager candidate {expected}"
            );
        }

        assert!(
            candidates
                .iter()
                .any(|(program, args)| *program == "gio" && args == &vec!["open".to_owned(), "/tmp/project".to_owned()]),
            "gio fallback must use the open subcommand"
        );
    }

    #[test]
    fn linux_terminal_candidates_cover_common_desktops_and_generic_fallbacks() {
        let candidates = linux_terminal_candidates("/tmp/project");
        let programs: Vec<&str> = candidates.iter().map(|(program, _)| *program).collect();

        for expected in [
            "gnome-terminal",
            "kgx",
            "konsole",
            "xfce4-terminal",
            "kitty",
            "wezterm",
            "x-terminal-emulator",
            "xterm",
        ] {
            assert!(
                programs.contains(&expected),
                "missing terminal candidate {expected}"
            );
        }

        assert!(
            candidates.iter().any(|(program, args)| *program == "x-terminal-emulator"
                && args.iter().any(|arg| arg == "/tmp/project")),
            "generic terminal fallback should carry the working directory as a discrete argv"
        );
    }
}
