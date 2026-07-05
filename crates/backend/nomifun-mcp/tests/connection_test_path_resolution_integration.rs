//! Isolated PATH-resolution coverage for stdio MCP connection tests.
//!
//! This file intentionally contains one test because it mutates process PATH
//! to model the startup-enhanced GUI environment.

#![cfg(unix)]

use std::collections::HashMap;

use nomifun_mcp::{McpConnectionTestService, McpServerTransport};

#[tokio::test]
async fn stdio_npx_resolves_from_enhanced_process_path() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::TempDir::new().unwrap();
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    let fake_npx = bin_dir.join("npx");
    std::fs::write(
        &fake_npx,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":"1"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":"1","result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fake-npx","version":"1.0.0"}}}'
      ;;
    *'"id":"2"'*)
      printf '%s\n' '{"jsonrpc":"2.0","id":"2","result":{"tools":[]}}'
      exit 0
      ;;
  esac
done
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fake_npx).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_npx, perms).unwrap();

    let original_path = std::env::var_os("PATH");
    unsafe {
        std::env::set_var("PATH", &bin_dir);
    }

    let svc = McpConnectionTestService::new(reqwest::Client::new());
    let transport = McpServerTransport::Stdio {
        command: "npx".into(),
        args: vec![],
        env: HashMap::new(),
    };

    let result = svc.test_connection("fake-npx", &transport).await;

    unsafe {
        if let Some(path) = original_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
    }

    assert!(result.success, "expected fake npx MCP server to connect: {result:?}");
    assert!(result.tools.unwrap().is_empty());
}
