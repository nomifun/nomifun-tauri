//! Startup-time PATH enhancement.
//!
//! Call [`enhance_process_path`] from `main()` **before any worker thread
//! is spawned** (including the tokio runtime). It rewrites
//! `std::env::var("PATH")` to include:
//!
//! 1. The bundled bun directory (highest priority).
//! 2. Platform extra bins (`~/.bun/bin`, `~/.cargo/bin`, homebrew,
//!    asdf/mise/fnm, env-var-driven roots like `PNPM_HOME`, …).
//! 3. The current `PATH` (inherited from the launching process).
//! 4. The **interactive** login-shell `PATH` (Unix only, 5s timeout) —
//!    sources `~/.zshrc` / `~/.bashrc` in addition to the login files, so
//!    toolchain dirs added there (nvm/fnm/pnpm/asdf/mise, custom npm
//!    prefixes) are visible. Fixes launchd / Finder / systemd-service
//!    starts where the inherited PATH is minimal.
//!
//! After this runs, all downstream `which::which(...)` and
//! `Command::new(...)` calls see the enhanced PATH with zero further
//! wiring.

use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::Duration;

/// Enhance the current process's `PATH`. Returns the merged PATH string
/// for logging/debugging.
///
/// # Safety
///
/// Must be called **before** any other thread exists (including the
/// tokio runtime). Internally calls `std::env::set_var` which is
/// `unsafe` on Rust 2024.
pub unsafe fn enhance_process_path() -> String {
    let current = std::env::var("PATH").unwrap_or_default();
    let login = login_shell_path();
    let extras = platform_extra_bins();
    let bun_dir = crate::bun_bin_dir();

    let merged = merge_paths(bun_dir.as_deref(), &extras, &current, login.as_deref());

    if merged == current {
        tracing::warn!("PATH enhancement produced no changes; continuing with inherited PATH");
    } else {
        tracing::info!(
            login = login.is_some(),
            extra_bin_count = extras.len(),
            bun_bundled = bun_dir.is_some(),
            original_len = current.len(),
            merged_len = merged.len(),
            "PATH enhanced at startup"
        );
    }

    // SAFETY: caller guarantees single-threaded precondition.
    unsafe {
        std::env::set_var("PATH", &merged);
    }
    merged
}

// Placeholder helpers — filled in by later tasks.

