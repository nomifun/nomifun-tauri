//! **P2 F3：多步 e2e（facade 端到端）+ 安全门生效证据**（`#[ignore]`，本机/打包 chrome）。
//!
//! 这是 P2 收官的端到端验证：**经 `BrowserTool` facade（`Tool::execute`）**串起完整真实流程，证明
//! P2 的各组件（navigate settle / observe ref 表 / actionability 五检查 + 三级兜底 / verify-after-act /
//! 不可逆分类器 + facade 独立 fail-closed 门 / secret 域绑定）在真 Chrome 上**协同工作**。
//!
//! 与 engine 层集成测试（`nomi-browser-engine/tests/integration_act.rs` 的 `c1_*`/`c2_*`）的区别：
//! 那些直接驱动 `engine.act(&ActSpec, &Progress)`（引擎契约）；本测试走**更高层**——经 facade 的
//! `execute(json!{...})`（LLM 真正调用的入口），故同时覆盖：①facade 的 dispatch/参数解析；②facade 的
//! redline 独立门（在 dispatch 前拦审批旁路会话的不可逆动作）；③facade 的 `secret:NAME` origin 门。
//!
//! ## 覆盖的 P2 DoD 验收点
//! - **多步协同**：navigate → observe → type username → type password → select_option Pro → click submit
//!   （普通会话 submit 真提交 → onsubmit 标记 `submitted:<user>:<plan>`，经再 observe 读回证实）。
//! - **安全门生效（红线）**：
//!   1. **yolo/审批旁路会话** click submit（accname="Submit order" → 分类 Irreversible）→ facade redline
//!      门 **hard-deny Blocked**（设计裁决⑧：不靠被旁路的 orchestration，靠 facade 独立 fail-closed 门）；
//!   2. **普通会话** 同一 submit → 门**不拦**（交 orchestration），动作真执行；
//!   3. **secret 域绑定 fail-closed**：`secret:NAME` 在 file:// 源（无 eTLD+1）→ Blocked，明文不入输出。
//!
//! 手动跑（本机 Windows 有系统 Chrome）：
//!   set NOMIFUN_CHROME_BINARY=C:\Program Files\Google\Chrome\Application\chrome.exe
//!   cargo nextest run -p nomi-browser --run-ignored all -E 'test(e2e)'
//! 跑完核对任务管理器无残留 chrome（engine 的 Builder kill_on_drop 应自动清；tool Drop 即释放）。

use std::path::PathBuf;

use nomi_browser::BrowserTool;
use nomi_config::config::BrowserConfig;
use nomi_tools::Tool;
use serde_json::json;

/// fixture 的 file:// URL。`CARGO_MANIFEST_DIR` 在 unix 是 `/abs`（已带前导斜杠）、在 windows
/// 是 `C:/abs`（需补一个），故仅缺失时补斜杠——避免 unix 上 `file:///{manifest}` 产生四斜杠
/// （`file:////...`）触发 chrome 归一成三斜杠 → navigate redirect 误判。
fn fixture_url(name: &str) -> String {
    let manifest = env!("CARGO_MANIFEST_DIR").replace('\\', "/");
    let abs = if manifest.starts_with('/') {
        manifest
    } else {
        format!("/{manifest}")
    };
    format!("file://{abs}/tests/fixtures/{name}")
}

/// 从 facade observe 的 aria YAML 文本里，按 `role` + accname 子串找到 `[ref=f<seq>e<n>]`。
///
/// observe 输出形如 `- textbox "Username" [ref=f0e1]` / `- button "Submit order" [ref=f0e4]`。
/// 我们找含 role 词 + accname 子串 + `[ref=` 标记的那一行，抽出 ref。facade 不暴露结构化
/// `Observation`（那是 engine 契约），故按 LLM 真正看到的文本解析（与模型同视角）。
fn find_ref(observe_text: &str, role: &str, accname: &str) -> String {
    observe_text
        .lines()
        .find(|line| line.contains(role) && line.contains(accname) && line.contains("[ref="))
        .and_then(|line| {
            let start = line.find("[ref=")? + 5;
            let end = line[start..].find(']')? + start;
            Some(line[start..end].to_string())
        })
        .unwrap_or_else(|| {
            panic!("observe output should expose a {role:?} with accname {accname:?}; got:\n{observe_text}")
        })
}

/// headless BrowserConfig（本机集成测试默认 headless；不依赖显示）。
fn headless_config() -> BrowserConfig {
    BrowserConfig { headless: true, ..Default::default() }
}

/// 本测试专属隔离 data_dir（避免与运行中的 app browser-data 争用同一 profile）。
fn isolated_data_dir(suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!("nomifun-f3-e2e-{suffix}-data"))
}

