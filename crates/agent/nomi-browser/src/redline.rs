//! E2 —— 不可逆动作分类器 + facade 独立 fail-closed 强制门（Stage E 安全 keystone）。
//!
//! # 为什么需要一道**独立**门（设计裁决⑧）
//!
//! orchestration 的审批闸（`category_for` → 普通会话弹审批）会被三条路径**旁路**：
//! `auto_approve`（orchestration.rs:271）、`SessionMode::Yolo`（lib.rs:100-104）、companion
//! 强制 yolo（companion.rs:281-287）。yolo 下**一切自动批准**。若一个 IRREVERSIBLE 浏览器
//! 动作（提交支付表单 / 删除 / 发送）只靠普通 orchestration 审批，yolo 下会**静默自动执行**——
//! 这是红线事故。
//!
//! 故 E2 在 facade（[`crate::tool::BrowserTool::execute`]）里加一道**不经 orchestration** 的
//! 强制门 [`enforce_redline`]：
//!
//! - **普通会话（非 yolo，审批未旁路）**：IRREVERSIBLE 动作经 [`classify_action`] 判
//!   [`ApprovalTier::Irreversible`] → `category_for` 返 [`ToolCategory::Irreversible`] →
//!   orchestration 正常弹审批（用户确认）。facade 门**不拦**（`session_bypasses_approval==false`）。
//! - **yolo / companion 会话（orchestration 审批被旁路）**：facade 门**拦截** IRREVERSIBLE 动作 →
//!   **hard-deny [`BrowserError::Blocked`]**（因为正常审批被旁路了，不能让它静默执行）。
//!
//! **带外确认**（headful takeover 原生 dialog / 网关手机审批）是 yolo 下唯一放行路径——但那是
//! **P3**。P2 没有带外确认机制，故 yolo 下 IRREVERSIBLE 恒 = Blocked（fail-closed）。
//!
//! 即：**红线动作只在 yolo/companion 下 hard-deny，不靠被旁路的 orchestration 闸**——门拦的是
//! 「审批被旁路的会话里的不可逆动作」，**不是**「所有不可逆动作」（普通会话交 orchestration）。
//!
//! 镜像 IDMM [`PermissionConfirm{safe_value:None}`](nomifun-idmm::signal)：不可逆动作**无**
//! `safe_value` 自动放行（只有 Read 类才有 safe_value）。这里同构——IRREVERSIBLE 在审批旁路会话
//! 里没有「保守安全自动放行值」，唯一放行是带外确认（P3）。
//!
//! 全模块**纯逻辑**（不进浏览器，元素 accname/role/origin/会话标志作入参），充分单测。

use nomi_browser_engine::BrowserError;
use nomi_protocol::events::ToolCategory;

/// 一次动作的审批等级（与 [`ToolCategory`] 对齐，加 [`ApprovalTier::Irreversible`] 最高级）。
///
/// 分类器 [`classify_action`] 产出本枚举；[`ApprovalTier::to_category`] 把它投影回
/// [`ToolCategory`]（让 orchestration 普通会话能据类别审批），[`enforce_redline`] 据它决定
/// 是否在审批旁路会话里 hard-deny。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalTier {
    /// 只读（navigate / observe / screenshot / get_page_text / search_page / …）：无副作用。
    Info,
    /// 轻写（导航类的良性 settle 等，本 facade 暂未细分到 Edit；保留对齐 ToolCategory）。
    Edit,
    /// 一般写（普通 click / type / scroll / select 等可逆交互）。
    Exec,
    /// 不可逆（submit / 付款 / 删除 / 发送 / 跨域 POST / Enter 落 form / POST 页 reload）：
    /// 最高审批级，审批旁路会话里 hard-deny（带外确认 P3 是唯一放行）。
    Irreversible,
}

impl ApprovalTier {
    /// 投影回 [`ToolCategory`]（orchestration 据类别审批；普通会话 Irreversible→用户确认）。
    pub fn to_category(self) -> ToolCategory {
        match self {
            ApprovalTier::Info => ToolCategory::Info,
            ApprovalTier::Edit => ToolCategory::Edit,
            ApprovalTier::Exec => ToolCategory::Exec,
            ApprovalTier::Irreversible => ToolCategory::Irreversible,
        }
    }
}

