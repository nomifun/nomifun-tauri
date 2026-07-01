//! Smoke test that drives the REAL desktop WebUI/LAN serving path end-to-end
//! (`DesktopServer::start` → `start_lan`) against a throwaway data dir, so the
//! actual failure cause of "enable WebUI" surfaces deterministically instead of
//! being guessed at. Prints the resolved status (port, LAN IP, URL, error).

fn local_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("local reqwest client should build")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webui_lan_start_smoke() {
    use clap::Parser as _;

    // Isolated data dir so we never touch a running instance's state / lock.
    let tmp = tempfile::TempDir::new().unwrap();
    // Isolate the data dir via --data-dir below (NOT a process-global env var):
    // these tests run in parallel, so a shared set_var("NOMIFUN_DATA_DIR") would
    // race and two backends could resolve to the SAME dir → "data directory
    // already in use by another running NomiFun backend".
    let data_dir = tmp.path().to_string_lossy().into_owned();
    let spa_dir = tmp.path().join("spa");
    std::fs::create_dir_all(&spa_dir).unwrap();
    std::fs::write(
        spa_dir.join("index.html"),
        "<!doctype html><title>Nomi</title>",
    )
    .unwrap();

    let cli = nomifun_app::cli::Cli::parse_from(["nomifun-desktop-test", "--data-dir", &data_dir]);
    let merged_path = std::env::var("PATH").unwrap_or_default();

    let started = nomifun_app::DesktopServer::start(&cli, &merged_path, Some(spa_dir), None).await;
    let (server, _keep) = match started {
        Ok(pair) => pair,
        Err(e) => panic!("DesktopServer::start failed: {e:#}"),
    };

    eprintln!("== loopback_port = {}", server.loopback_port());

    let status = server.start_lan().await;
    eprintln!("== start_lan status = {status:?}");

    server.stop_lan().await;

    assert!(
        status.running,
        "start_lan did NOT run — error = {:?}",
        status.error
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webui_lan_spa_deep_link_serves_app_shell() {
    use clap::Parser as _;

    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().to_string_lossy().into_owned();
    let spa_dir = tmp.path().join("spa");
    std::fs::create_dir_all(&spa_dir).unwrap();
    std::fs::write(
        spa_dir.join("index.html"),
        "<!doctype html><title>Nomi deep link</title>",
    )
    .unwrap();

    let cli = nomifun_app::cli::Cli::parse_from(["nomifun-desktop-test", "--data-dir", &data_dir]);
    let merged_path = std::env::var("PATH").unwrap_or_default();

    let (server, _keep) =
        nomifun_app::DesktopServer::start(&cli, &merged_path, Some(spa_dir), None)
            .await
            .expect("DesktopServer::start failed");

    let status = server.start_lan().await;
    assert!(status.running, "start_lan failed: {:?}", status.error);

    let response = local_http_client()
        .get(format!(
            "http://127.0.0.1:{}/open-capabilities",
            status.port
        ))
        .send()
        .await
        .expect("request to LAN listener failed");
    let response_status = response.status();
    let body = response.text().await.unwrap_or_default();

    server.stop_lan().await;

    assert_eq!(response_status, reqwest::StatusCode::OK);
    assert!(
        body.contains("Nomi deep link"),
        "SPA deep link should serve index.html, got status={response_status}; body={body}"
    );
}

/// A LAN listener with only `/qr-login` + `/api/auth/qr-login` but no SPA shell
/// reproduces the phone symptom: the QR page can say "Login successful", then
/// the browser navigates to `/` and receives an HTTP failure. Refuse that
/// partial state at start-up so the desktop UI reports a real WebUI start error
/// instead of handing users a broken QR flow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webui_lan_without_app_shell_fails_instead_of_serving_partial_qr_flow() {
    use clap::Parser as _;

    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().to_string_lossy().into_owned();
    let cli = nomifun_app::cli::Cli::parse_from(["nomifun-desktop-test", "--data-dir", &data_dir]);
    let merged_path = std::env::var("PATH").unwrap_or_default();

    let (server, _keep) = nomifun_app::DesktopServer::start(&cli, &merged_path, None, None)
        .await
        .expect("DesktopServer::start failed");

    let status = server.start_lan().await;

    assert!(
        !status.running,
        "LAN WebUI must not start without an app shell"
    );
    assert!(
        status
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("WebUI app shell"),
        "missing app shell error should be actionable, got {:?}",
        status.error
    );
}

/// Regression guard for the dev bug "saved figure image is broken + desktop
/// companion renders blank". Native `<img>` / `new Image()` loads (figure
/// thumbnails, the companion mesh texture) cannot present the local-trust
/// header, so under `TrustLocalToken` the figure-image GET MUST be auth-exempt —
/// while listing/creation stay authenticated. Boots the real desktop backend and
/// hits its loopback port with NO trust header, exactly like a native image load.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn figure_image_get_is_public_but_listing_stays_authenticated() {
    use clap::Parser as _;

    let tmp = tempfile::TempDir::new().unwrap();
    // Isolate the data dir via --data-dir below (NOT a process-global env var):
    // these tests run in parallel, so a shared set_var("NOMIFUN_DATA_DIR") would
    // race and two backends could resolve to the SAME dir → "data directory
    // already in use by another running NomiFun backend".
    let data_dir = tmp.path().to_string_lossy().into_owned();

    let cli = nomifun_app::cli::Cli::parse_from(["nomifun-desktop-test", "--data-dir", &data_dir]);
    let merged_path = std::env::var("PATH").unwrap_or_default();
    let (server, _keep) = nomifun_app::DesktopServer::start(&cli, &merged_path, None, None)
        .await
        .expect("DesktopServer::start failed");

    let base = format!("http://127.0.0.1:{}", server.loopback_port());
    let client = local_http_client();

    // Figure-image GET with NO trust header (what a native <img> sends): must NOT
    // be auth-rejected, AND must not 500. Under `TrustLocalToken` an untrusted
    // request gets no injected `CurrentUser`, so the handler must not depend on
    // that extension — an unknown id yields 404, a real one 200, never 401/403/500.
    let img = client
        .get(format!(
            "{base}/api/companion/figures/figure_nonexistent/image"
        ))
        .send()
        .await
        .expect("figure image request failed");
    assert_eq!(
        img.status(),
        reqwest::StatusCode::NOT_FOUND,
        "figure-image GET for an unknown id must be a clean 404 (auth-exempt, no \
         CurrentUser dependency); got {} — a 401/403 means the route is still \
         authenticated, a 500 means the handler still extracts Extension<CurrentUser>",
        img.status()
    );

    // The figures listing must STILL require auth — no trust header → rejected.
    let list = client
        .get(format!("{base}/api/companion/figures"))
        .send()
        .await
        .expect("figures listing request failed");
    assert!(
        list.status() == reqwest::StatusCode::UNAUTHORIZED
            || list.status() == reqwest::StatusCode::FORBIDDEN,
        "figures listing must stay authenticated, got {}",
        list.status()
    );
}

