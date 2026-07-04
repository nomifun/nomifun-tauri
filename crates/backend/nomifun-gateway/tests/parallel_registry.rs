//! 网关并行执行面：`BrowserRegistry::execute_parallel` 异 key 并发、同 key 串行,结果保输入序。
//!
//! **Hermetic（不启动 Chrome）**：用**未知动作**——facade 在 dispatch 即返 `Unknown action {name}`
//! 错误,**不调 `engine()`**（不解析/下载 Chrome）,但仍走完 `execute` 的 per-key 锁 + 派发路径。
//! 错误消息含动作名,故可据此断言结果**按输入序**返回。
//!
//! 跑：`cargo nextest run -p nomifun-gateway --features browser-use -E 'test(execute_parallel)'`

#![cfg(feature = "browser-use")]

use nomifun_gateway::browser_registry::{tool_result_to_value, BrowserRegistry};
use serde_json::json;

#[tokio::test]
async fn execute_parallel_distinct_keys_results_in_input_order() {
    let reg = BrowserRegistry::default_for_browser_use();
    // 两个不同 key（异 key 并发）。未知动作 → 各自快速返错,不启动 Chrome。
    let batch = vec![
        ("companion-a".to_string(), json!({"action": "zzz_marker_alpha"})),
        ("companion-b".to_string(), json!({"action": "zzz_marker_beta"})),
    ];
    let started = std::time::Instant::now();
    let results = reg.execute_parallel(batch).await;
    assert_eq!(results.len(), 2, "一输入一结果");
    // 保序：结果[0] 对应 alpha、结果[1] 对应 beta（错误消息回带动作名）。
    let strs: Vec<String> = results
        .into_iter()
        .map(|r| tool_result_to_value(r).to_string())
        .collect();
    assert!(strs[0].contains("zzz_marker_alpha"), "结果须按输入序(idx0=alpha): {}", strs[0]);
    assert!(strs[1].contains("zzz_marker_beta"), "结果须按输入序(idx1=beta): {}", strs[1]);
    // 不得死锁/卡住（未知动作不启动 Chrome,应近乎瞬时）。
    assert!(started.elapsed().as_secs() < 20, "execute_parallel 不得死锁/卡住");
}

#[tokio::test]
async fn execute_parallel_same_key_serializes_and_returns_all() {
    let reg = BrowserRegistry::default_for_browser_use();
    // 同一 key 两次：必须经该 key 的 CompanionBrowser 锁**串行**（不并发撞同一引擎）,但两结果都返回。
    let batch = vec![
        ("companion-a".to_string(), json!({"action": "zzz_one"})),
        ("companion-a".to_string(), json!({"action": "zzz_two"})),
    ];
    let results = reg.execute_parallel(batch).await;
    assert_eq!(results.len(), 2, "同 key 两调用都须返回(串行,无丢失)");
}

// ── real-chrome：并发隔离真机证明（#[ignore]，需本机/打包 chrome）───────────────────────
// 上面两个测试是 hermetic 的（未知动作 → 不启动 Chrome），只证保序/串行/不死锁，**不**证明隔离。
// 本测试用真动作让两个**不同 key** 并发各 navigate：每 key 的 BrowserTool 自分配进程内唯一
// user-data-dir（`<data_dir>/profiles/<token>`），故两个 Chrome 真并发冷启都成功——旧的共享
// `<data_dir>/profile` 下第二个会撞 Chromium 进程单例（转发命令行后退出）而失败。这正是修复的核心证明。
//   set NOMIFUN_CHROME_BINARY=C:\Program Files\Google\Chrome\Application\chrome.exe 后：
//   cargo nextest run -p nomifun-gateway --features browser-use --run-ignored all -E 'test(distinct_keys_both_launch)'
#[tokio::test]
#[ignore = "需本机/打包 chrome：并发隔离真机证明（set NOMIFUN_CHROME_BINARY）"]
async fn execute_parallel_distinct_keys_both_launch_real_chrome() {
    let reg = BrowserRegistry::default_for_browser_use();
    let page = "data:text/html,<title>iso</title><h1>hello</h1>";
    let batch = vec![
        ("companion-iso-a".to_string(), json!({"action": "navigate", "url": page})),
        ("companion-iso-b".to_string(), json!({"action": "navigate", "url": page})),
    ];
    let results = reg.execute_parallel(batch).await;
    assert_eq!(results.len(), 2, "一输入一结果");
    for (i, r) in results.into_iter().enumerate() {
        let v = tool_result_to_value(r);
        assert!(
            v.get("error").is_none(),
            "key {i} navigate must succeed — distinct per-instance user-data-dirs mean no Chromium \
             singleton collision between the two concurrent launches: {v}"
        );
    }
}
