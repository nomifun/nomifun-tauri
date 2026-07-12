use std::{
    fs,
    path::{Path, PathBuf},
};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("nomi-execution should live three levels below the workspace root")
        .to_path_buf()
}

fn read_workspace(path: impl AsRef<Path>) -> String {
    let path = workspace_root().join(path);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn blank_non_newline(bytes: &mut [u8], start: usize, end: usize) {
    for byte in &mut bytes[start..end] {
        if *byte != b'\n' && *byte != b'\r' {
            *byte = b' ';
        }
    }
}

fn quoted_end(bytes: &[u8], quote: usize) -> usize {
    let mut cursor = quote + 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor = (cursor + 2).min(bytes.len());
        } else if bytes[cursor] == b'"' {
            return cursor + 1;
        } else {
            cursor += 1;
        }
    }
    bytes.len()
}

fn char_literal_end(bytes: &[u8], quote: usize) -> Option<usize> {
    let mut cursor = quote + 1;
    if bytes.get(cursor) == Some(&b'\\') {
        cursor += 1;
        match bytes.get(cursor).copied()? {
            b'n' | b'r' | b't' | b'0' | b'\'' | b'"' | b'\\' => cursor += 1,
            b'x' => cursor += 3,
            b'u' if bytes.get(cursor + 1) == Some(&b'{') => {
                cursor += 2;
                while cursor < bytes.len() && bytes[cursor] != b'}' {
                    cursor += 1;
                }
                cursor += 1;
            }
            _ => return None,
        }
    } else {
        let text = std::str::from_utf8(&bytes[cursor..]).ok()?;
        let character = text.chars().next()?;
        if character == '\'' || character == '\r' || character == '\n' {
            return None;
        }
        cursor += character.len_utf8();
    }
    (bytes.get(cursor) == Some(&b'\'')).then_some(cursor + 1)
}

fn raw_string_end(bytes: &[u8], index: usize) -> Option<usize> {
    let mut cursor = index;
    if bytes.get(cursor) == Some(&b'b') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'r') {
        return None;
    }
    cursor += 1;
    let mut hashes = 0;
    while bytes.get(cursor) == Some(&b'#') {
        hashes += 1;
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'"') {
        return None;
    }
    cursor += 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'"'
            && bytes
                .get(cursor + 1..cursor + 1 + hashes)
                .is_some_and(|suffix| suffix.iter().all(|byte| *byte == b'#'))
        {
            return Some(cursor + 1 + hashes);
        }
        cursor += 1;
    }
    Some(bytes.len())
}

/// Preserve byte offsets and line breaks while hiding comments and literals.
///
/// Architecture checks inspect Rust identifiers, not prose. Keeping offsets
/// stable also lets `production_source` remove a complete cfg(test) item without
/// truncating production declarations that follow an earlier test-only field.
fn rust_code_mask(source: &str) -> String {
    let input = source.as_bytes();
    let mut output = input.to_vec();
    let mut index = 0;
    while index < input.len() {
        if input.get(index..index + 2) == Some(b"//") {
            let end = input[index + 2..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(input.len(), |offset| index + 2 + offset);
            blank_non_newline(&mut output, index, end);
            index = end;
            continue;
        }
        if input.get(index..index + 2) == Some(b"/*") {
            let mut cursor = index + 2;
            let mut depth = 1usize;
            while cursor < input.len() && depth > 0 {
                if input.get(cursor..cursor + 2) == Some(b"/*") {
                    depth += 1;
                    cursor += 2;
                } else if input.get(cursor..cursor + 2) == Some(b"*/") {
                    depth -= 1;
                    cursor += 2;
                } else {
                    cursor += 1;
                }
            }
            blank_non_newline(&mut output, index, cursor);
            index = cursor;
            continue;
        }
        if let Some(end) = raw_string_end(input, index) {
            blank_non_newline(&mut output, index, end);
            index = end;
            continue;
        }
        let quote = if input[index] == b'"' {
            Some(index)
        } else if input.get(index..index + 2) == Some(b"b\"") {
            Some(index + 1)
        } else {
            None
        };
        if let Some(quote) = quote {
            let end = quoted_end(input, quote);
            blank_non_newline(&mut output, index, end);
            index = end;
            continue;
        }
        if input[index] == b'\''
            && index
                .checked_sub(1)
                .and_then(|previous| input.get(previous))
                .is_none_or(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_')
            && let Some(end) = char_literal_end(input, index)
        {
            blank_non_newline(&mut output, index, end);
            index = end;
            continue;
        }
        index += 1;
    }
    String::from_utf8(output).expect("masking Rust source must preserve UTF-8")
}

fn skip_space(bytes: &[u8], mut index: usize) -> usize {
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        index += 1;
    }
    index
}

fn attribute_end(bytes: &[u8], index: usize) -> Option<usize> {
    if bytes.get(index) != Some(&b'#') || bytes.get(index + 1) != Some(&b'[') {
        return None;
    }
    let mut depth = 1usize;
    let mut cursor = index + 2;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(cursor + 1);
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    Some(bytes.len())
}

fn matching_brace(bytes: &[u8], open: usize) -> usize {
    let mut depth = 1usize;
    let mut cursor = open + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return cursor + 1;
                }
            }
            _ => {}
        }
        cursor += 1;
    }
    bytes.len()
}