/// In DEV the LAN listener must serve the SAME live frontend the desktop webview
/// loads — proxied to the vite dev server — NOT a stale bundled `ui/dist`. This
/// stands up a mock "vite" server, points `DesktopServer` at it, enables LAN,
/// and asserts a request to the LAN port is proxied through to the live content.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webui_lan_dev_proxy_serves_live_frontend() {
    use clap::Parser as _;

    let tmp = tempfile::TempDir::new().unwrap();
    // Isolate the data dir via --data-dir below (NOT a process-global env var):
    // these tests run in parallel, so a shared set_var("NOMIFUN_DATA_DIR") would
    // race and two backends could resolve to the SAME dir → "data directory
    // already in use by another running NomiFun backend".
    let data_dir = tmp.path().to_string_lossy().into_owned();

    // Mock vite dev server: returns a recognizable marker for any path.
    let mock = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let mock_port = mock.local_addr().unwrap().port();
    tokio::spawn(async move {
        let app = axum::Router::new().fallback(|| async { "LIVE_VITE_INDEX_MARKER" });
        let _ = axum::serve(mock, app).await;
    });

    let cli = nomifun_app::cli::Cli::parse_from(["nomifun-desktop-test", "--data-dir", &data_dir]);
    let merged_path = std::env::var("PATH").unwrap_or_default();
    let dev_url = format!("http://127.0.0.1:{mock_port}");

    let (server, _keep) =
        nomifun_app::DesktopServer::start(&cli, &merged_path, None, Some(dev_url))
            .await
            .expect("DesktopServer::start failed");

    let status = server.start_lan().await;
    assert!(status.running, "start_lan failed: {:?}", status.error);

    // A request to the LAN listener's SPA path must be proxied to the mock vite.
    let url = format!("http://127.0.0.1:{}/some/spa/route", status.port);
    let response = local_http_client()
        .get(&url)
        .send()
        .await
        .expect("request to LAN listener failed");
    let response_status = response.status();
    let body = response.text().await.unwrap_or_default();
    eprintln!("== proxied status = {response_status}; body = {body}");
    assert!(
        body.contains("LIVE_VITE_INDEX_MARKER"),
        "LAN listener did not proxy to the dev frontend; status={response_status}; got: {body}"
    );

    server.stop_lan().await;
}