/// 分类器据以判 tier 的运行时上下文（**纯入参**，由 facade 在 dispatch 前 best-effort 采集）。
///
/// E2 把所有「运行时才知道」的危险信号收进本结构作纯函数入参，让 [`classify_action`] 保持纯逻辑、
/// 充分单测。各字段的采集点（last_snapshot 按 ref 查 accname/role、注入查 focus-in-form、
/// getNavigationHistory 查 POST 页、firewall::is_cross_origin 判跨域）在 F1 接线时填实；E2 只定义
/// 分类逻辑 + 单测。
#[derive(Clone, Debug, Default)]
pub struct ActionContext {
    /// 点击/交互目标元素的 accessible name（从最近一次 observe 的 [`nomi_browser_engine::Observation`]
    /// 按 ref 查）。空 = 未知 / 无名（保守不据 accname 升级）。
    pub element_accname: Option<String>,
    /// 目标元素的 role（同上按 ref 查）。`button` + `submit` 语义 / link 等。
    pub element_role: Option<String>,
    /// 目标元素是否是 `<button type=submit>` / `<input type=submit>`（form submit 触发器）。
    /// 由 last_snapshot 的元素属性（或注入查 `el.type==='submit'`）判，F1 填实。
    pub is_submit_control: bool,
    /// 本次动作会触发一个**跨域 POST**（接 E5 [`nomi_browser_engine::firewall::is_cross_origin`]
    /// + 含 body 的写）。F1/E5 出口防火墙在 dispatch 前判，填这里。
    pub is_cross_origin_post: bool,
    /// press_key 的裸 Enter 落在 `<form>` 内（隐式提交风险，复用 C2
    /// [`nomi_browser_engine::actions::press_key_is_irreversible`] 的判定）。
    pub enter_submits_form: bool,
    /// reload 一个 POST 表单提交来的页面（重提交风险，复用 D4
    /// [`nomi_browser_engine::nav::current_entry_is_post`] 的判定）。
    pub reload_resubmits_post: bool,
}

/// 不可逆动词词表（中英）：accessible name 含其一即按不可逆触发器升级（DESIGN §⑧/§16
/// 「submit/付款/删除/发送/确认」）。
///
/// 收**动作语义**词根（而非泛词），降低误判普通按钮（如「显示更多」/"Show more"）的概率。大小写
/// 不敏感子串匹配（英文）；中文逐字含子串匹配。**保守过判优于漏判**——宁可让个别良性按钮多过一道
/// 确认（普通会话只是弹审批，yolo 下被拦但 P3 带外确认放行），也不漏判一个真支付/删除/发送。
const IRREVERSIBLE_EN_WORDS: &[&str] = &[
    "pay",       // pay / payment / pay now（含 "pay" 子串；"display"/"replay" 见下负向词规避）
    "purchase",  //
    "checkout",  // 结账
    "buy",       // 下单
    "order now", // 立即下单（"order" 单独太泛——"order by"/"in order to"，故收短语）
    "place order",
    "submit",  // 提交
    "confirm", // 确认
    "delete",  // 删除
    "remove",  // 移除（删除类）
    "send",    // 发送
    "transfer", // 转账
    "withdraw", // 提现
    "subscribe", // 订阅（产生费用/绑定）
    "sign contract",
    "agree and",  // "agree and continue/pay" 类
];

/// 不可逆中文词表（逐字含子串）：付款/支付/删除/发送/确认/提交/购买/下单/转账/提现/订阅/结账。
const IRREVERSIBLE_CN_WORDS: &[&str] = &[
    "付款", "支付", "删除", "移除", "发送", "发布", "确认", "提交", "购买", "下单", "结账", "结算",
    "转账", "提现", "订阅", "立即购买", "确定支付", "同意并",
];

/// 英文负向词（含这些词根时，**即便**命中某不可逆词根也不升级——避免 "display"/"replay" 因含 "pay"
/// 被误判）。仅用于消解 "pay" 子串的常见误命中（display/replay/payment 本身是付款不在此列）。
const EN_FALSE_POSITIVE_HINTS: &[&str] = &["display", "replay", "repaper"];