fn merge_paths(bun_dir: Option<&Path>, extras: &[PathBuf], current: &str, login: Option<&str>) -> String {
    // Order: bun_dir, extras, current, login. First-occurrence wins.
    // `env::split_paths` and `env::join_paths` honour the OS-specific
    // separator (':' on Unix, ';' on Windows) and handle quoting.
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut parts: Vec<PathBuf> = Vec::new();

    let mut push = |p: PathBuf| {
        if p.as_os_str().is_empty() {
            return;
        }
        if seen.insert(p.clone()) {
            parts.push(p);
        }
    };

    if let Some(p) = bun_dir {
        push(p.to_path_buf());
    }
    for p in extras {
        push(p.clone());
    }
    for p in std::env::split_paths(current) {
        push(p);
    }
    if let Some(l) = login {
        for p in std::env::split_paths(l) {
            push(p);
        }
    }

    std::env::join_paths(&parts)
        .map(|os| os.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn platform_extra_bins() -> Vec<PathBuf> {
    let mut out = platform_extra_bins_at(dirs::home_dir().as_deref());
    // Env-var-driven install locations. Kept out of `platform_extra_bins_at`
    // so that function stays a pure function of `home` for unit tests; the
    // real env is only read here.
    out.extend(env_driven_bins(|k| std::env::var(k).ok()));
    out
}

/// Resolve toolchain bin dirs from explicit env vars a user may have set
/// to relocate a package manager's install root (e.g. `PNPM_HOME`,
/// `NPM_CONFIG_PREFIX`). `get` returns the raw value of an env var, or
/// `None` if unset — injectable so the resolution logic is unit-testable
/// without mutating the process environment. Only directories that exist
/// on disk are returned.
fn env_driven_bins<F>(get: F) -> Vec<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    // (env var, subdir appended to its value). Empty subdir => the value
    // is already the bin dir.
    const SPECS: &[(&str, &str)] = &[
        ("PNPM_HOME", ""),            // pnpm global bin (its own dir)
        ("NPM_CONFIG_PREFIX", "bin"), // custom `npm config set prefix`
        ("BUN_INSTALL", "bin"),       // bun install root
        ("VOLTA_HOME", "bin"),        // volta
        ("DENO_INSTALL", "bin"),      // deno
        ("N_PREFIX", "bin"),          // `n` node version manager
        ("CODEX_INSTALL_DIR", ""),    // OpenAI Codex standalone visible command dir
    ];

    let mut out: Vec<PathBuf> = Vec::new();
    for (var, sub) in SPECS {
        let Some(raw) = get(var) else { continue };
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let dir = if sub.is_empty() {
            PathBuf::from(raw)
        } else {
            PathBuf::from(raw).join(sub)
        };
        if dir.is_dir() {
            out.push(dir);
        }
    }
    out
}

fn platform_extra_bins_at(home: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push_if_dir = |p: PathBuf| {
        if p.is_dir() {
            out.push(p);
        }
    };

    if let Some(h) = home {
        push_if_dir(h.join(".bun").join("bin"));
        push_if_dir(h.join(".cargo").join("bin"));
        push_if_dir(h.join("go").join("bin"));
        push_if_dir(h.join(".deno").join("bin"));
        push_if_dir(h.join(".local").join("bin"));
        push_if_dir(h.join(".volta").join("bin"));
        // Legacy Claude Code local installs used ~/.claude/local instead of
        // the newer native ~/.local/bin command location.
        push_if_dir(h.join(".claude").join("local"));
        // Custom npm global prefixes (`npm config set prefix …`) and other
        // common per-user node install roots.
        push_if_dir(h.join(".npm-global").join("bin"));
        push_if_dir(h.join(".npm-packages").join("bin"));
        push_if_dir(h.join(".node").join("bin"));
        // Version-manager shim dirs (asdf, mise/rtx).
        push_if_dir(h.join(".asdf").join("shims"));
        push_if_dir(h.join(".local").join("share").join("mise").join("shims"));
        // pnpm global bin default locations (Linux XDG vs macOS).
        push_if_dir(h.join(".local").join("share").join("pnpm"));
        push_if_dir(h.join("Library").join("pnpm"));
        for nvm_bin in nvm_version_bins(h) {
            push_if_dir(nvm_bin);
        }
        for fnm_bin in fnm_version_bins(h) {
            push_if_dir(fnm_bin);
        }
    }

    #[cfg(unix)]
    {
        // Homebrew on Apple Silicon (`/opt/homebrew/bin`) is NOT on the
        // minimal PATH a GUI launch inherits, and `/usr/local` is where
        // many CLIs (claude/codex via npm/brew or official installers)
        // land. Cheap to probe; `push_if_dir` drops the ones absent.
        push_if_dir(PathBuf::from("/opt/homebrew/bin"));
        push_if_dir(PathBuf::from("/opt/homebrew/sbin"));
        push_if_dir(PathBuf::from("/usr/local/bin"));
        push_if_dir(PathBuf::from("/usr/local/sbin"));
    }

    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            push_if_dir(PathBuf::from(&appdata).join("npm"));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            push_if_dir(PathBuf::from(&local).join("pnpm"));
            push_if_dir(PathBuf::from(&local).join("fnm_multishells"));
            push_if_dir(
                PathBuf::from(&local)
                    .join("Programs")
                    .join("OpenAI")
                    .join("Codex")
                    .join("bin"),
            );
            // winget package shims (stable since App Installer 1.4).
            push_if_dir(PathBuf::from(&local).join("Microsoft").join("WinGet").join("Links"));
            // Yarn classic global bin.
            push_if_dir(PathBuf::from(&local).join("Yarn").join("bin"));
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            push_if_dir(PathBuf::from(&pf).join("Git").join("cmd"));
            push_if_dir(PathBuf::from(&pf).join("Git").join("bin"));
            push_if_dir(PathBuf::from(&pf).join("nodejs"));
        }
        if let Ok(pf86) = std::env::var("ProgramFiles(x86)") {
            push_if_dir(PathBuf::from(&pf86).join("nodejs"));
        }
        if let Ok(scoop) = std::env::var("SCOOP") {
            push_if_dir(PathBuf::from(&scoop).join("shims"));
        } else if let Some(h) = home {
            push_if_dir(h.join("scoop").join("shims"));
        }
    }

    out
}