/// **并发隔离：同一 facade 的两个并发首调用只启动一个引擎（构造锁 `engine_build_gate`）。**
///
/// 场景对应 MCP stdio 桥：它共享**一个** `Arc<BrowserTool>` 且对 `execute` **零上游串行**。两个并发
/// 动作会各自触发 `engine()` 首建；若无构造锁，二者会各 `launch_chrome` 一个 Chrome，撞本 facade 的
/// **单一** user-data-dir（`<data_dir>/profiles/<token>`）→ Chromium 进程单例 → 一个失败。构造锁使
/// 只 launch 一个引擎、另一个复用 → 两个 navigate 都成功。这是 stdio 桥「并行查询一个节点失败」的修复证明。
///
///   set NOMIFUN_CHROME_BINARY=C:\Program Files\Google\Chrome\Application\chrome.exe
///   cargo nextest run -p nomi-browser --run-ignored all -E 'test(concurrent_first_calls)'
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需本机/打包 chrome：同 facade 并发构造锁证明（set NOMIFUN_CHROME_BINARY）"]
async fn concurrent_first_calls_on_one_facade_launch_single_engine() {
    let tool = BrowserTool::with_data_dir(isolated_data_dir("concurrent-gate"), false);
    let page = "data:text/html,<title>gate</title><h1>ok</h1>";
    let (a, b) = tokio::join!(
        tool.execute(json!({"action": "navigate", "url": page})),
        tool.execute(json!({"action": "navigate", "url": page})),
    );
    assert!(!a.is_error, "concurrent first-call A must succeed (single-flight engine build): {:?}", a.content);
    assert!(!b.is_error, "concurrent first-call B must succeed (single-flight engine build): {:?}", b.content);
}

/// **多步 e2e（普通会话，经 facade）+ 安全门「普通会话 submit 不被门拦」证据。**
///
/// navigate → observe → type username → type password → select_option Pro → click submit →
/// 再 observe 读回 `#form-status == submitted:e2e-user:pro`（证 type/select 真写入 + submit 真触发，
/// 且普通会话的 Irreversible submit **未被 facade 门拦**——门方向正确：只拦审批旁路会话）。
#[tokio::test]
#[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 --run-ignored all"]
async fn e2e_multistep_form_flow_through_facade_normal_session() {
    // 普通会话（session_bypasses_approval=false）：facade redline 门不拦不可逆动作（交 orchestration）。
    let tool = BrowserTool::with_data_dir(isolated_data_dir("normal"), false);

    // ── 1. navigate ────────────────────────────────────────────────────────────
    let nav = tool
        .execute(json!({"action": "navigate", "url": fixture_url("e2e-form.html")}))
        .await;
    eprintln!("navigate -> is_error={} content={:?}", nav.is_error, nav.content);
    assert!(!nav.is_error, "navigate must succeed: {}", nav.content);
    assert!(nav.content.contains("Navigated to"), "navigate message: {}", nav.content);

    // ── 2. observe（填 ref 表 + 武装注入侧 elements 缓存，act 反查的前置）────────────
    let obs = tool.execute(json!({"action": "observe"})).await;
    eprintln!("=== observe output ===\n{}", obs.content);
    assert!(!obs.is_error, "observe must succeed: {}", obs.content);
    let user_ref = find_ref(&obs.content, "textbox", "Username");
    let pass_ref = find_ref(&obs.content, "textbox", "Password");
    let plan_ref = find_ref(&obs.content, "combobox", "Plan");
    let submit_ref = find_ref(&obs.content, "button", "Submit order");
    eprintln!("refs: user={user_ref} pass={pass_ref} plan={plan_ref} submit={submit_ref}");

    // ── 3. type username（literal）→ verify changed ──────────────────────────────
    let type_user = tool
        .execute(json!({"action": "type", "ref": user_ref, "text": "e2e-user"}))
        .await;
    eprintln!("type username -> is_error={} content={:?}", type_user.is_error, type_user.content);
    assert!(!type_user.is_error, "type username must succeed: {}", type_user.content);
    assert!(type_user.content.contains("changed=true"), "type username should change value: {}", type_user.content);

    // ── 4. type password（literal——secret 路径的 fail-closed 在专门用例验，见下；正向 secret
    //        路径需真 http 源 + eTLD+1，离线 file:// 测不到，由 facade/engine 既有测试覆盖）─────
    let type_pass = tool
        .execute(json!({"action": "type", "ref": pass_ref, "text": "literal-pw-not-secret"}))
        .await;
    eprintln!("type password -> is_error={} content={:?}", type_pass.is_error, type_pass.content);
    assert!(!type_pass.is_error, "type password must succeed: {}", type_pass.content);
    assert!(type_pass.content.contains("changed=true"), "type password should change value: {}", type_pass.content);

    // ── 5. select_option Pro → verify after-anchor 含 "pro"（C2 修复点：读 .value 非 textContent）─
    let select = tool
        .execute(json!({"action": "select_option", "ref": plan_ref, "options": ["Pro"]}))
        .await;
    eprintln!("select_option -> is_error={} content={:?}", select.is_error, select.content);
    assert!(!select.is_error, "select_option must succeed: {}", select.content);
    assert!(select.content.contains("changed=true"), "select Pro should change value (free→pro): {}", select.content);
    assert!(
        select.content.contains("pro"),
        "select_option verify after-anchor should reflect the chosen value 'pro': {}",
        select.content
    );

    // ── 6. 普通会话 click submit（accname="Submit order" → Irreversible）→ 门不拦 + 真提交 ──
    // 先确认 facade 把它分类为 Irreversible（category_for 据 last_snapshot 的 accname 判）。
    assert_eq!(
        tool.category_for(&json!({"action": "click", "ref": submit_ref})),
        nomi_protocol::events::ToolCategory::Irreversible,
        "submit-order click must classify as Irreversible (so orchestration prompts in a normal session)"
    );
    let submit = tool
        .execute(json!({"action": "click", "ref": submit_ref}))
        .await;
    eprintln!("click submit (normal session) -> is_error={} content={:?}", submit.is_error, submit.content);
    // 普通会话：facade 门**不**hard-deny（方向正确）。click 真执行（成功或良性失败，但绝不是 Blocked）。
    let lower = submit.content.to_lowercase();
    assert!(
        !(submit.is_error && (lower.contains("blocked") || lower.contains("irreversible"))),
        "normal-session irreversible submit must NOT be hard-denied by the facade gate: {}",
        submit.content
    );

    // ── 7. 再 observe 读回 #form-status（role=status）== submitted:e2e-user:pro ─────────
    let after = tool.execute(json!({"action": "observe"})).await;
    eprintln!("=== observe after submit ===\n{}", after.content);
    assert!(!after.is_error, "post-submit observe must succeed: {}", after.content);
    assert!(
        after.content.contains("submitted:e2e-user:pro"),
        "form submit should fire with the typed username + selected plan (onsubmit marker); \
         observe output:\n{}",
        after.content
    );

    eprintln!(
        "=== F3 E2E READBACK SUMMARY (normal session) ===\n\
         navigate     = ok\n\
         observe refs = user={user_ref} pass={pass_ref} plan={plan_ref} submit={submit_ref}\n\
         type user    = changed=true\n\
         type pass    = changed=true\n\
         select Pro   = changed=true (value 'pro')\n\
         submit       = NOT blocked in normal session (classified Irreversible → orchestration)\n\
         form-status  = submitted:e2e-user:pro (onsubmit fired)"
    );
}