/// Regression for the credential-persistence bug: a username the user set while
/// WebUI was OFF (password still empty) must NOT be reset to "admin" when the
/// LAN server is first enabled, and `status_snapshot` must report the persisted
/// username + `password_set` even before/without the LAN listener running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn webui_enable_preserves_user_set_username_and_reports_persisted_state() {
    use clap::Parser as _;

    let tmp = tempfile::TempDir::new().unwrap();
    let data_dir = tmp.path().to_string_lossy().into_owned();
    let spa_dir = tmp.path().join("spa");
    std::fs::create_dir_all(&spa_dir).unwrap();
    std::fs::write(spa_dir.join("index.html"), "<!doctype html><title>Nomi</title>").unwrap();

    let cli = nomifun_app::cli::Cli::parse_from(["nomifun-desktop-test", "--data-dir", &data_dir]);
    let merged_path = std::env::var("PATH").unwrap_or_default();

    let (server, _keep) = nomifun_app::DesktopServer::start(&cli, &merged_path, Some(spa_dir), None)
        .await
        .expect("DesktopServer::start failed");

    // Simulate the user renaming the admin from the panel while WebUI is OFF:
    // this leaves `password_hash` empty. Open the SAME db file the server uses
    // (WAL allows a second connection) and rewrite the username directly.
    let db_path = tmp.path().join("nomifun-backend.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db_path.display()))
        .await
        .expect("open backend db");
    let affected = sqlx::query("UPDATE users SET username = 'custom_admin' WHERE id = 'system_default_user'")
        .execute(&pool)
        .await
        .expect("rename system admin")
        .rows_affected();
    assert_eq!(affected, 1, "should have renamed the seeded system_default_user");
    pool.close().await;

    // Before enabling: snapshot reflects the persisted username and "no password yet".
    let before = server.status_snapshot().await;
    assert_eq!(before.admin_username, "custom_admin", "persisted username must surface while stopped");
    assert!(!before.password_set, "no password provisioned yet");

    // Enable LAN: this provisions a password but MUST NOT clobber the username.
    let started = server.start_lan().await;
    assert!(started.running, "start_lan failed: {:?}", started.error);
    assert_eq!(started.admin_username, "custom_admin", "username must be preserved, not reset to 'admin'");
    assert!(started.password_set, "a password must exist once LAN is exposed");
    assert!(
        started.initial_password.is_some(),
        "first enable should surface the one-time initial password"
    );

    // "Restart" equivalent: a fresh snapshot still shows the custom username and
    // that a password is set (the credential is durable, not perceived as lost).
    server.stop_lan().await;
    let after = server.status_snapshot().await;
    assert_eq!(after.admin_username, "custom_admin");
    assert!(after.password_set, "password must remain set after stopping the LAN listener");
}