fn nvm_version_bins(home: &Path) -> Vec<PathBuf> {
    let versions_dir = home.join(".nvm").join("versions").join("node");
    let Ok(entries) = std::fs::read_dir(&versions_dir) else {
        return Vec::new();
    };

    let mut bins: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("bin"))
        .filter(|bin| bin.is_dir())
        .collect();

    // Prefer newer-looking versions first, matching the user's active
    // Node installation ahead of older fallbacks when multiple bins exist.
    bins.sort_by(|a, b| b.cmp(a));
    bins
}

/// fnm installs each Node version under
/// `<data>/fnm/node-versions/<ver>/installation/bin`. The per-shell
/// `fnm_multishells` symlinks are ephemeral, so we walk the stable
/// version dirs instead. We probe the common data roots (Linux XDG,
/// macOS Application Support, and `~/.fnm`).
fn fnm_version_bins(home: &Path) -> Vec<PathBuf> {
    let roots = [
        home.join(".local").join("share").join("fnm").join("node-versions"),
        home.join("Library")
            .join("Application Support")
            .join("fnm")
            .join("node-versions"),
        home.join(".fnm").join("node-versions"),
    ];

    let mut bins: Vec<PathBuf> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let bin = entry.path().join("installation").join("bin");
            if bin.is_dir() {
                bins.push(bin);
            }
        }
    }
    // Newer-looking versions first, mirroring nvm handling above.
    bins.sort_by(|a, b| b.cmp(a));
    bins
}

/// Markers wrapped around the probed `$PATH` so we can extract it
/// cleanly even when an interactive shell's startup files print banners,
/// version notices, or prompt escapes to stdout. Without the markers,
/// any such noise would be mistaken for PATH segments.
#[cfg(unix)]
const PATH_PROBE_BEGIN: &str = "__NOMIFUN_PATH_BEGIN__";
#[cfg(unix)]
const PATH_PROBE_END: &str = "__NOMIFUN_PATH_END__";

/// Shell snippet that prints the live `$PATH` wrapped in our markers.
#[cfg(unix)]
const PATH_PROBE_SNIPPET: &str = "printf '__NOMIFUN_PATH_BEGIN__%s__NOMIFUN_PATH_END__' \"$PATH\"";

/// How long to wait for the login-shell probe before giving up. An
/// interactive shell sources the user's full startup files (oh-my-zsh,
/// nvm, conda init, …), which can take a second or two on a heavily
/// customized setup, so we allow more headroom than a bare command needs.
#[cfg(unix)]
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);

/// Pull the `$PATH` value out of the probe's stdout, ignoring any
/// surrounding noise emitted by the shell's startup files. Returns
/// `None` when the markers are absent or the captured value is empty.
#[cfg(unix)]
fn extract_probe_path(raw: &str) -> Option<String> {
    let start = raw.find(PATH_PROBE_BEGIN)? + PATH_PROBE_BEGIN.len();
    let end_rel = raw[start..].find(PATH_PROBE_END)?;
    let path = raw[start..start + end_rel].trim();
    if path.is_empty() {
        None
    } else {
        Some(path.to_owned())
    }
}