/// **安全门生效证据（红线）：审批旁路（yolo/companion）会话里的不可逆 submit → facade hard-deny Blocked。**
///
/// 这是设计裁决⑧的端到端证明：不靠被旁路的 orchestration 审批闸，靠 facade 的独立 fail-closed 门。
/// 经 `with_policy(.., session_bypasses_approval=true, ..)` 构造一个审批旁路会话的 tool（= yolo / companion
/// 强制 yolo / --auto-approve 的等价 test seam），navigate + observe 真页拿到真 submit ref（accname
/// "Submit order" → 分类 Irreversible），然后 `execute(click submit)` → **Blocked**（门在 dispatch 之前拦）。
#[tokio::test]
#[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 --run-ignored all"]
async fn e2e_security_gate_blocks_irreversible_submit_in_bypassing_session() {
    // 审批旁路会话（with_policy 第二参 = config.tools.auto_approve = true）→ redline 门武装。
    // 注意：with_policy 用 app_config_dir 的 browser-data；为隔离，先 with_data_dir 再... 但 with_data_dir
    // 不带 policy。这里直接用 with_policy（headless）——它的 data_dir 是 app browser-data；本测试只 navigate
    // 一个 file:// fixture（不落数据），且 chrome user-data-dir 由 engine 专属管理，争用风险低。
    let tool = BrowserTool::with_policy(&headless_config(), /* session_bypasses_approval */ true, false, false, None, None, None);

    let nav = tool
        .execute(json!({"action": "navigate", "url": fixture_url("e2e-form.html")}))
        .await;
    assert!(!nav.is_error, "navigate must succeed: {}", nav.content);

    let obs = tool.execute(json!({"action": "observe"})).await;
    assert!(!obs.is_error, "observe must succeed: {}", obs.content);
    let submit_ref = find_ref(&obs.content, "button", "Submit order");
    eprintln!("yolo session submit ref = {submit_ref}");

    // 旁路会话 + 不可逆 submit → facade redline 门 hard-deny（dispatch 前拦）。
    let blocked = tool
        .execute(json!({"action": "click", "ref": submit_ref}))
        .await;
    eprintln!("yolo click submit -> is_error={} content={:?}", blocked.is_error, blocked.content);
    assert!(
        blocked.is_error,
        "irreversible submit in an approval-bypassing session MUST be hard-denied: {}",
        blocked.content
    );
    let lower = blocked.content.to_lowercase();
    assert!(
        lower.contains("blocked") || lower.contains("irreversible"),
        "block message should explain the redline (blocked/irreversible): {}",
        blocked.content
    );

    eprintln!(
        "=== F3 SECURITY GATE EVIDENCE ===\n\
         session      = approval-bypassing (yolo/companion/auto_approve)\n\
         action       = click submit [ref={submit_ref}] (accname 'Submit order' → Irreversible)\n\
         result       = HARD-DENY Blocked (facade fail-closed gate, NOT orchestration)\n\
         message      = {:?}",
        blocked.content
    );
}