/// **[纯逻辑] accessible name 是否含不可逆触发词**（中英；大小写不敏感）。
///
/// 算法：
/// 1. 英文：lower-case 后，先排除明显误命中（含 [`EN_FALSE_POSITIVE_HINTS`] 词根**且**不含其它
///    独立不可逆词的，视作非不可逆）；否则任一 [`IRREVERSIBLE_EN_WORDS`] 子串命中 → true。
/// 2. 中文：原串含任一 [`IRREVERSIBLE_CN_WORDS`] 子串 → true。
///
/// 空串 / 全空白 → false（无名按钮不据 accname 升级——交由 `is_submit_control` 等其它信号判）。
pub fn accname_is_irreversible(accname: &str) -> bool {
    let trimmed = accname.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_lowercase();

    // 中文：逐字含子串（中文无大小写，用原 trimmed 匹配）。
    if IRREVERSIBLE_CN_WORDS.iter().any(|w| trimmed.contains(w)) {
        return true;
    }

    // 英文命中。
    let en_hit = IRREVERSIBLE_EN_WORDS.iter().any(|w| lower.contains(w));
    if !en_hit {
        return false;
    }

    // "pay" 子串误命中消解：若命中仅因含 display/replay 这类词根，且不含**其它**独立不可逆信号，
    // 则不升级。先看是否含负向词根；含则要求另有一个非 "pay" 的不可逆词命中才算真不可逆。
    let has_fp_hint = EN_FALSE_POSITIVE_HINTS
        .iter()
        .any(|fp| lower.contains(fp));
    if has_fp_hint {
        // 另需一个非 "pay" 的不可逆词命中（如 "display and submit" 仍升级；纯 "display" 不升级）。
        let non_pay_hit = IRREVERSIBLE_EN_WORDS
            .iter()
            .filter(|w| **w != "pay")
            .any(|w| lower.contains(w));
        return non_pay_hit;
    }

    true
}

/// **[纯逻辑] 不可逆动作分类器**（设计裁决⑧核心）：据 facade 动作名 + [`ActionContext`] 运行时信号
/// 判 [`ApprovalTier`]。
///
/// `action` 是 facade 的 `input["action"]` 动作名（navigate/observe/click/type/press_key/reload/…）。
/// `ctx` 携带运行时危险信号（元素 accname/role、跨域 POST、Enter-落-form、POST 页 reload）。
///
/// 判 [`ApprovalTier::Irreversible`] 的信号（DESIGN §9/§16/§⑧）：
/// - **click**：目标是 submit 控件（[`ActionContext::is_submit_control`]）/ accname 含付款删除发送确认类
///   词（[`accname_is_irreversible`]，中英）/ 本次点击触发跨域 POST（[`ActionContext::is_cross_origin_post`]）。
/// - **press_key**：裸 Enter 落 form（[`ActionContext::enter_submits_form`]，复用 C2 判定）。
/// - **reload**：reload 一个 POST 提交来的页（[`ActionContext::reload_resubmits_post`]，复用 D4 判定）。
/// - **任何动作触发跨域 POST**（[`ActionContext::is_cross_origin_post`]，接 E5）→ Irreversible。
///
/// 只读类（navigate/observe/screenshot/get_page_text/search_page/find_elements/get_dropdown_options/
/// cursor/wait/wait_for/capabilities/tabs/**extract**）→ [`ApprovalTier::Info`]。
/// 普通 type/set_value/hover/select_option/scroll/click（非危险）/ upload_file / download /
/// save_as_pdf → [`ApprovalTier::Exec`]。
///
/// **纯函数**：元素 accname/role/origin 全由 `ctx` 携带，无副作用，充分单测。
pub fn classify_action(action: &str, ctx: &ActionContext) -> ApprovalTier {
    // 跨域 POST 任何承载它的动作都升不可逆（接 E5；与 click 的 submit 信号独立）。
    if ctx.is_cross_origin_post {
        return ApprovalTier::Irreversible;
    }

    match action {
        // ── 只读类（Info，零副作用）──────────────────────────────────────────
        // extract = deterministic 页面表示捕获（aria snapshot + 可见文本，redact+wrap），只读零写。
        "navigate" | "observe" | "screenshot" | "capabilities" | "get_page_text" | "search_page"
        | "find_elements" | "get_dropdown_options" | "cursor" | "wait" | "wait_for" | "tabs"
        | "extract" | "get_console_logs" | "get_page_errors" | "get_network_log" => {
            ApprovalTier::Info
        }

        // ── click：submit 控件 / 危险 accname → Irreversible；否则 Exec ────────────
        "click" => {
            if ctx.is_submit_control {
                return ApprovalTier::Irreversible;
            }
            if let Some(accname) = ctx.element_accname.as_deref()
                && accname_is_irreversible(accname)
            {
                return ApprovalTier::Irreversible;
            }
            ApprovalTier::Exec
        }

        // ── press_key：裸 Enter 落 form（隐式提交）→ Irreversible；否则 Exec ──────────
        "press_key" => {
            if ctx.enter_submits_form {
                ApprovalTier::Irreversible
            } else {
                ApprovalTier::Exec
            }
        }

        // ── reload：POST 页 reload（重提交）→ Irreversible；否则导航类（Exec）────────
        "reload" => {
            if ctx.reload_resubmits_post {
                ApprovalTier::Irreversible
            } else {
                ApprovalTier::Exec
            }
        }

        // ── 一般写交互（可逆）→ Exec ───────────────────────────────────────────
        // type/set_value/hover/select_option/scroll/scroll_to_text/upload_file/download/save_as_pdf/
        // back/forward/switch_tab/close_tab/open_link_new_tab/switch_frame/
        // evaluate（evaluate 另有 E3 门控，这里仅给类别）。extract 是 Info（见上，只读零写）。
        _ => ApprovalTier::Exec,
    }
}