#[cfg(unix)]
fn login_shell_path() -> Option<String> {
    let shell = std::env::var("SHELL").ok()?;
    if !Path::new(&shell).is_absolute() {
        tracing::debug!(%shell, "SHELL is not absolute, skipping login shell probe");
        return None;
    }
    run_login_shell_path(&shell, None)
}

/// Spawn `shell` as an **interactive login** shell and capture the
/// `$PATH` it exports.
///
/// `-i` (interactive) is essential: most users add their toolchain dirs
/// (nvm / fnm / pnpm / asdf / mise, custom npm prefixes, manual
/// `export PATH=…`) in `~/.zshrc` / `~/.bashrc`, which a *non*-interactive
/// login shell (`-l` only) does NOT source — that gap is why some
/// machines detect no CLI agents at all. `-l` (login) additionally pulls
/// in `~/.zprofile` / `~/.bash_profile`, so `-i -l` is a strict superset
/// of the previous `-l`-only probe.
///
/// `home_override` lets tests point the child at a scratch `$HOME` (and
/// drops `ZDOTDIR` so zsh resolves its rc files under that `$HOME`); in
/// production it is `None` and the real environment is inherited.
#[cfg(unix)]
fn run_login_shell_path(shell: &str, home_override: Option<&Path>) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use wait_timeout::ChildExt;

    let mut cmd = Command::new(shell);
    cmd.args(["-i", "-l", "-c", PATH_PROBE_SNIPPET])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(home) = home_override {
        cmd.env("HOME", home);
        cmd.env_remove("ZDOTDIR");
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(%shell, error = %e, "login shell spawn failed");
            return None;
        }
    };

    // Drain stdout on a dedicated thread. An interactive shell may emit a
    // PATH long enough to fill the pipe buffer (deadlocks a read-after-wait)
    // AND — unlike the old non-interactive probe — may never exit if a
    // startup file blocks. Reading on a separate thread lets the timeout
    // below fire and `kill()` the child, which closes the pipe and unblocks
    // this reader. The thread is always joined before we return, so no
    // worker thread outlives `enhance_process_path`'s `set_var`.
    let mut stdout_handle = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout_handle.read_to_string(&mut buf);
        buf
    });

    let status = match child.wait_timeout(LOGIN_SHELL_TIMEOUT) {
        Ok(Some(s)) => s,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            tracing::warn!("login shell PATH probe timed out");
            return None;
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            tracing::debug!(error = %e, "login shell wait_timeout errored");
            return None;
        }
    };

    let stdout = reader.join().ok()?;

    if !status.success() {
        tracing::debug!(?status, "login shell exited non-zero");
        return None;
    }

    extract_probe_path(&stdout)
}