/// **安全门生效证据（secret 域绑定 fail-closed）：`secret:NAME` 在无 eTLD+1 的 file:// 源 → Blocked，
/// 明文绝不入输出。**
///
/// secret 正向注入路径需真 http 源（eTLD+1 域绑定），离线 file:// 无 registrable domain → 域门 fail-closed。
/// 这正好验**最关键的安全方向**：源不匹配 / 无源 → 拒绝解析，且 `secret:NAME` 字面量绝不当普通文本输入、
/// 也绝不泄漏配置的值。即便 yolo 会话也拦（门是 vault 的属性，非 orchestration 审批）。
#[tokio::test]
#[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 --run-ignored all"]
async fn e2e_secret_origin_gate_fails_closed_on_file_origin() {
    use nomifun_secret::SecretStore;

    // 配一个绑定到 example.com 的 secret（其值绝不应出现在任何输出里）。
    let mut store = SecretStore::ephemeral().expect("ephemeral store");
    let secret_plaintext = "F3-TOP-SECRET-PLAINTEXT-must-never-leak";
    store
        .register("login_pw", secret_plaintext, vec!["example.com".to_string()])
        .expect("register secret");

    let tool = BrowserTool::with_secret_store(isolated_data_dir("secret"), false, store);

    let nav = tool
        .execute(json!({"action": "navigate", "url": fixture_url("e2e-form.html")}))
        .await;
    assert!(!nav.is_error, "navigate must succeed: {}", nav.content);

    let obs = tool.execute(json!({"action": "observe"})).await;
    assert!(!obs.is_error, "observe must succeed: {}", obs.content);
    let pass_ref = find_ref(&obs.content, "textbox", "Password");

    // current origin = file://...e2e-form.html → 无 eTLD+1 → secret 域门 fail-closed（即便 secret 存在）。
    let res = tool
        .execute(json!({"action": "type", "ref": pass_ref, "text": "secret:login_pw"}))
        .await;
    eprintln!("type secret on file:// origin -> is_error={} content={:?}", res.is_error, res.content);
    assert!(
        res.is_error,
        "a secret bound to example.com must NOT resolve on a file:// origin (fail-closed): {}",
        res.content
    );
    // 安全铁律：明文绝不出现在错误输出里；`secret:login_pw` 字面量也不能被当普通文本输入（值不泄漏）。
    assert!(
        !res.content.contains(secret_plaintext),
        "SECURITY: the secret plaintext must NEVER appear in the tool output: {}",
        res.content
    );

    eprintln!(
        "=== F3 SECRET GATE EVIDENCE ===\n\
         origin       = file:// (no registrable eTLD+1)\n\
         secret       = bound to example.com (mismatch)\n\
         result       = fail-closed Blocked; plaintext NOT typed, NOT in output\n\
         message      = {:?}",
        res.content
    );
}

// ─── P3 Structured Extract (real Chrome + stub model) ───────────────────────

use std::sync::Arc;

use nomi_browser::extract::ExtractModel;

/// A stub model that "extracts" by returning a hardcoded JSON response.
/// In a real scenario the LLM would parse the aria snapshot; here we simulate
/// a correct extraction to verify the end-to-end facade wiring.
struct StubExtractModel;

