# 设计：桌面伙伴「总览」页自定义形象尺寸滑块

- 日期：2026-06-30
- 范围：仅 `character == "custom"`（用户上传的自定义形象）的伙伴
- 状态：待用户复审

## 1. 背景与问题

自定义形象桌宠此前没有可用的尺寸调节：尺寸只来自离散三档 `size_tier`（s/m/l），且
档位只藏在「形象库 → 编辑形象」弹窗里，桌宠本身/总览页没有入口（用户感知为「无法调整尺寸」）。
上一轮已把三档基准缩小到 `{ s:150, m:210, l:280 }`（见 `customDesk.ts`），解决了「默认太大」。

本设计在此基础上增加**按伙伴的连续尺寸微调**：在「总览」页就地放一个滑块，让用户在
**140–400px**（立绘逻辑高度）之间任意拖动调节**当前这个伙伴**的桌宠大小，实时生效。

## 2. 目标 / 非目标

**目标**
- 总览页内联滑块，仅对使用自定义形象的伙伴显示。
- 连续调节立绘高度 140–400px，所见即所得（页内预览随拖动缩放），桌宠窗口实时缩放。
- 按伙伴存储，互不影响（同一形象库形象被多个伙伴使用时，各自尺寸独立）。
- 向后兼容：旧配置不含该字段时回落到 `size_tier`；旧 JSON 字节不变（无迁移）。

**非目标**
- 不改内置角色（mochi/ink/bolt）的固定尺寸（本次范围只含自定义形象）。
- 不替换/废弃现有三档 `size_tier`——它仍作为创建向导 / 形象库的「模板默认值」。
- 不做自由拖拽窗口边缘 resize（窗口 `resizable(false)`，沿用滑块设值 + 程序化 setSize）。

## 3. 数据模型

### 3.1 后端 `CustomFigureMeta`（`crates/backend/nomifun-companion/src/profile.rs`）

新增**可选**字段：

```rust
/// Per-companion continuous figure-height override (logical px). When set, it
/// supersedes `size_tier` for THIS companion's desktop window. Absent ⇒ fall
/// back to the tier's height. skip_serializing_if keeps pre-slider configs
/// byte-identical (no migration).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub size_px: Option<f32>,
```

- `size_tier` 保留不变（仍是创建/形象库模板）。
- `figure_id` / `aspect` / `head_box` 不变。
- 仍走 `appearance.custom_figure`，故 `patch_companion` 与 WS `companion.config-updated` 链路天然适用。

### 3.2 前端镜像

- `ui/src/common/adapter/ipcBridge.ts`：`appearance.custom_figure` 的 wire 类型加 `size_px?: number | null`。
  `figureToCustomPatch` 返回值加 `size_px: null`：(重)指派形象时**显式清除**任何旧的按伙伴覆盖（RFC 7396 的
  `null` 删除键），让新形象从其档位默认高度起步——若只是“不带该键”，合并会**保留**旧覆盖，故必须显式 null。
- `ui/src/renderer/pages/companion/characters/types.ts`：`CustomFigureMeta` 加 `sizePx?: number`。
- `ui/src/renderer/pages/companion/characters/customMeta.ts`：`customFigureMetaOf` 读 `cf.size_px`
  （仅当为有限正数时取，否则不设）。

## 4. 尺寸计算（`customDesk.ts`）

新增/调整常量：

```ts
export const FIGURE_HEIGHTS = { s: 150, m: 210, l: 280 } as const; // 档位模板（不变）
export const SIZE_MIN = 140;   // 滑块下限；> BUST_MAX_SIZE(130) → 始终全身，不退化成头像裁剪
export const SIZE_MAX = 400;   // 滑块上限
export const MAX_WINDOW_WIDTH = 400; // ← 与 SIZE_MAX 同步，否则宽度会把高个立绘“压回去”
export const MIN_WINDOW_WIDTH = 240; // 不变
```