#[cfg(not(unix))]
fn login_shell_path() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate the process-global `SHELL` env var.
    /// `cargo test` runs test fns on parallel threads; without this lock
    /// one test's `set_var`/`remove_var` races another's read.
    #[cfg(unix)]
    static SHELL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn sep() -> &'static str {
        if cfg!(windows) { ";" } else { ":" }
    }

    #[test]
    fn merge_paths_dedupes_preserve_order() {
        let s = sep();
        let current = format!("/a{s}/b{s}/c");
        let login = format!("/b{s}/d");
        let extras: Vec<PathBuf> = vec![PathBuf::from("/e")];

        let result = merge_paths(None, &extras, &current, Some(&login));
        let parts: Vec<&str> = result.split(s).collect();

        assert_eq!(parts, vec!["/e", "/a", "/b", "/c", "/d"]);
    }

    #[test]
    fn merge_paths_with_bun_dir_at_front() {
        let s = sep();
        let current = format!("/a{s}/b");
        let bun = PathBuf::from("/bun");

        let result = merge_paths(Some(&bun), &[], &current, None);
        let parts: Vec<&str> = result.split(s).collect();

        assert_eq!(parts, vec!["/bun", "/a", "/b"]);
    }

    #[test]
    fn merge_paths_drops_empty_segments() {
        let s = sep();
        let current = format!("{s}/a{s}{s}/b{s}");

        let result = merge_paths(None, &[], &current, None);
        let parts: Vec<&str> = result.split(s).collect();

        assert_eq!(parts, vec!["/a", "/b"]);
    }

    #[test]
    fn merge_paths_all_optional_none() {
        let result = merge_paths(None, &[], "", None);
        assert_eq!(result, "");
    }

    #[test]
    fn merge_paths_bun_dir_deduplicates_if_already_in_current() {
        let s = sep();
        let current = format!("/bun{s}/a");
        let bun = PathBuf::from("/bun");

        let result = merge_paths(Some(&bun), &[], &current, None);
        let parts: Vec<&str> = result.split(s).collect();

        // /bun appears first (from bun_dir), then /a from current.
        // Second /bun (inside current) is dedup'd.
        assert_eq!(parts, vec!["/bun", "/a"]);
    }

    #[test]
    fn platform_extra_bins_at_filters_nonexistent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();

        // 构造少量"存在"的 bin 目录，其他 candidate 仍会被 platform_extra_bins_at
        // 检查但应被过滤掉。
        std::fs::create_dir_all(home.join(".bun/bin")).unwrap();
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(home.join(".nvm/versions/node/v22.22.0/bin")).unwrap();
        std::fs::create_dir_all(home.join(".nvm/versions/node/v25.1.0/bin")).unwrap();

        let bins = platform_extra_bins_at(Some(home));

        // The product builds these tails with Path::join, so the separator is
        // platform-native (backslash on Windows). Match component-wise — a
        // multi-component &str like "a/b" is one component to Path::ends_with and
        // never matches the two-component a\b tail on Windows. Build tails as
        // PathBufs (and normalize separators for the substring check) instead.
        let tail = |segs: &[&str]| segs.iter().collect::<PathBuf>();

        // 至少这两个应出现
        assert!(
            bins.iter().any(|p| p.ends_with(tail(&[".bun", "bin"]))),
            "expected ~/.bun/bin in result"
        );
        assert!(
            bins.iter().any(|p| p.ends_with(tail(&[".cargo", "bin"]))),
            "expected ~/.cargo/bin in result"
        );
        assert!(
            bins.iter()
                .any(|p| p.ends_with(tail(&[".nvm", "versions", "node", "v22.22.0", "bin"]))),
            "expected ~/.nvm/versions/node/v22.22.0/bin in result"
        );
        assert!(
            bins.iter()
                .any(|p| p.ends_with(tail(&[".nvm", "versions", "node", "v25.1.0", "bin"]))),
            "expected ~/.nvm/versions/node/v25.1.0/bin in result"
        );
        let nvm_bins: Vec<_> = bins
            .iter()
            .filter(|p| {
                p.to_string_lossy()
                    .replace('\\', "/")
                    .contains(".nvm/versions/node/")
            })
            .collect();
        assert_eq!(nvm_bins.len(), 2);
        assert!(
            nvm_bins[0].ends_with(tail(&[".nvm", "versions", "node", "v25.1.0", "bin"])),
            "expected newer NVM bin first"
        );
        assert!(
            nvm_bins[1].ends_with(tail(&[".nvm", "versions", "node", "v22.22.0", "bin"])),
            "expected older NVM bin second"
        );

        // 没创建的目录不应出现
        assert!(!bins.iter().any(|p| p.ends_with(tail(&["go", "bin"]))));
        assert!(!bins.iter().any(|p| p.ends_with(tail(&[".deno", "bin"]))));
    }

    #[test]
    fn platform_extra_bins_at_handles_no_home() {
        let bins = platform_extra_bins_at(None);
        // 没 home 时，Unix 返回空；Windows 可能仍从 env 读到 APPDATA 等——两种都可接受。
        // 只验证不 panic。
        let _ = bins;
    }

    #[test]
    fn platform_extra_bins_at_includes_common_node_tool_dirs() {
        // Defense-in-depth: tools that install CLIs (claude/codex) into dirs
        // these managers own. If the interactive-shell probe somehow fails,
        // these still let detection succeed.
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".npm-global/bin")).unwrap();
        std::fs::create_dir_all(home.join(".asdf/shims")).unwrap();
        std::fs::create_dir_all(home.join(".local/share/mise/shims")).unwrap();
        // fnm installs node under <root>/node-versions/<ver>/installation/bin.
        std::fs::create_dir_all(home.join(".local/share/fnm/node-versions/v20.11.0/installation/bin")).unwrap();

        let bins = platform_extra_bins_at(Some(home));

        assert!(
            bins.iter().any(|p| p.ends_with(".npm-global/bin")),
            "expected ~/.npm-global/bin in {bins:?}"
        );
        assert!(
            bins.iter().any(|p| p.ends_with(".asdf/shims")),
            "expected ~/.asdf/shims in {bins:?}"
        );
        assert!(
            bins.iter().any(|p| p.ends_with("mise/shims")),
            "expected mise shims in {bins:?}"
        );
        assert!(
            bins.iter().any(|p| p.ends_with("node-versions/v20.11.0/installation/bin")),
            "expected fnm node bin in {bins:?}"
        );
    }

    #[test]
    fn env_driven_bins_resolves_subdir_specs_and_filters_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let pnpm = tmp.path().join("pnpm-home");
        std::fs::create_dir_all(&pnpm).unwrap();
        let bun_root = tmp.path().join("bun");
        std::fs::create_dir_all(bun_root.join("bin")).unwrap();

        let pnpm_s = pnpm.to_string_lossy().into_owned();
        let bun_s = bun_root.to_string_lossy().into_owned();
        let bins = env_driven_bins(|k| match k {
            // PNPM_HOME is itself the bin dir (no subdir appended).
            "PNPM_HOME" => Some(pnpm_s.clone()),
            // BUN_INSTALL points at a root; the bin dir is <root>/bin.
            "BUN_INSTALL" => Some(bun_s.clone()),
            // Points at a non-existent dir: must be filtered out.
            "VOLTA_HOME" => Some(tmp.path().join("nope").to_string_lossy().into_owned()),
            _ => None,
        });

        assert!(bins.contains(&pnpm), "PNPM_HOME (no subdir) should be included: {bins:?}");
        assert!(
            bins.contains(&bun_root.join("bin")),
            "BUN_INSTALL/bin should be included: {bins:?}"
        );
        assert!(
            !bins.iter().any(|p| p.ends_with("nope/bin")),
            "non-existent VOLTA_HOME/bin must be filtered: {bins:?}"
        );
    }

    #[test]
    fn env_driven_bins_resolves_codex_install_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let codex_bin = tmp.path().join("codex-bin");
        std::fs::create_dir_all(&codex_bin).unwrap();

        let codex_s = codex_bin.to_string_lossy().into_owned();
        let bins = env_driven_bins(|k| match k {
            "CODEX_INSTALL_DIR" => Some(codex_s.clone()),
            _ => None,
        });

        assert!(
            bins.contains(&codex_bin),
            "CODEX_INSTALL_DIR should be treated as the visible codex command directory: {bins:?}"
        );
    }

    #[test]
    fn platform_extra_bins_at_includes_legacy_claude_local_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        let legacy_claude = home.join(".claude").join("local");
        std::fs::create_dir_all(&legacy_claude).unwrap();

        let bins = platform_extra_bins_at(Some(home));

        assert!(
            bins.contains(&legacy_claude),
            "legacy Claude local installer directory should be included: {bins:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_path_returns_none_without_shell_var() {
        let _guard = SHELL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: SHELL_ENV_LOCK serializes SHELL mutations across tests.
        unsafe {
            std::env::remove_var("SHELL");
        }
        let result = login_shell_path();
        assert!(result.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_path_rejects_relative_shell() {
        let _guard = SHELL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: SHELL_ENV_LOCK serializes SHELL mutations across tests.
        unsafe {
            std::env::set_var("SHELL", "sh");
        }
        let result = login_shell_path();
        assert!(result.is_none());
        unsafe {
            std::env::remove_var("SHELL");
        }
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_path_roundtrip_with_sh() {
        let _guard = SHELL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: SHELL_ENV_LOCK serializes SHELL mutations across tests.
        unsafe {
            std::env::set_var("SHELL", "/bin/sh");
        }
        let result = login_shell_path();
        assert!(result.is_some(), "login shell probe should return Some");
        let path = result.unwrap();
        assert!(!path.is_empty(), "login shell PATH should not be empty");
        unsafe {
            std::env::remove_var("SHELL");
        }
    }

    #[cfg(unix)]
    #[test]
    fn extract_probe_path_pulls_value_between_markers() {
        let raw = format!("{PATH_PROBE_BEGIN}/usr/bin:/bin{PATH_PROBE_END}");
        assert_eq!(extract_probe_path(&raw).as_deref(), Some("/usr/bin:/bin"));
    }

    #[cfg(unix)]
    #[test]
    fn extract_probe_path_ignores_surrounding_shell_noise() {
        // Interactive startup files (oh-my-zsh banner, nvm notice, p10k
        // preamble) can print before/after our markers — must be stripped.
        let raw = format!(
            "Welcome banner\noh-my-zsh updated\n{PATH_PROBE_BEGIN}/opt/homebrew/bin:/usr/bin{PATH_PROBE_END}\n% "
        );
        assert_eq!(
            extract_probe_path(&raw).as_deref(),
            Some("/opt/homebrew/bin:/usr/bin")
        );
    }

    #[cfg(unix)]
    #[test]
    fn extract_probe_path_none_without_markers() {
        assert_eq!(extract_probe_path("/usr/bin:/bin"), None);
    }

    #[cfg(unix)]
    #[test]
    fn extract_probe_path_none_when_value_empty() {
        let raw = format!("{PATH_PROBE_BEGIN}{PATH_PROBE_END}");
        assert_eq!(extract_probe_path(&raw), None);
    }

    #[cfg(unix)]
    #[test]
    fn run_login_shell_path_sources_interactive_rc() {
        // Regression test for the "only nomi shows up" bug: a *non*-interactive
        // login shell (`-l`) does NOT source ~/.zshrc, where most users add
        // their CLI dirs (nvm/fnm/pnpm/asdf/mise/custom npm prefixes). The
        // probe must use an *interactive* login shell (`-i -l`) so PATH
        // entries from ~/.zshrc are visible — otherwise claude/codex go
        // undetected and only the internal `nomi` agent shows up.
        let zsh = Path::new("/bin/zsh");
        if !zsh.exists() {
            eprintln!("skipping run_login_shell_path_sources_interactive_rc: /bin/zsh absent");
            return;
        }
        let home = tempfile::TempDir::new().unwrap();
        let marker = home.path().join("nomimarker-bin");
        std::fs::create_dir_all(&marker).unwrap();
        // ~/.zshrc is sourced for INTERACTIVE shells only.
        std::fs::write(
            home.path().join(".zshrc"),
            format!("export PATH=\"{}:$PATH\"\n", marker.display()),
        )
        .unwrap();

        let path = run_login_shell_path("/bin/zsh", Some(home.path()))
            .expect("interactive login shell probe should return a PATH");
        let marker_str = marker.to_string_lossy();
        assert!(
            path.split(':').any(|p| p == marker_str),
            "expected ~/.zshrc PATH entry {marker_str} in probed PATH, got: {path}"
        );
    }
}