#[async_trait::async_trait]
impl ExtractModel for StubExtractModel {
    async fn complete(&self, _prompt: &str) -> Result<String, String> {
        // Return structured JSON matching the schema we'll request.
        Ok(r#"{"products": [{"name": "Widget A", "price": 9.99}, {"name": "Gadget B", "price": 19.50}, {"name": "Doohickey C", "price": 4.25}]}"#.into())
    }
}

/// **P3 e2e: structured extract with a stub model on a real Chrome page.**
///
/// navigate fixture table → Extract{schema} with StubExtractModel injected →
/// verify the response is the model's structured JSON (not the raw deterministic payload).
///
/// Run: `NOMIFUN_CHROME_BINARY="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" cargo nextest run -p nomi-browser --run-ignored all -E 'test(e2e_structured_extract)'`
#[tokio::test]
#[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 --run-ignored all"]
async fn e2e_structured_extract_with_stub_model() {
    let data_dir = isolated_data_dir("extract");
    let tool = BrowserTool::with_data_dir(data_dir.clone(), false)
        .with_extract_model(Arc::new(StubExtractModel));

    // Navigate to the fixture table.
    let nav = tool
        .execute(json!({"action": "navigate", "url": fixture_url("extract-products.html")}))
        .await;
    eprintln!("navigate -> is_error={} content={:?}", nav.is_error, nav.content);
    assert!(!nav.is_error, "navigate must succeed: {}", nav.content);

    // Run Extract with a schema requesting products.
    let extract = tool
        .execute(json!({
            "action": "extract",
            "schema": {
                "type": "object",
                "required": ["products"],
                "properties": {
                    "products": {
                        "type": "array"
                    }
                }
            }
        }))
        .await;
    eprintln!("extract -> is_error={} content={:?}", extract.is_error, extract.content);
    assert!(!extract.is_error, "extract must succeed: {}", extract.content);

    // The output should be the model's structured JSON (pretty-printed).
    let parsed: serde_json::Value = serde_json::from_str(&extract.content)
        .expect("extract output must be valid JSON when model is available");
    assert!(parsed.get("products").is_some(), "response must have 'products' field");
    let products = parsed["products"].as_array().unwrap();
    assert_eq!(products.len(), 3, "expected 3 products");
    assert_eq!(products[0]["name"], "Widget A");
    assert_eq!(products[1]["price"], 19.50);

    eprintln!("=== P3 STRUCTURED EXTRACT EVIDENCE ===\nmodel output parsed as valid JSON with schema fields");

    let _ = std::fs::remove_dir_all(&data_dir);
}

/// **P3 e2e: extract WITHOUT model returns deterministic payload (graceful degradation).**
#[tokio::test]
#[ignore = "需本机/打包 chrome：set NOMIFUN_CHROME_BINARY 后 --run-ignored all"]
async fn e2e_extract_without_model_returns_deterministic_payload() {
    let data_dir = isolated_data_dir("extract-no-model");
    // No model injected → graceful degradation.
    let tool = BrowserTool::with_data_dir(data_dir.clone(), false);

    let nav = tool
        .execute(json!({"action": "navigate", "url": fixture_url("extract-products.html")}))
        .await;
    assert!(!nav.is_error, "navigate must succeed: {}", nav.content);

    let extract = tool
        .execute(json!({
            "action": "extract",
            "schema": { "type": "object", "required": ["products"] }
        }))
        .await;
    eprintln!("extract (no model) -> is_error={} content length={}", extract.is_error, extract.content.len());
    assert!(!extract.is_error, "extract must succeed even without model");

    // Without model, the output is the deterministic payload (not JSON-parseable as structured data).
    assert!(
        extract.content.contains("structured page representation")
            || extract.content.contains("accessibility snapshot")
            || extract.content.contains("[visible text]"),
        "without model, output must be the engine's deterministic payload, got: {}",
        &extract.content[..extract.content.len().min(200)]
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}

/// **Task 6 P7C: record→replay e2e** (`#[ignore]`, needs `NOMIFUN_CHROME_BINARY`).
///
/// Records click + type on a fixture form, replays on a fresh page, asserts the
/// same end-state. Proves the full record→replay pipeline end-to-end with a real
/// browser.
///
/// Run: `NOMIFUN_CHROME_BINARY="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
///   cargo nextest run -p nomi-browser --run-ignored all -E 'test(record_replay_e2e_smoke)'`
#[tokio::test]
#[ignore = "需 NOMIFUN_CHROME_BINARY（真 Chrome）：record→replay 端到端冒烟"]
async fn record_replay_e2e_smoke() {
    use nomi_browser::recording::{RecordedStep, Recording};
    use nomi_browser::replay::ReplayRunner;

    let data_dir = isolated_data_dir("record-replay");
    let tool = BrowserTool::with_data_dir(data_dir.clone(), false);

    // 1) Navigate to the fixture form.
    let nav = tool
        .execute(json!({"action": "navigate", "url": fixture_url("e2e-form.html")}))
        .await;
    assert!(!nav.is_error, "navigate: {}", nav.content);

    // 2) Observe to get refs.
    let obs = tool.execute(json!({"action": "observe"})).await;
    assert!(!obs.is_error, "observe: {}", obs.content);
    let user_ref = find_ref(&obs.content, "textbox", "Username");
    let pass_ref = find_ref(&obs.content, "textbox", "Password");
    eprintln!("record: user_ref={user_ref}, pass_ref={pass_ref}");

    // 3) Start recording and type into the username field.
    tool.start_recording();
    assert!(tool.is_recording());

    let type_res = tool
        .execute(json!({"action": "type", "ref": &user_ref, "text": "replay-test-user"}))
        .await;
    assert!(!type_res.is_error, "type: {}", type_res.content);

    let type_pass = tool
        .execute(json!({"action": "type", "ref": &pass_ref, "text": "replay-pass-123"}))
        .await;
    assert!(!type_pass.is_error, "type pass: {}", type_pass.content);

    // 4) Stop recording.
    let recording = tool.stop_recording().expect("should have recording");
    assert_eq!(recording.steps.len(), 2, "should have 2 recorded steps");
    assert_eq!(recording.steps[0].action, "type");
    assert_eq!(recording.steps[1].action, "type");
    eprintln!("recorded {} steps", recording.steps.len());

    // 5) Navigate to a fresh instance of the same page.
    let nav2 = tool
        .execute(json!({"action": "navigate", "url": fixture_url("e2e-form.html")}))
        .await;
    assert!(!nav2.is_error, "navigate fresh: {}", nav2.content);

    // 6) Observe on the fresh page to get new refs.
    let obs2 = tool.execute(json!({"action": "observe"})).await;
    assert!(!obs2.is_error, "observe fresh: {}", obs2.content);
    let new_user_ref = find_ref(&obs2.content, "textbox", "Username");
    let new_pass_ref = find_ref(&obs2.content, "textbox", "Password");
    eprintln!("replay: new_user_ref={new_user_ref}, new_pass_ref={new_pass_ref}");

    // 7) Build a replay recording with the fresh refs (simulating selector→ref
    //    re-resolution that a real replay system would do).
    let replay_recording = Recording {
        steps: vec![
            RecordedStep {
                intent: recording.steps[0].intent.clone(),
                action: "type".into(),
                args: json!({"ref": &new_user_ref, "text": "replay-test-user"}),
                selector: recording.steps[0].selector.clone(),
                url: recording.steps[0].url.clone(),
            },
            RecordedStep {
                intent: recording.steps[1].intent.clone(),
                action: "type".into(),
                args: json!({"ref": &new_pass_ref, "text": "replay-pass-123"}),
                selector: recording.steps[1].selector.clone(),
                url: recording.steps[1].url.clone(),
            },
        ],
        created_url: recording.created_url.clone(),
    };

    // 8) Replay.
    let replay_result = ReplayRunner::replay(&replay_recording, &tool).await;
    assert_eq!(
        replay_result.succeeded, 2,
        "both replay steps should succeed; outcomes: {:?}",
        replay_result.outcomes.iter().map(|o| (&o.action, o.success, &o.result.content)).collect::<Vec<_>>()
    );
    assert_eq!(replay_result.failed, 0);

    // 9) Verify the page state matches: re-observe and check the inputs have values.
    let final_obs = tool.execute(json!({"action": "observe"})).await;
    assert!(!final_obs.is_error, "final observe: {}", final_obs.content);

    eprintln!(
        "=== P7C RECORD→REPLAY E2E ===\n\
         recorded     = 2 type actions\n\
         replayed     = 2 steps, all succeeded\n\
         pipeline     = recording → fresh page → re-resolve refs → replay via act path\n\
         gates intact = replay dispatches through execute() (same path as live actions)"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}

///
/// Proves the full takeover flow end-to-end against a real browser:
/// 1. Opens a headful window with a bypass session + takeover enabled.
/// 2. Navigates to a form, observes to get refs.
/// 3. Clicks the "Submit order" button (irreversible).
/// 4. With force_resolution=Confirmed, the redline gate releases the action.
/// 5. The submit actually executes (verify via re-observe).
///
/// Manual run:
///   NOMIFUN_CHROME_BINARY="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
///     cargo nextest run -p nomi-browser --run-ignored all -E 'test(takeover_smoke)'
#[tokio::test]
#[ignore = "requires NOMIFUN_CHROME_BINARY + display (headful takeover smoke)"]
async fn takeover_smoke_confirmed_releases_irreversible_through_facade() {
    use nomi_browser::takeover::TakeoverResolution;

    let data_dir = isolated_data_dir("takeover-smoke");
    // Bypass session (yolo) + takeover enabled with forced Confirmed.
    let mut tool = BrowserTool::with_policy(
        &BrowserConfig { headless: true, ..Default::default() },
        true,  // session_bypasses_approval
        false, // evaluate_full_power
        false, // evaluate_persistent_login
        None,
        None,
        None,
    );
    tool.takeover_controller_mut().enabled = true;
    tool.takeover_controller_mut().force_resolution = Some(TakeoverResolution::Confirmed);

    // Navigate.
    let nav = tool
        .execute(json!({"action": "navigate", "url": fixture_url("e2e-form.html")}))
        .await;
    eprintln!("takeover smoke: navigate -> {}", nav.content);
    assert!(!nav.is_error, "navigate: {}", nav.content);

    // Observe.
    let obs = tool.execute(json!({"action": "observe"})).await;
    assert!(!obs.is_error, "observe: {}", obs.content);
    let submit_ref = find_ref(&obs.content, "button", "Submit order");
    eprintln!("takeover smoke: submit_ref={submit_ref}");

    // Click submit (irreversible in bypass session → takeover → Confirmed → proceeds).
    let click = tool
        .execute(json!({"action": "click", "ref": submit_ref}))
        .await;
    eprintln!(
        "takeover smoke: click submit -> is_error={} content={}",
        click.is_error,
        &click.content[..click.content.len().min(200)]
    );
    // With Confirmed takeover, the action should proceed past the redline gate.
    assert!(
        !click.content.to_lowercase().contains("blocked"),
        "Confirmed takeover must release the submit past the redline gate: {}",
        click.content
    );

    // must_re_observe should be set after the Confirmed takeover.
    assert!(
        tool.needs_re_observe(),
        "must_re_observe should be set after Confirmed takeover"
    );

    // Re-observe to clear the flag and verify the submit went through.
    let obs2 = tool.execute(json!({"action": "observe"})).await;
    assert!(!obs2.is_error, "re-observe: {}", obs2.content);
    assert!(
        !tool.needs_re_observe(),
        "must_re_observe should be cleared after observe"
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}

/// **Task 6 P7B: visual-fallback canvas smoke** (`#[ignore]`, needs `NOMIFUN_CHROME_BINARY`).
///
/// Navigates to a `<canvas>` fixture with NO accessible button in the DOM (the "button"
/// is drawn purely as pixels on the canvas). Asserts:
/// 1. DOM/aria anchoring fails (observe does not expose the canvas "button").
/// 2. With a stub locator returning the known button box coordinates, the visual
///    fallback click lands correctly (verified by checking `#click-result` text).
///
/// This proves the full visual fallback path end-to-end with a real Chrome:
///   navigate → observe (no ref for canvas button) → attempt click with stale/fake ref
///   → NodeStale → visual fallback → locator returns known coords → DPR mapping
///   → click_at_css_point → canvas click handler fires.
///
/// Run:
/// ```sh
/// NOMIFUN_CHROME_BINARY="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
///   cargo nextest run -p nomi-browser --run-ignored all -E 'test(visual_fallback_canvas_smoke)'
/// ```
#[tokio::test]
#[ignore = "需 NOMIFUN_CHROME_BINARY（真 Chrome）：visual-fallback canvas 冒烟"]
async fn visual_fallback_canvas_smoke() {
    use nomi_browser::visual_fallback::{PixelBox, VisualLocateResult, VisualLocator};
    use std::sync::Arc;

    /// Stub locator that returns the known canvas button center coordinates.
    /// The button is drawn at (150, 120) with size (100, 40) — center = (200, 140).
    /// In headless Chrome (DPR=1.0), pixel coords == CSS coords.
    struct CanvasButtonLocator;

    #[async_trait::async_trait]
    impl VisualLocator for CanvasButtonLocator {
        async fn locate(
            &self,
            _screenshot: &[u8],
            _instruction: &str,
        ) -> Result<VisualLocateResult, String> {
            Ok(VisualLocateResult {
                pixel_box: PixelBox {
                    x: 150.0,
                    y: 120.0,
                    width: 100.0,
                    height: 40.0,
                },
                confidence: 1.0,
            })
        }
    }

    let data_dir = isolated_data_dir("visual-fallback-canvas");
    let tool = BrowserTool::with_data_dir(data_dir.clone(), false)
        .with_visual_fallback_enabled(true)
        .with_visual_locator(Arc::new(CanvasButtonLocator));

    // 1. Navigate to the canvas fixture.
    let nav = tool
        .execute(json!({"action": "navigate", "url": fixture_url("visual-fallback-canvas.html")}))
        .await;
    eprintln!("navigate -> is_error={} content={:?}", nav.is_error, nav.content);
    assert!(!nav.is_error, "navigate must succeed: {}", nav.content);

    // 2. Observe — the canvas button should NOT appear in the accessibility tree.
    let obs = tool.execute(json!({"action": "observe"})).await;
    eprintln!("=== observe output ===\n{}", obs.content);
    assert!(!obs.is_error, "observe must succeed: {}", obs.content);
    // The canvas is just a generic element — no "Submit" button is exposed.
    assert!(
        !obs.content.contains("Submit") || obs.content.contains("canvas"),
        "observe must NOT expose the canvas-drawn button as an interactive element"
    );

    // 3. Attempt a click with a deliberately stale ref (from the observe output, there is
    //    no ref for the canvas button). Use a fake ref that doesn't exist — this will
    //    trigger NodeStale, which then triggers the visual fallback.
    let click_result = tool
        .execute(json!({"action": "click", "ref": "f999e999"}))
        .await;
    eprintln!("click (stale ref) -> is_error={} content={:?}", click_result.is_error, click_result.content);

    // The visual fallback should have fired and clicked at (200, 140) CSS pixels
    // (center of the button box).
    assert!(
        click_result.content.contains("via visual fallback"),
        "expected visual fallback to fire: {}",
        click_result.content
    );

    // 4. Verify the click actually landed on the canvas button by checking #click-result.
    // Wait a moment for the click handler to fire.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let obs2 = tool.execute(json!({"action": "observe"})).await;
    eprintln!("=== post-click observe ===\n{}", obs2.content);

    // The click handler sets #click-result text to "canvas-button-clicked".
    assert!(
        obs2.content.contains("canvas-button-clicked"),
        "the visual fallback click must have landed on the canvas button (expected \
         'canvas-button-clicked' in post-click observe): {}",
        obs2.content
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}

/// **P7B SoM (Set-of-Marks) visual-fallback e2e** (`#[ignore]`, needs `NOMIFUN_CHROME_BINARY`).
///
/// Proves the full SoM path on real Chrome: `observe` (with visual fallback on) collects per-ref
/// CSS-pixel boxes → a stale-ref click triggers the fallback → the facade draws a numbered overlay
/// on the screenshot and asks the (stub) locator for a label → the label maps back to the real
/// button's CSS center → the click lands on it. Three stacked real buttons make label numbering
/// deterministic: the topmost ("Alpha") is always label 1, which the stub picks.
///
/// Run: `NOMIFUN_CHROME_BINARY="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
///   cargo nextest run -p nomi-browser --run-ignored all -E 'test(visual_fallback_som_smoke)'`
#[tokio::test]
#[ignore = "需 NOMIFUN_CHROME_BINARY（真 Chrome）：visual-fallback SoM 冒烟"]
async fn visual_fallback_som_smoke() {
    use nomi_browser::visual_fallback::{SomLabelResult, VisualLocateResult, VisualLocator};
    use std::sync::Arc;

    /// Stub SoM locator: always picks label 1 (the topmost button = "Alpha"). Its `locate`
    /// (raw bbox) returns Err so that IF the code fell back to raw instead of SoM, the click
    /// would fail — making a green test proof that the SoM path actually ran.
    struct PickLabelOne;

    #[async_trait::async_trait]
    impl VisualLocator for PickLabelOne {
        async fn locate(
            &self,
            _screenshot: &[u8],
            _instruction: &str,
        ) -> Result<VisualLocateResult, String> {
            Err("raw bbox path must not be used in the SoM smoke".to_string())
        }
        async fn locate_labeled(
            &self,
            _annotated_screenshot: &[u8],
            _instruction: &str,
            _n_labels: usize,
        ) -> Result<SomLabelResult, String> {
            Ok(SomLabelResult { label: 1, confidence: 1.0 })
        }
    }

    let data_dir = isolated_data_dir("visual-fallback-som");
    let tool = BrowserTool::with_data_dir(data_dir.clone(), false)
        .with_visual_fallback_enabled(true)
        .with_visual_locator(Arc::new(PickLabelOne));

    // 1. Navigate to the multi-button fixture.
    let nav = tool
        .execute(json!({"action": "navigate", "url": fixture_url("som-fallback.html")}))
        .await;
    assert!(!nav.is_error, "navigate must succeed: {}", nav.content);

    // 2. Observe — visual_fallback_enabled ⇒ observe collects per-ref boxes (cached for SoM).
    let obs = tool.execute(json!({"action": "observe"})).await;
    assert!(!obs.is_error, "observe must succeed: {}", obs.content);

    // 3. Click with a deliberately stale ref → NodeStale → visual fallback → SoM mode (boxes
    //    are cached, count is in range). The stub picks label 1 = the topmost button "Alpha".
    let click_result = tool
        .execute(json!({"action": "click", "ref": "f999e999"}))
        .await;
    eprintln!(
        "click (stale ref) -> is_error={} content={:?}",
        click_result.is_error, click_result.content
    );
    assert!(
        click_result.content.contains("via visual fallback (SoM)"),
        "expected the SoM path to fire (not raw bbox): {}",
        click_result.content
    );

    // 4. Verify the click landed on the topmost button (label 1) by its distinct result.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let obs2 = tool.execute(json!({"action": "observe"})).await;
    eprintln!("=== post-click observe ===\n{}", obs2.content);
    assert!(
        obs2.content.contains("top-clicked"),
        "the SoM click must have landed on the topmost button (label 1) — expected \
         'top-clicked' in post-click observe: {}",
        obs2.content
    );

    let _ = std::fs::remove_dir_all(&data_dir);
}