/// **[纯逻辑] facade 独立 fail-closed 强制门**（设计裁决⑧关键）。
///
/// `tier`：[`classify_action`] 判出的动作审批级。
/// `session_bypasses_approval`：本会话的 orchestration 审批闸是否被旁路——
/// `yolo || companion-forced-yolo || auto_approve`（见模块级文档的三条旁路）。
/// `out_of_band_confirmed`：是否已获**带外确认**（headful takeover 原生 dialog / 网关手机审批）。
/// **P2 恒 `false`**（带外确认机制 P3 才接）。
///
/// 门逻辑（**只**拦审批旁路会话里的不可逆动作）：
/// - `tier == Irreversible && session_bypasses_approval && !out_of_band_confirmed`
///   → `Err(BrowserError::Blocked{reason})`（hard-deny，**不经 orchestration**）。
/// - 其它一切 → `Ok(())`：
///   - **普通会话**（`!session_bypasses_approval`）的 Irreversible → Ok（交 orchestration 正常审批，
///     facade 门不拦）；
///   - **任何会话**的非 Irreversible（Info/Edit/Exec）→ Ok（良性/可逆动作不拦）；
///   - 已**带外确认**的 Irreversible → Ok（P3 放行路径）。
///
/// 即：门拦的是「审批被旁路的会话里的不可逆动作」，**不是**「所有不可逆动作」——方向勿搞反。
pub fn enforce_redline(
    tier: ApprovalTier,
    session_bypasses_approval: bool,
    out_of_band_confirmed: bool,
) -> Result<(), BrowserError> {
    if tier == ApprovalTier::Irreversible && session_bypasses_approval && !out_of_band_confirmed {
        return Err(BrowserError::Blocked {
            reason: "irreversible browser action (submit / payment / delete / send) blocked in an \
                     auto-approving session (yolo/companion): orchestration approval is bypassed \
                     here, so this fail-closed gate denies it. Out-of-band confirmation (headful \
                     takeover dialog / gateway phone approval) is the only way to allow it — that \
                     lands in P3."
                .to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── accname_is_irreversible：付款/删除/发送/确认/提交（中英）→ true；良性 → false ──

    #[test]
    fn accname_irreversible_english_payment_delete_send() {
        for name in [
            "Pay now",
            "Pay $49.99",
            "Complete purchase",
            "Checkout",
            "Submit order",
            "Confirm and pay",
            "Delete account",
            "Remove item permanently",
            "Send message",
            "Transfer funds",
            "Withdraw",
            "Place order",
        ] {
            assert!(
                accname_is_irreversible(name),
                "{name:?} should be irreversible"
            );
        }
    }

    #[test]
    fn accname_irreversible_chinese_payment_delete_send() {
        for name in [
            "立即支付",
            "确认付款",
            "删除账户",
            "永久移除",
            "发送",
            "提交订单",
            "立即购买",
            "确定支付",
            "结账",
            "转账",
            "提现",
        ] {
            assert!(
                accname_is_irreversible(name),
                "{name:?} should be irreversible (CN)"
            );
        }
    }

    #[test]
    fn accname_benign_buttons_are_not_irreversible() {
        for name in [
            "Show more",
            "Load more",
            "Next",
            "Back",
            "Cancel",
            "Close",
            "Expand",
            "Filter",
            "Search",
            "搜索",
            "展开",
            "下一页",
            "取消",
            "关闭",
            "更多",
        ] {
            assert!(
                !accname_is_irreversible(name),
                "{name:?} should NOT be irreversible"
            );
        }
    }

    #[test]
    fn accname_pay_substring_false_positives_are_regected() {
        // "display"/"replay" 含 "pay" 子串但不是付款——不升级（除非另有独立不可逆词）。
        assert!(!accname_is_irreversible("Display options"));
        assert!(!accname_is_irreversible("Replay video"));
        assert!(!accname_is_irreversible("Display"));
        // 但 "display and submit" 仍升级（另有 "submit" 独立命中）。
        assert!(accname_is_irreversible("Display and submit"));
    }

    #[test]
    fn accname_empty_or_whitespace_is_not_irreversible() {
        assert!(!accname_is_irreversible(""));
        assert!(!accname_is_irreversible("   "));
        assert!(!accname_is_irreversible("\t\n"));
    }

    // ── classify_action：submit 按钮 / 危险 accname / 跨域 POST / Enter-form / POST reload → Irreversible ──

    #[test]
    fn classify_click_submit_button_is_irreversible() {
        let ctx = ActionContext {
            is_submit_control: true,
            ..Default::default()
        };
        assert_eq!(classify_action("click", &ctx), ApprovalTier::Irreversible);
    }

    #[test]
    fn classify_click_pay_now_accname_is_irreversible() {
        let ctx = ActionContext {
            element_accname: Some("Pay now".to_string()),
            ..Default::default()
        };
        assert_eq!(classify_action("click", &ctx), ApprovalTier::Irreversible);
    }

    #[test]
    fn classify_click_delete_account_cn_accname_is_irreversible() {
        let ctx = ActionContext {
            element_accname: Some("删除账户".to_string()),
            ..Default::default()
        };
        assert_eq!(classify_action("click", &ctx), ApprovalTier::Irreversible);
    }

    #[test]
    fn classify_click_benign_show_more_is_exec() {
        let ctx = ActionContext {
            element_accname: Some("Show more".to_string()),
            ..Default::default()
        };
        assert_eq!(classify_action("click", &ctx), ApprovalTier::Exec);
    }

    #[test]
    fn classify_click_no_accname_no_submit_is_exec() {
        // 无 accname + 非 submit 控件 → 普通可逆点击（Exec），不据缺信息升级。
        let ctx = ActionContext::default();
        assert_eq!(classify_action("click", &ctx), ApprovalTier::Exec);
    }

    #[test]
    fn classify_cross_origin_post_is_irreversible_on_any_action() {
        // 跨域 POST（接 E5）任何承载它的动作都升不可逆——即便是 type/click/navigate。
        let ctx = ActionContext {
            is_cross_origin_post: true,
            ..Default::default()
        };
        assert_eq!(classify_action("click", &ctx), ApprovalTier::Irreversible);
        assert_eq!(classify_action("type", &ctx), ApprovalTier::Irreversible);
        assert_eq!(classify_action("navigate", &ctx), ApprovalTier::Irreversible);
    }

    #[test]
    fn classify_press_key_enter_in_form_is_irreversible() {
        let ctx = ActionContext {
            enter_submits_form: true,
            ..Default::default()
        };
        assert_eq!(classify_action("press_key", &ctx), ApprovalTier::Irreversible);
    }

    #[test]
    fn classify_press_key_not_in_form_is_exec() {
        let ctx = ActionContext {
            enter_submits_form: false,
            ..Default::default()
        };
        assert_eq!(classify_action("press_key", &ctx), ApprovalTier::Exec);
    }

    #[test]
    fn classify_reload_post_page_is_irreversible() {
        let ctx = ActionContext {
            reload_resubmits_post: true,
            ..Default::default()
        };
        assert_eq!(classify_action("reload", &ctx), ApprovalTier::Irreversible);
    }

    #[test]
    fn classify_reload_get_page_is_exec() {
        let ctx = ActionContext::default();
        assert_eq!(classify_action("reload", &ctx), ApprovalTier::Exec);
    }

    #[test]
    fn classify_readonly_actions_are_info() {
        let ctx = ActionContext::default();
        for action in [
            "navigate",
            "observe",
            "screenshot",
            "capabilities",
            "get_page_text",
            "search_page",
            "find_elements",
            "get_dropdown_options",
            "cursor",
            "wait",
            "wait_for",
            "tabs",
            "extract",
            "get_console_logs",
            "get_page_errors",
            "get_network_log",
        ] {
            assert_eq!(
                classify_action(action, &ctx),
                ApprovalTier::Info,
                "{action} should be Info"
            );
        }
    }

    #[test]
    fn classify_ordinary_writes_are_exec() {
        let ctx = ActionContext::default();
        for action in [
            "type",
            "set_value",
            "hover",
            "select_option",
            "scroll",
            "scroll_to_text",
            "back",
            "forward",
            "switch_tab",
            "switch_frame",
            "upload_file",
            "download",
            "save_as_pdf",
        ] {
            assert_eq!(
                classify_action(action, &ctx),
                ApprovalTier::Exec,
                "{action} should be Exec"
            );
        }
    }

    // ── ApprovalTier → ToolCategory 投影 ──────────────────────────────────────

    #[test]
    fn tier_maps_to_tool_category() {
        assert_eq!(ApprovalTier::Info.to_category(), ToolCategory::Info);
        assert_eq!(ApprovalTier::Edit.to_category(), ToolCategory::Edit);
        assert_eq!(ApprovalTier::Exec.to_category(), ToolCategory::Exec);
        assert_eq!(
            ApprovalTier::Irreversible.to_category(),
            ToolCategory::Irreversible
        );
    }

    // ── enforce_redline：红线方向（拦 yolo 下 irreversible，非拦所有 irreversible）─────────

    #[test]
    fn enforce_redline_blocks_irreversible_in_bypassing_session() {
        // yolo/companion（审批旁路）+ 不可逆 + 无带外确认 → Blocked（hard-deny，不经 orchestration）。
        let r = enforce_redline(ApprovalTier::Irreversible, true, false);
        assert!(
            matches!(r, Err(BrowserError::Blocked { .. })),
            "irreversible in a bypassing session must be hard-denied, got {r:?}"
        );
    }

    #[test]
    fn enforce_redline_allows_irreversible_in_normal_session() {
        // 普通会话（审批未旁路）+ 不可逆 → Ok：facade 门不拦，交 orchestration 正常审批。
        let r = enforce_redline(ApprovalTier::Irreversible, false, false);
        assert!(
            r.is_ok(),
            "irreversible in a normal session must pass the facade gate (orchestration approves), \
             got {r:?}"
        );
    }

    #[test]
    fn enforce_redline_allows_exec_in_bypassing_session() {
        // yolo + 可逆动作（Exec）→ Ok：门只拦不可逆，不拦良性/可逆。
        assert!(enforce_redline(ApprovalTier::Exec, true, false).is_ok());
        assert!(enforce_redline(ApprovalTier::Edit, true, false).is_ok());
        assert!(enforce_redline(ApprovalTier::Info, true, false).is_ok());
    }

    #[test]
    fn enforce_redline_allows_irreversible_with_out_of_band_confirmation() {
        // 带外确认（P3 路径）放行：即便 yolo + 不可逆，已确认 → Ok。
        let r = enforce_redline(ApprovalTier::Irreversible, true, true);
        assert!(
            r.is_ok(),
            "out-of-band-confirmed irreversible must pass (P3 release path), got {r:?}"
        );
    }

    #[test]
    fn enforce_redline_benign_passes_in_any_session() {
        // 边界：良性/可逆动作在任何会话（旁路 / 普通）都放行。
        for bypass in [true, false] {
            for confirmed in [true, false] {
                assert!(enforce_redline(ApprovalTier::Info, bypass, confirmed).is_ok());
                assert!(enforce_redline(ApprovalTier::Exec, bypass, confirmed).is_ok());
            }
        }
        // 普通会话的不可逆（未确认）也放行（交 orchestration）。
        assert!(enforce_redline(ApprovalTier::Irreversible, false, false).is_ok());
    }

    #[test]
    fn enforce_redline_block_reason_mentions_irreversible_and_p3() {
        // Blocked 文案含恢复语义关键词（让 LLM 知道为何被拦 + 唯一放行是带外确认 P3）。
        let Err(BrowserError::Blocked { reason }) =
            enforce_redline(ApprovalTier::Irreversible, true, false)
        else {
            panic!("expected Blocked");
        };
        let lower = reason.to_lowercase();
        assert!(lower.contains("irreversible"), "{reason}");
        assert!(
            lower.contains("out-of-band") || lower.contains("p3"),
            "{reason}"
        );
    }
}
