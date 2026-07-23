//! E2E tests for file operations (/api/fs/*).

use axum::http::StatusCode;
use serde_json::json;
use tower::ServiceExt;

use common::{body_json, build_app, build_app_with_file_roots, json_with_token, setup_and_login};

// ===========================================================================
// Auth guard
// ===========================================================================

#[tokio::test]
async fn fs_endpoints_require_auth() {
    let (app, _services) = build_app().await;
    let endpoints = [
        "/api/fs/dir",
        "/api/fs/list",
        "/api/fs/metadata",
        "/api/fs/read",
        "/api/fs/write",
        "/api/fs/copy",
        "/api/fs/remove",
        "/api/fs/rename",
        "/api/fs/temp",
        "/api/fs/upload",
        "/api/fs/image-base64",
        "/api/fs/fetch-remote-image",
        "/api/fs/zip",
        "/api/fs/zip/cancel",
        "/api/fs/watch/start",
        "/api/fs/watch/stop",
        "/api/fs/watch/stop-all",
        "/api/fs/office-watch/start",
        "/api/fs/office-watch/stop",
        "/api/fs/snapshot/init",
        "/api/fs/snapshot/info",
        "/api/fs/snapshot/compare",
        "/api/fs/snapshot/baseline",
        "/api/fs/snapshot/stage",
        "/api/fs/snapshot/stage-all",
        "/api/fs/snapshot/unstage",
        "/api/fs/snapshot/unstage-all",
        "/api/fs/snapshot/discard",
        "/api/fs/snapshot/reset",
        "/api/fs/snapshot/branches",
        "/api/fs/snapshot/dispose",
    ];

    for uri in endpoints {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{}"#))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "expected 403 for unauthenticated {uri}"
        );
    }
}

// ===========================================================================
// Directory browsing
// ===========================================================================

#[tokio::test]
async fn get_files_by_dir_returns_directory_contents() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("hello.txt"), "world").unwrap();
    std::fs::create_dir(root.join("subdir")).unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/dir",
        json!({
            "dir": root.to_str().unwrap(),
            "root": root.to_str().unwrap()
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);

    let data = json["data"].as_array().unwrap();
    assert!(data.len() >= 2, "should contain file + subdir");

    let names: Vec<&str> = data.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(names.contains(&"hello.txt"));
    assert!(names.contains(&"subdir"));

    // Check directory has isDir=true
    let subdir_entry = data.iter().find(|e| e["name"] == "subdir").unwrap();
    assert_eq!(subdir_entry["is_dir"], true);
    assert_eq!(subdir_entry["is_file"], false);

    // Check file has isFile=true
    let file_entry = data.iter().find(|e| e["name"] == "hello.txt").unwrap();
    assert_eq!(file_entry["is_dir"], false);
    assert_eq!(file_entry["is_file"], true);
}