fn attributed_item_end(bytes: &[u8], mut index: usize) -> usize {
    index = skip_space(bytes, index);
    while bytes.get(index) == Some(&b'#') {
        let Some(end) = attribute_end(bytes, index) else {
            break;
        };
        index = skip_space(bytes, end);
    }

    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    let mut cursor = index;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'(' => paren_depth += 1,
            b')' => paren_depth = paren_depth.saturating_sub(1),
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b';' | b',' if paren_depth == 0 && bracket_depth == 0 => {
                return cursor + 1;
            }
            b'{' if paren_depth == 0 && bracket_depth == 0 => {
                return matching_brace(bytes, cursor);
            }
            _ => {}
        }
        cursor += 1;
    }
    bytes.len()
}

fn split_top_level_arguments(source: &[u8]) -> Vec<&[u8]> {
    let mut arguments = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (index, byte) in source.iter().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                arguments.push(&source[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    arguments.push(&source[start..]);
    arguments
}

fn is_test_only_cfg_attribute(attribute: &[u8]) -> bool {
    let compact = attribute
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    if compact == b"#[cfg(test)]" {
        return true;
    }
    const PREFIX: &[u8] = b"#[cfg(all(";
    const SUFFIX: &[u8] = b"))]";
    if !compact.starts_with(PREFIX) || !compact.ends_with(SUFFIX) {
        return false;
    }
    let arguments = &compact[PREFIX.len()..compact.len() - SUFFIX.len()];
    split_top_level_arguments(arguments)
        .into_iter()
        .any(|argument| argument == b"test")
}

fn production_source(path: impl AsRef<Path>) -> String {
    production_source_text(&read_workspace(path))
}

fn production_source_text(source: &str) -> String {
    let masked = rust_code_mask(source);
    let input = masked.as_bytes();
    let mut output = input.to_vec();
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'#' || input.get(index + 1) != Some(&b'[') {
            index += 1;
            continue;
        }
        let Some(attribute_end) = attribute_end(input, index) else {
            break;
        };
        let attribute = &input[index..attribute_end];
        if is_test_only_cfg_attribute(attribute) {
            let item_end = attributed_item_end(input, attribute_end);
            blank_non_newline(&mut output, index, item_end);
            index = item_end;
        } else {
            index = attribute_end;
        }
    }
    String::from_utf8(output).expect("removing cfg(test) items must preserve UTF-8")
}

fn without_whitespace(source: &str) -> String {
    source.chars().filter(|char| !char.is_whitespace()).collect()
}

fn count(source: &str, needle: &str) -> usize {
    source.match_indices(needle).count()
}

fn dependency_sections(manifest: &str, dependency: &str) -> Vec<String> {
    let mut section = String::new();
    let mut matches = Vec::new();
    for raw_line in manifest.lines() {
        let line = raw_line.split('#').next().unwrap_or_default().trim();
        if let Some(header) = line.strip_prefix('[').and_then(|line| line.strip_suffix(']')) {
            section = header.trim().to_owned();
            continue;
        }
        let Some(rest) = line.strip_prefix(dependency) else {
            continue;
        };
        if rest.starts_with(char::is_whitespace)
            || rest.starts_with('=')
            || rest.starts_with(".workspace")
        {
            matches.push(section.clone());
        }
    }
    matches
}

fn visit_rust_files(root: &Path, files: &mut Vec<PathBuf>) {
    for entry in
        fs::read_dir(root).unwrap_or_else(|error| panic!("read {}: {error}", root.display()))
    {
        let path = entry.expect("read directory entry").path();
        if path.is_dir() {
            visit_rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn production_source_mask_preserves_real_code_after_test_items_and_cfg_attr() {
    let source = r#"
// 非 ASCII 注释不得改变后续字节偏移。
#[cfg(test)]
mod tests {
    fn fake() {
        let closing = '}';
        libc::kill(-1, 9);
    }
}
#[cfg_attr(test, allow(dead_code))]
fn production() { libc::kill(-2, 9); }
#[cfg(not(test))]
fn production_cfg() { libc::kill(-3, 9); }
#[cfg(test)]
const TEST_ONLY: bool = 1 < 2;
#[cfg(all(target_os = "linux", test))]
fn test_late() { libc::kill(-5, 9); }
fn after_comparison() { libc::kill(-4, 9); }
fn lifetime<'a>() {
    libc::kill(-6, 9);
    let _: &'a str;
}
"#;
    let production = without_whitespace(&production_source_text(source));
    assert!(!production.contains("libc::kill(-1,9)"));
    assert!(production.contains("libc::kill(-2,9)"));
    assert!(production.contains("libc::kill(-3,9)"));
    assert!(!production.contains("constTEST_ONLY"));
    assert!(!production.contains("libc::kill(-5,9)"));
    assert!(production.contains("libc::kill(-4,9)"));
    assert!(production.contains("libc::kill(-6,9)"));
}

#[test]
fn execution_crate_is_registered_and_backend_neutral() {
    let workspace = read_workspace("Cargo.toml");
    let workspace_compact = without_whitespace(&workspace);
    assert!(
        workspace_compact.contains(
            "nomi-execution={path=\"crates/shared/nomi-execution\"}"
        ),
        "workspace dependencies must register nomi-execution"
    );
    assert!(
        workspace_compact.contains("members=[\"crates/agent/*\",\"crates/backend/*\",\"crates/shared/*\""),
        "workspace members must include the shared crates directory"
    );

    let manifest = read_workspace("crates/shared/nomi-execution/Cargo.toml");
    let forbidden = [
        "nomifun-",
        "nomi-types",
        "nomi-agent",
        "nomi-tools",
        "rusqlite",
        "sqlx",
        "tauri",
    ];
    let present = forbidden
        .into_iter()
        .filter(|dependency| manifest.contains(dependency))
        .collect::<Vec<_>>();
    assert!(
        present.is_empty(),
        "nomi-execution must remain backend-neutral: {present:?}"
    );
}

#[test]
fn platform_module_is_private_and_owns_os_process_primitives() {
    let lib = without_whitespace(&production_source(
        "crates/shared/nomi-execution/src/lib.rs",
    ));
    assert!(lib.contains("modplatform;"));
    assert!(!lib.contains("pubmodplatform;"));

    let src = workspace_root().join("crates/shared/nomi-execution/src");
    let platform = src.join("platform");
    let mut files = Vec::new();
    visit_rust_files(&src, &mut files);
    let mut escaped = Vec::new();
    for path in files {
        if path.starts_with(&platform) {
            continue;
        }
        let relative = path
            .strip_prefix(workspace_root())
            .expect("execution source should be inside workspace");
        let source = without_whitespace(&production_source(relative));
        for primitive in [
            "JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE",
            "PR_SET_PDEATHSIG",
            "libc::kqueue(",
            "libc::kill(-",
            "native_pty_system(",
        ] {
            if source.contains(primitive) {
                escaped.push(format!("{}: {primitive}", relative.display()));
            }
        }
    }
    assert!(
        escaped.is_empty(),
        "OS ownership primitives escaped nomi-execution/src/platform: {escaped:?}"
    );
}

#[test]
fn runtime_facade_and_tools_depend_on_the_shared_kernel() {
    let runtime_manifest = without_whitespace(&read_workspace(
        "crates/backend/nomifun-runtime/Cargo.toml",
    ));
    assert!(
        runtime_manifest.contains("nomi-execution.workspace=true"),
        "nomifun-runtime must depend on nomi-execution"
    );
    let tools_manifest =
        without_whitespace(&read_workspace("crates/agent/nomi-tools/Cargo.toml"));
    assert!(
        tools_manifest.contains("nomi-execution.workspace=true"),
        "nomi-tools must depend on nomi-execution"
    );

    let spawn = without_whitespace(&production_source(
        "crates/backend/nomifun-runtime/src/spawn.rs",
    ));
    for delegation in [
        "inner:nomi_execution::CommandBuilder",
        "nomi_execution::CommandBuilder::new(",
        "nomi_execution::CommandBuilder::clean_cli(",
        "nomi_execution::kill_legacy_process_tree(",
    ] {
        assert!(
            spawn.contains(delegation),
            "nomifun-runtime spawn facade must delegate through {delegation}"
        );
    }
    let shell_env = without_whitespace(&production_source(
        "crates/backend/nomifun-runtime/src/shell_env.rs",
    ));
    assert!(
        shell_env.contains("nomi_execution::merge_process_path("),
        "nomifun-runtime PATH merging must delegate to nomi-execution"
    );
}

#[test]
fn legacy_command_adapters_delegate_to_the_shared_supervisor() {
    for path in [
        "crates/agent/nomi-tools/src/bash.rs",
        "crates/agent/nomi-tools/src/exec_command.rs",
        "crates/agent/nomi-tools/src/write_stdin.rs",
    ] {
        let source = without_whitespace(&production_source(path));
        assert!(
            source.contains("ProcessSupervisor"),
            "{path} must delegate execution to ProcessSupervisor"
        );
        for forbidden in [
            "crate::pty",
            "PtyParams",
            "Pty::spawn",
            "MasterPty",
            "ChildKiller",
            "ExecSession",
            "collect_until_deadline",
            "tokio::process::Command",
            ".output(",
        ] {
            assert!(
                !source.contains(forbidden),
                "{path} retained forbidden process path {forbidden:?}"
            );
        }
    }
}

#[test]
fn bootstrap_creates_one_supervisor_and_shares_it_with_all_command_tools() {
    let bootstrap = without_whitespace(&production_source(
        "crates/agent/nomi-agent/src/bootstrap.rs",
    ));
    assert_eq!(
        count(
            &bootstrap,
            "nomi_execution::ProcessSupervisor::new("
        ),
        1,
        "bootstrap must create exactly one ProcessSupervisor"
    );
    for consumer in [
        "nomi_tools::bash::BashTool::new(Arc::clone(&process_supervisor),",
        "nomi_tools::exec_command::ExecCommandTool::new(Arc::clone(&process_supervisor),",
        "nomi_tools::write_stdin::WriteStdinTool::new(Arc::clone(&process_supervisor),",
        "engine.set_process_supervisor(Arc::clone(&process_supervisor))",
    ] {
        assert!(
            bootstrap.contains(consumer),
            "bootstrap must share process_supervisor with {consumer}"
        );
    }
    assert_eq!(
        count(
            &bootstrap,
            "nomi_tools::process_store::ProcessStore::new("
        ),
        1,
        "bootstrap must create one numeric ProcessStore adapter"
    );
    for consumer in [
        "ExecCommandTool::new(Arc::clone(&process_supervisor),Arc::clone(&process_store),",
        "WriteStdinTool::new(Arc::clone(&process_supervisor),Arc::clone(&process_store),",
    ] {
        assert!(
            bootstrap.contains(consumer),
            "exec_command and write_stdin must share the numeric adapter: {consumer}"
        );
    }
}

#[test]
fn native_exec_tools_are_registered_before_mcp_collision_snapshot() {
    let bootstrap = without_whitespace(&production_source(
        "crates/agent/nomi-agent/src/bootstrap.rs",
    ));
    let snapshot = bootstrap
        .find("letbuiltin_names:Vec<String>=registry.tool_names();")
        .expect("bootstrap must snapshot builtin names before MCP registration");

    for native in [
        "nomi_tools::exec_command::ExecCommandTool::new(",
        "nomi_tools::write_stdin::WriteStdinTool::new(",
    ] {
        let registration = bootstrap
            .find(native)
            .unwrap_or_else(|| panic!("missing native registration: {native}"));
        assert!(
            registration < snapshot,
            "{native} must be included in the MCP collision snapshot"
        );
    }
}

#[test]
fn legacy_pty_modules_and_portable_pty_are_test_only() {
    let lib_path = "crates/agent/nomi-tools/src/lib.rs";
    let complete_lib = without_whitespace(&rust_code_mask(&read_workspace(lib_path)));
    let production_lib = without_whitespace(&production_source(lib_path));
    for module in ["pty", "persistent_shell"] {
        let declaration = format!("pubmod{module};");
        assert!(
            complete_lib.contains(&declaration),
            "expected compatibility module {module}"
        );
        assert!(
            !production_lib.contains(&declaration),
            "{module} must not be exported in production"
        );
        assert!(
            workspace_root()
                .join(format!("crates/agent/nomi-tools/src/{module}.rs"))
                .is_file(),
            "expected test-only compatibility source {module}.rs"
        );
    }

    let manifest = read_workspace("crates/agent/nomi-tools/Cargo.toml");
    let sections = dependency_sections(&manifest, "portable-pty");
    assert!(
        !sections.is_empty(),
        "test-only compatibility modules require portable-pty"
    );
    assert!(
        sections
            .iter()
            .all(|section| section == "dev-dependencies" || section.ends_with(".dev-dependencies")),
        "portable-pty must appear only in nomi-tools dev-dependencies, found {sections:?}"
    );
}

#[test]
fn numeric_process_store_contains_metadata_but_no_process_owner() {
    let path = "crates/agent/nomi-tools/src/process_store.rs";
    let store = without_whitespace(&production_source(path));
    for required in [
        "ExecutionOwner",
        "SessionId",
        "OutputCursor",
        "Transport",
    ] {
        assert!(
            store.contains(required),
            "ProcessStore must retain {required} metadata"
        );
    }
    for forbidden in [
        "crate::pty",
        "PtyParams",
        "Pty::spawn",
        "Arc<Pty>",
        "MasterPty",
        "ChildKiller",
        "ExecSession",
        "std::process::Child",
        "tokio::process::Child",
    ] {
        assert!(
            !store.contains(forbidden),
            "ProcessStore must not own process state: {forbidden:?}"
        );
    }
}