`customDeskSpec(meta)` 算法：

```
aspect = (finite && >0) ? meta.aspect : 1
baseHeight = (meta.sizePx finite && >0)
               ? clamp(meta.sizePx, SIZE_MIN, SIZE_MAX)
               : (FIGURE_HEIGHTS[meta.sizeTier] ?? FIGURE_HEIGHTS.m)
figureHeight = baseHeight
windowWidth  = ceil(figureHeight * aspect) + SIDE_MARGIN*2
if windowWidth > MAX_WINDOW_WIDTH:
    windowWidth  = MAX_WINDOW_WIDTH
    figureHeight = floor((MAX_WINDOW_WIDTH - SIDE_MARGIN*2) / aspect)  // 极宽横图仍兜底
windowWidth = max(windowWidth, MIN_WINDOW_WIDTH)
return { windowWidth, windowHeight: figureHeight + CHROME_HEIGHT, figureHeight }
```

要点：`MAX_WINDOW_WIDTH = SIZE_MAX` 使常见竖图（aspect < ~0.93）在滑到 400 时高度不被宽度钳制；
仅近正方形/横图在接近上限时略有收窄，宽幅横图仍受 400 兜底，避免横向铺满桌面。

## 5. 总览页 UI（`tabs/OverviewTab.tsx`）

- **仅当** `customFigureMetaOf(profile)` 非空（即 custom 形象）时渲染滑块区；内置角色不显示。
- 位置：头像那一列下方（头像 + 「调整形象」按钮 + **新增滑块**），与「在总览页面滑动调节」一致。
- 组件：Arco `Slider`，`min=SIZE_MIN max=SIZE_MAX step=4`；左右端注 `小 / 大`（复用 `sizeS`/`sizeL`），
  右侧显示当前 px 数值；附一个「复位」按钮（清除 `size_px`，回到档位高度）。
- 初值：`effectiveHeight = sizePx ?? FIGURE_HEIGHTS[sizeTier]`（150/210/280）。
- 所见即所得预览：头像在一个**底边对齐的固定高度槽**内按比例缩放
  `previewH = round(effectiveHeight / SIZE_MAX * PREVIEW_MAX)`（`PREVIEW_MAX ≈ 160`，最小floor ~40），
  随拖动即时变化（仿向导 `FrameStep` 的相对缩放，不跳版）。

## 6. 落库与实时联动

`patch_companion` 用 **RFC 7396 JSON merge patch**（对象递归合并、`null` 删除键、其余替换），
故只发**最小补丁**即可，沿用现有 onMoved 存坐标的写法：

- 拖动时：本地 state 即时更新预览；**防抖 ~400ms** 后落库。`ICompanionProfilePatch.appearance` 的
  `custom_figure` 不是部分类型，故发**整个 wire 对象的浅拷贝 + 覆盖 `size_px`**（RFC 7396 合并结果等价于
  只改 size_px，保留 aspect/head_box/size_tier/figure_id）：
  `patchCompanion({ appearance: { custom_figure: { ...profile.appearance.custom_figure, size_px } } })`。
- 「复位」：同形发 `{ ...custom_figure, size_px: null }` —— RFC 7396 的 `null` 删除该键，回落到档位高度。
- 桌宠实时缩放：`patch_companion` 广播 `companion.config-updated` → 桌宠窗口现有
  `onConfigUpdated → applyDeskSize` 重算 `customDeskSpec` 并 `setSize`（与拖拽存坐标同一条已验证回声链路）。
  无需新增事件或前端额外接线。

## 7. 与形象库同步的边界（已被合并语义自然满足）

`service.rs::sync_figure_to_active_companions`（编辑库形象 head_box/size_tier 时触发）下发的 patch **只含**
`aspect/head_box/size_tier/figure_id`，**从不提及 `size_px`**。在 RFC 7396 合并下，未提及的键会被**保留**，
因此各伙伴的 `size_px` 覆盖**已自动不被清零**——无需改动同步的生产代码。仅补一个回归测试锁死该行为
（编辑库形象档位后，原有 `size_px` 仍在）。