#[tokio::test]
async fn list_workspace_files_flat_list() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested").join("b.txt"), "b").unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/list",
        json!({ "root": root.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let data = json["data"].as_array().unwrap();
    assert!(data.len() >= 2, "should contain at least 2 files");

    let names: Vec<&str> = data.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.txt"));
}

#[tokio::test]
async fn get_file_metadata_returns_info() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("test.txt");
    std::fs::write(&file_path, "hello world").unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/metadata",
        json!({ "path": file_path.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["name"], "test.txt");
    assert_eq!(json["data"]["size"], 11); // "hello world" = 11 bytes
    assert!(json["data"]["last_modified"].as_i64().unwrap() > 0);
}

// ===========================================================================
// File read/write
// ===========================================================================

#[tokio::test]
async fn read_file_returns_content() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("read_me.txt");
    std::fs::write(&file_path, "file content here").unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/read",
        json!({ "path": file_path.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"], "file content here");
}

#[tokio::test]
async fn read_file_nonexistent_returns_null() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let dir = tempfile::tempdir().unwrap();
    let fake_path = dir.path().join("nonexistent.txt");

    let req = json_with_token(
        "POST",
        "/api/fs/read",
        json!({ "path": fake_path.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["data"].is_null());
}

#[tokio::test]
async fn read_file_with_workspace_field_accepts_non_home_path() {
    let sandbox = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app_with_file_roots(vec![sandbox.path().to_path_buf()]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let file_path = workspace.path().join("preview.md");
    std::fs::write(&file_path, "# hello").unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/read",
        json!({
            "path": file_path.to_str().unwrap(),
            "workspace": workspace.path().to_str().unwrap()
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"], "# hello");
}

#[tokio::test]
async fn read_file_without_workspace_rejects_non_sandbox_path() {
    let sandbox = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app_with_file_roots(vec![sandbox.path().to_path_buf()]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let file_path = workspace.path().join("preview.md");
    std::fs::write(&file_path, "# hello").unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/read",
        json!({ "path": file_path.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let json = body_json(resp).await;
    assert_eq!(json["code"], "PATH_OUTSIDE_SANDBOX");
}

#[tokio::test]
async fn read_file_non_existent_within_sandbox_returns_null() {
    let sandbox = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app_with_file_roots(vec![sandbox.path().to_path_buf()]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let file_path = sandbox.path().join("missing.md");

    let req = json_with_token(
        "POST",
        "/api/fs/read",
        json!({ "path": file_path.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["data"].is_null());
}

#[tokio::test]
async fn image_base64_with_workspace_field_accepts_non_home_path() {
    let sandbox = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let (mut app, services) = build_app_with_file_roots(vec![sandbox.path().to_path_buf()]).await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let file_path = workspace.path().join("preview.png");
    std::fs::write(&file_path, [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]).unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/image-base64",
        json!({
            "path": file_path.to_str().unwrap(),
            "workspace": workspace.path().to_str().unwrap()
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert!(json["data"].as_str().unwrap().starts_with("data:image/png;base64,"));
}

#[tokio::test]
async fn write_file_creates_and_returns_true() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("new_file.txt");

    let req = json_with_token(
        "POST",
        "/api/fs/write",
        json!({
            "path": file_path.to_str().unwrap(),
            "data": "written via api",
            "workspace": dir.path().to_str().unwrap()
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"], true);

    // Verify file actually written
    let content = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(content, "written via api");
}

#[tokio::test]
async fn read_file_buffer_returns_base64() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("binary.bin");
    std::fs::write(&file_path, [0x00, 0xFF, 0xAB]).unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/read-buffer",
        json!({ "path": file_path.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let encoded = json["data"].as_str().unwrap();
    // Verify base64 roundtrip
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap();
    assert_eq!(decoded, vec![0x00, 0xFF, 0xAB]);
}

// ===========================================================================
// File management
// ===========================================================================

#[tokio::test]
async fn copy_files_to_workspace() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let src_dir = tempfile::tempdir().unwrap();
    std::fs::write(src_dir.path().join("source.txt"), "content").unwrap();

    let ws_dir = tempfile::tempdir().unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/copy",
        json!({
            "file_paths": [src_dir.path().join("source.txt").to_str().unwrap()],
            "workspace": ws_dir.path().to_str().unwrap()
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert!(!json["data"]["copied_files"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn remove_entry_deletes_file() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("to_delete.txt");
    std::fs::write(&file_path, "bye").unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/remove",
        json!({
            "path": file_path.to_str().unwrap(),
            "workspace": dir.path().to_str().unwrap()
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!file_path.exists());
}

#[tokio::test]
async fn rename_entry_returns_new_path() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.txt");
    std::fs::write(&old_path, "data").unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/rename",
        json!({
            "path": old_path.to_str().unwrap(),
            "new_name": "new.txt"
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let new_path = json["data"]["new_path"].as_str().unwrap();
    assert!(new_path.contains("new.txt"));
    assert!(!old_path.exists());
}

#[tokio::test]
async fn create_temp_file_returns_path() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/fs/temp",
        json!({ "file_name": "temp_test.txt" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let path = json["data"].as_str().unwrap();
    assert!(path.contains("temp_test.txt"));
    assert!(std::path::Path::new(path).exists());

    // Cleanup
    let _ = std::fs::remove_file(path);
}

// ===========================================================================
// Image processing
// ===========================================================================

#[tokio::test]
async fn get_image_base64_returns_data_url() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let dir = tempfile::tempdir().unwrap();
    let img_path = dir.path().join("pixel.png");
    // Minimal valid 1x1 PNG
    let png_bytes: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00,
        0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0xE2,
        0x21, 0xBC, 0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    std::fs::write(&img_path, png_bytes).unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/image-base64",
        json!({ "path": img_path.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let data_url = json["data"].as_str().unwrap();
    assert!(data_url.starts_with("data:image/png;base64,"));
}

#[tokio::test]
async fn fetch_remote_image_non_whitelisted_returns_placeholder_svg() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/fs/fetch-remote-image",
        json!({ "url": "https://evil.example.com/image.png" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let data_url = json["data"].as_str().unwrap();
    assert!(
        data_url.starts_with("data:image/svg+xml"),
        "expected placeholder SVG for non-whitelisted host"
    );
}

// ===========================================================================
// ZIP operations
// ===========================================================================

#[tokio::test]
async fn create_zip_with_text_content() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let dir = tempfile::tempdir().unwrap();
    let zip_path = dir.path().join("test.zip");

    let req = json_with_token(
        "POST",
        "/api/fs/zip",
        json!({
            "path": zip_path.to_str().unwrap(),
            "files": [
                { "name": "greeting.txt", "content": "hello zip" }
            ]
        }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"], true);
    assert!(zip_path.exists());
}

#[tokio::test]
async fn cancel_zip_nonexistent_returns_false() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/fs/zip/cancel",
        json!({ "request_id": "nonexistent-id" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"], false);
}

// ===========================================================================
// File watch
// ===========================================================================

#[tokio::test]
async fn watch_stop_all_succeeds() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token("POST", "/api/fs/watch/stop-all", json!({}), &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
}

// ===========================================================================
// Snapshot operations
// ===========================================================================

#[tokio::test]
async fn snapshot_init_and_compare_on_plain_dir() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();
    std::fs::write(workspace.join("file.txt"), "initial").unwrap();

    // Init snapshot
    let req = json_with_token(
        "POST",
        "/api/fs/snapshot/init",
        json!({ "workspace": workspace.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    assert_eq!(json["data"]["mode"], "snapshot");

    // Get info
    let req = json_with_token(
        "POST",
        "/api/fs/snapshot/info",
        json!({ "workspace": workspace.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"]["mode"], "snapshot");

    // Modify file and compare
    std::fs::write(workspace.join("file.txt"), "modified").unwrap();

    let req = json_with_token(
        "POST",
        "/api/fs/snapshot/compare",
        json!({ "workspace": workspace.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let unstaged = json["data"]["unstaged"].as_array().unwrap();
    assert!(!unstaged.is_empty(), "should detect unstaged modification");
    assert_eq!(unstaged[0]["operation"], "modify");

    // Get baseline content
    let req = json_with_token(
        "POST",
        "/api/fs/snapshot/baseline",
        json!({
            "workspace": workspace.to_str().unwrap(),
            "file_path": "file.txt"
        }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["data"], "initial");

    // Dispose
    let req = json_with_token(
        "POST",
        "/api/fs/snapshot/dispose",
        json!({ "workspace": workspace.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn snapshot_init_git_repo() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Create a temporary git repo
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path();
    let repo = git2::Repository::init(workspace).unwrap();
    std::fs::write(workspace.join("readme.md"), "# hello").unwrap();

    // Stage and commit
    let mut index = repo.index().unwrap();
    index.add_path(std::path::Path::new("readme.md")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

    // Init snapshot — should detect git-repo mode
    let req = json_with_token(
        "POST",
        "/api/fs/snapshot/init",
        json!({ "workspace": workspace.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["data"]["mode"], "git-repo");
    assert!(json["data"]["branch"].is_string());

    // Branches
    let req = json_with_token(
        "POST",
        "/api/fs/snapshot/branches",
        json!({ "workspace": workspace.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let branches = json["data"].as_array().unwrap();
    assert!(!branches.is_empty());

    // Dispose
    let req = json_with_token(
        "POST",
        "/api/fs/snapshot/dispose",
        json!({ "workspace": workspace.to_str().unwrap() }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

// ===========================================================================
// Path traversal rejection
// ===========================================================================

#[tokio::test]
async fn path_traversal_rejected() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let req = json_with_token(
        "POST",
        "/api/fs/read",
        json!({ "path": "/tmp/../../../etc/passwd" }),
        &token,
        &csrf,
    );
    let resp = app.oneshot(req).await.unwrap();
    // Should be rejected (400 bad request)
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ===========================================================================
// /api/fs/upload — multipart upload
// ===========================================================================

struct UploadMultipart {
    boundary: String,
    parts: Vec<u8>,
}

impl UploadMultipart {
    fn new() -> Self {
        Self {
            boundary: "----TestBoundaryFsUpload9XyZ".to_owned(),
            parts: Vec::new(),
        }
    }

    fn add_text(mut self, name: &str, value: &str) -> Self {
        self.parts
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        self.parts
            .extend_from_slice(format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes());
        self.parts.extend_from_slice(value.as_bytes());
        self.parts.extend_from_slice(b"\r\n");
        self
    }

    fn add_file(mut self, name: &str, filename: &str, mime: &str, data: &[u8]) -> Self {
        self.parts
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        self.parts.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n").as_bytes(),
        );
        self.parts
            .extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
        self.parts.extend_from_slice(data);
        self.parts.extend_from_slice(b"\r\n");
        self
    }

    fn build(mut self) -> (String, Vec<u8>) {
        self.parts
            .extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        let content_type = format!("multipart/form-data; boundary={}", self.boundary);
        (content_type, self.parts)
    }
}

fn upload_request(content_type: &str, body: Vec<u8>, token: &str, csrf: &str) -> axum::http::Request<axum::body::Body> {
    let content_length = body.len();
    axum::http::Request::builder()
        .method("POST")
        .uri("/api/fs/upload")
        .header("content-type", content_type)
        .header("content-length", content_length)
        .header("authorization", format!("Bearer {token}"))
        .header("x-csrf-token", csrf)
        .header("cookie", format!("nomifun-csrf-token={csrf}"))
        .body(axum::body::Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn upload_accepts_small_png_and_returns_readable_path() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // Minimal valid 1x1 PNG (67 bytes).
    let png_bytes: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00,
        0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0xE2,
        0x21, 0xBC, 0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    let conv_id = format!("conv-upload-{}", std::process::id());
    let (content_type, body) = UploadMultipart::new()
        .add_file("file", "paste.png", "image/png", &png_bytes)
        .add_text("conversation_id", &conv_id)
        .build();

    let req = upload_request(&content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    assert_eq!(json["success"], true);
    let path = json["data"].as_str().expect("data should be a string path");
    let p = std::path::Path::new(path);
    assert!(p.is_absolute(), "returned path must be absolute: {path}");
    assert_eq!(p.file_name().unwrap().to_string_lossy(), "paste.png");
    // conversation_id routing produced a sub-directory of that name.
    assert_eq!(p.parent().unwrap().file_name().unwrap().to_string_lossy(), conv_id);
    // File contents must match what we uploaded.
    let on_disk = std::fs::read(p).expect("uploaded file should be readable");
    assert_eq!(on_disk, png_bytes);

    // Cleanup.
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_dir(p.parent().unwrap());
}

#[tokio::test]
async fn upload_uses_content_disposition_filename_when_file_name_missing() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let bytes = b"hello upload".to_vec();
    let unique = format!("dispo-{}.bin", std::process::id());
    let (content_type, body) = UploadMultipart::new()
        .add_file("file", &unique, "application/octet-stream", &bytes)
        .build();

    let req = upload_request(&content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let path = json["data"].as_str().unwrap();
    assert!(path.ends_with(&unique));
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn upload_prefers_explicit_file_name_field_over_dispo() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let bytes = b"pref".to_vec();
    let explicit = format!("explicit-{}.bin", std::process::id());
    let (content_type, body) = UploadMultipart::new()
        .add_file("file", "dispo.bin", "application/octet-stream", &bytes)
        .add_text("file_name", &explicit)
        .build();

    let req = upload_request(&content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let json = body_json(resp).await;
    let path = json["data"].as_str().unwrap();
    assert!(path.ends_with(&explicit), "expected filename {explicit}, got {path}");
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn upload_missing_file_field_returns_400() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    let (content_type, body) = UploadMultipart::new().add_text("file_name", "ignored.png").build();

    let req = upload_request(&content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let json = body_json(resp).await;
    assert_eq!(json["success"], false);
}

#[tokio::test]
async fn upload_body_exceeding_30mb_returns_413() {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;

    // 31 MB payload comfortably exceeds UPLOAD_MAX_SIZE (30 MB).
    let big = vec![0u8; 31 * 1024 * 1024];
    let (content_type, body) = UploadMultipart::new()
        .add_file("file", "big.bin", "application/octet-stream", &big)
        .build();

    let req = upload_request(&content_type, body, &token, &csrf);
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