语义取舍：当伙伴存在 `size_px` 覆盖时，它在 `customDeskSpec` 里**优先于** `size_tier`；故库形象的档位变更
在该伙伴上要等用户「复位」后才显现——符合「手动设过尺寸，手动优先」的直觉。

## 8. i18n

复用：`nomi.customFigure.sizeLabel`（"桌面形象尺寸"）、`sizeS`（"小"）、`sizeL`（"大"）、`adjustFigure`。
新增（zh-CN + en-US 各一）：
- `nomi.customFigure.sizeReset` — "复位" / "Reset"
- （可选）`nomi.customFigure.sizePxValue` 若需带单位文案；px 纯数字可不走 i18n。
新增 key 后跑 `bun run gen:i18n` 重新生成 `i18n-keys.d.ts`。

## 9. 测试

- `customDesk.test.ts`：
  - `sizePx` 覆盖时用其作 figureHeight（如 sizePx=420, aspect 0.6 → figure 420, window 420+64=484, width=ceil(252)+28=280）。
  - `sizePx` 越界被钳制（如 1000 → 400；100 → 140）。
  - `sizePx` 缺省/非法（undefined / NaN / ≤0）回落到档位高度。
  - 高个 + 宽 aspect 触发 `MAX_WINDOW_WIDTH=400` 兜底并收窄 figure。
- `profile.rs`：`custom_figure` 往返测试覆盖 `size_px` 有值 / 为 None 时不出现在 JSON。
- `service.rs`：`update_figure` 同步保留既有 `size_px` 的回归测试。
- 前端结构性测试（可选，仿 `figureActionsVisual.test.ts`）：OverviewTab 含 Slider 且 gated on custom。

## 10. 改动文件清单（≈9）

后端：
1. `crates/backend/nomifun-companion/src/profile.rs` — `CustomFigureMeta.size_px` + 往返测试。
2. `crates/backend/nomifun-companion/src/service.rs` — **仅加回归测试**（同步保留 `size_px`）；无生产代码改动。

前端：
3. `ui/src/common/adapter/ipcBridge.ts` — wire 类型加 `size_px?`。
4. `ui/src/renderer/pages/companion/characters/types.ts` — `CustomFigureMeta.sizePx?`。
5. `ui/src/renderer/pages/companion/characters/customMeta.ts` — 读 `size_px`。
6. `ui/src/renderer/pages/companion/characters/customDesk.ts` — `SIZE_MIN/SIZE_MAX`、`MAX_WINDOW_WIDTH=400`、`customDeskSpec` 用 `sizePx`。
7. `ui/src/renderer/pages/nomi/tabs/OverviewTab.tsx` — 内联滑块 + 预览 + 防抖落库。
8. i18n：`zh-CN/nomi.json` + `en-US/nomi.json` 加 `sizeReset` 等；`bun run gen:i18n`。
9. 单测：`customDesk.test.ts`（+ 后端两处测试）。

## 11. 向后兼容 / 风险

- 无数据迁移：旧伙伴无 `size_px` → 回落档位（当前 150/210/280），行为同上一轮缩小后的状态。
- `MAX_WINDOW_WIDTH` 由 360 调到 400：仅影响**自定义形象**的宽幅图上限（至多 400）；内置角色走
  `DEFAULT_DESK`（240）不受影响。属预期（用户要更大）。
- 滑块下限 140 > `BUST_MAX_SIZE`(130)，保证全身渲染，不会意外切成头像裁剪。
- 验证：前端 `bun run build:ui` + `bun run typecheck`（注意当前环境缺 `@xyflow/react`，与本改动无关）；
  vitest 本地未装，单测算术按值复核 / CI 跑。
