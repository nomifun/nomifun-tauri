# Local Model Capability Center Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the vertically stacked local text, image, and OCR model sections with one capability-tabbed local AI center whose technical details expand only when needed.

**Architecture:** `LocalModelsContent` becomes a thin shell that owns all three existing controllers and keeps every implemented capability mounted while showing one panel at a time. Pure capability/view-state helpers are tested with `bun:test`; focused shared React components render tabs, summaries, and disclosures while existing hooks and backend contracts remain unchanged.

**Tech Stack:** React 19, TypeScript 5.8, SWR, Arco Design, Icon Park, UnoCSS, i18next, Bun test runner.

## Global Constraints

- Keep backend routes, IPC protocol types, catalogs, download orchestration, and storage formats unchanged.
- Preserve active downloads and polling when capability tabs switch by keeping implemented panels mounted.
- Show tabs in this exact order: Text Understanding, Image Generation, OCR, Speech Recognition, Speech Synthesis.
- Speech Recognition and Speech Synthesis remain disabled and display a localized “Planned” label and explanation.
- Secondary source, license, component, and third-party information is collapsed by default.
- Transfer and failure states force the relevant details disclosure open.
- Reuse theme tokens and existing Arco/Icon Park components; introduce no fixed light-only colors or new dependencies.

---

### Task 1: Capability and disclosure view state

**Files:**
- Create: `ui/src/renderer/pages/modelHub/localModelCapabilityView.ts`
- Create: `ui/src/renderer/pages/modelHub/localModelCapabilityView.test.ts`

**Interfaces:**
- Produces: `LocalModelCapabilityKey`, `LOCAL_MODEL_CAPABILITIES`, `ModelTransferPhase`, `detailsForcedOpen()`, and `capabilityActivity()`.
- Consumes: no React or backend runtime; pure strings and phase arrays only.

- [ ] **Step 1: Write the failing view-state tests**

```ts
import { describe, expect, test } from 'bun:test';
import {
  LOCAL_MODEL_CAPABILITIES,
  capabilityActivity,
  detailsForcedOpen,
} from './localModelCapabilityView';

describe('local model capability center view state', () => {
  test('keeps implemented and planned capabilities in product order', () => {
    expect(LOCAL_MODEL_CAPABILITIES.map(({ key, implemented }) => [key, implemented])).toEqual([
      ['text', true],
      ['image', true],
      ['ocr', true],
      ['speech_recognition', false],
      ['speech_synthesis', false],
    ]);
  });

  test('forces details open only for actionable transfer states', () => {
    expect(detailsForcedOpen('not_installed', false)).toBe(false);
    expect(detailsForcedOpen('installed', false)).toBe(false);
    expect(detailsForcedOpen('downloading', false)).toBe(true);
    expect(detailsForcedOpen('verifying', false)).toBe(true);
    expect(detailsForcedOpen('extracting', false)).toBe(true);
    expect(detailsForcedOpen('paused', false)).toBe(true);
    expect(detailsForcedOpen('failed', false)).toBe(true);
    expect(detailsForcedOpen('installed', true)).toBe(true);
  });

  test('summarizes hidden tab activity with errors taking precedence', () => {
    expect(capabilityActivity(['installed'], false)).toBe('idle');
    expect(capabilityActivity(['downloading'], false)).toBe('running');
    expect(capabilityActivity(['installed'], true)).toBe('error');
    expect(capabilityActivity(['downloading'], true)).toBe('error');
  });
});
```

- [ ] **Step 2: Run the test and verify it fails because the module does not exist**

Run: `bun test ui/src/renderer/pages/modelHub/localModelCapabilityView.test.ts`

Expected: FAIL with `Cannot find module './localModelCapabilityView'`.

- [ ] **Step 3: Implement the pure view-state module**

```ts
export type LocalModelCapabilityKey =
  | 'text'
  | 'image'
  | 'ocr'
  | 'speech_recognition'
  | 'speech_synthesis';

export interface LocalModelCapabilityDefinition {
  key: LocalModelCapabilityKey;
  implemented: boolean;
}

export const LOCAL_MODEL_CAPABILITIES: readonly LocalModelCapabilityDefinition[] = [
  { key: 'text', implemented: true },
  { key: 'image', implemented: true },
  { key: 'ocr', implemented: true },
  { key: 'speech_recognition', implemented: false },
  { key: 'speech_synthesis', implemented: false },
];

export type ModelTransferPhase =
  | 'not_installed'
  | 'downloading'
  | 'verifying'
  | 'extracting'
  | 'installed'
  | 'paused'
  | 'failed';

export type CapabilityActivity = 'idle' | 'running' | 'error';

export const detailsForcedOpen = (phase: ModelTransferPhase, hasError: boolean): boolean =>
  hasError || ['downloading', 'verifying', 'extracting', 'paused', 'failed'].includes(phase);

export const capabilityActivity = (
  phases: readonly ModelTransferPhase[],
  hasError: boolean
): CapabilityActivity => {
  if (hasError || phases.includes('failed')) return 'error';
  return phases.some((phase) => ['downloading', 'verifying', 'extracting'].includes(phase))
    ? 'running'
    : 'idle';
};
```

- [ ] **Step 4: Run the test and verify it passes**

Run: `bun test ui/src/renderer/pages/modelHub/localModelCapabilityView.test.ts`

Expected: 3 tests pass, 0 fail.

- [ ] **Step 5: Commit the view-state foundation**

```bash
git add ui/src/renderer/pages/modelHub/localModelCapabilityView.ts ui/src/renderer/pages/modelHub/localModelCapabilityView.test.ts
git commit -m "test(local-models): define capability center view state"
```

### Task 2: Shared capability-center UI and translations

**Files:**
- Create: `ui/src/renderer/pages/modelHub/LocalModelCapabilityTabs.tsx`
- Create: `ui/src/renderer/pages/modelHub/LocalModelCapabilitySummary.tsx`
- Create: `ui/src/renderer/pages/modelHub/LocalModelDetails.tsx`
- Modify: `ui/src/renderer/services/i18n/locales/en-US/settings.json`
- Modify: `ui/src/renderer/services/i18n/locales/zh-CN/settings.json`
- Regenerate: `ui/src/renderer/services/i18n/i18n-keys.d.ts`

**Interfaces:**
- Consumes: `LocalModelCapabilityKey`, `CapabilityActivity`, and `LOCAL_MODEL_CAPABILITIES` from Task 1.
- Produces: `LocalModelCapabilityTabs`, `LocalModelCapabilitySummary`, and `LocalModelDetails` React components.

- [ ] **Step 1: Add localized shell, tab, summary, and disclosure copy**

Add the following object below `settings.modelHub.local` in both locale files, translated equivalently:

```json
"capabilityCenter": {
  "subtitle": "Private local AI, downloaded only when you choose",
  "tabs": {
    "text": "Text Understanding",
    "image": "Image Generation",
    "ocr": "OCR",
    "speechRecognition": "Speech Recognition",
    "speechSynthesis": "Speech Synthesis"
  },
  "planned": "Planned",
  "plannedHint": "This local capability is planned and is not available in the current version.",
  "availableModels": "{{count}} available models",
  "installedModels": "{{count}} installed",
  "runtimeReady": "Runtime ready",
  "runtimeOnDemand": "Starts on demand",
  "details": "Model details",
  "collapseDetails": "Hide details",
  "backgroundRunning": "Download in progress",
  "needsAttention": "Needs attention"
}
```

Use these Chinese values: “本地隐私运行，仅在你选择时下载”, “文本理解”, “图片生成”, “OCR”, “语音识别”, “语音合成”, “规划中”, “该本地能力正在规划中，当前版本暂不可用”, “{{count}} 个可用模型”, “已安装 {{count}} 个”, “运行环境已就绪”, “使用时启动”, “模型详情”, “收起详情”, “正在后台下载”, and “需要处理”.

- [ ] **Step 2: Regenerate and validate i18n types**

Run: `bun scripts/generate-i18n-types.mjs`

Expected: `ui/src/renderer/services/i18n/i18n-keys.d.ts` contains `settings.modelHub.local.capabilityCenter.*` keys.

- [ ] **Step 3: Implement the capability tabs**

```tsx
interface LocalModelCapabilityTabsProps {
  activeKey: LocalModelCapabilityKey;
  activity: Partial<Record<LocalModelCapabilityKey, CapabilityActivity>>;
  onChange: (key: LocalModelCapabilityKey) => void;
}

// Render a horizontally scrollable role="tablist" using Icon Park icons.
// Implemented tabs call onChange. Planned tabs set aria-disabled="true",
// render the localized Planned pill, and show plannedHint in an Arco Tooltip.
// Running tabs show a primary dot; error tabs show a danger dot.
```

Use `DataServer`, `Pic`, `FileText`, `HeadsetOne`, and `VolumeNotice` when exported by Icon Park; if either speech icon is unavailable, use the closest existing Icon Park audio icon discovered from the package typings.

- [ ] **Step 4: Implement the summary and disclosure primitives**

```tsx
export interface LocalModelCapabilitySummaryItem {
  label: React.ReactNode;
  value: React.ReactNode;
  tone?: 'neutral' | 'success' | 'warning' | 'danger';
}

export const LocalModelCapabilitySummary: React.FC<{
  items: LocalModelCapabilitySummaryItem[];
}>;

export const LocalModelDetails: React.FC<{
  forcedOpen?: boolean;
  children: React.ReactNode;
}>;
```

`LocalModelDetails` stores only the manual toggle. Its visible state is `manualOpen || forcedOpen`, so an active transfer or error cannot be hidden accidentally. Render a full-width text button with `Down` rotated 180 degrees when open and an adjacent content region with a light top divider.

- [ ] **Step 5: Typecheck the shared UI**

Run: `bun run typecheck`

Expected: exit 0 with no TypeScript diagnostics.

- [ ] **Step 6: Commit the shared UI**

```bash
git add ui/src/renderer/pages/modelHub/LocalModelCapabilityTabs.tsx ui/src/renderer/pages/modelHub/LocalModelCapabilitySummary.tsx ui/src/renderer/pages/modelHub/LocalModelDetails.tsx ui/src/renderer/services/i18n/locales/en-US/settings.json ui/src/renderer/services/i18n/locales/zh-CN/settings.json ui/src/renderer/services/i18n/i18n-keys.d.ts
git commit -m "feat(local-models): add capability center primitives"
```

### Task 3: Extract the text model panel and build the tabbed shell

**Files:**
- Create: `ui/src/renderer/pages/modelHub/TextModelsPanel.tsx`
- Modify: `ui/src/renderer/pages/modelHub/LocalModelsContent.tsx`
- Modify: `ui/src/renderer/pages/modelHub/ImageModelsPanel.tsx`
- Modify: `ui/src/renderer/pages/modelHub/OcrModelsPanel.tsx`

**Interfaces:**
- Consumes: the three existing controller hooks plus Task 2 components.
- Produces: `TextModelsPanelProps`, `ImageModelsPanelProps.controller`, and `OcrModelsPanelProps.controller` using `ReturnType<typeof useLocalModels>` and corresponding hook types.

- [ ] **Step 1: Move text-model rendering into `TextModelsPanel`**

```tsx
export interface TextModelsPanelProps {
  controller: ReturnType<typeof useLocalModels>;
}

const TextModelsPanel: React.FC<TextModelsPanelProps> = ({ controller }) => {
  const { t, i18n } = useTranslation();
  const { catalog, status, catalogError, statusError, isLoading, pendingAction,
    install, cancel, remove, setActive } = controller;

  // Move the existing phase labels, action handlers, confirmation modal,
  // progress rendering, empty/error rendering, and catalog map here unchanged.
  // Replace the old privacy/runtime header cards with LocalModelCapabilitySummary.
  // Wrap source/capability chips, progress, error detail, and delete control in
  // LocalModelDetails with forcedOpen={detailsForcedOpen(state.installPhase, Boolean(state.errorKind))}.
};
```

The collapsed card keeps model name, status tags, description, essential metadata, and primary action visible. Keep delete inside the disclosure.

- [ ] **Step 2: Convert image and OCR components to controller-driven content panels**

Change each panel to accept its existing hook result as a prop. Remove each panel’s outer title, refresh button, on-demand banner, top-level border, and duplicate section divider. Keep loading/empty/error behavior, catalog cards, and model actions.

```tsx
export interface ImageModelsPanelProps {
  controller: ReturnType<typeof useLocalImageModels>;
  className?: string;
}

export interface OcrModelsPanelProps {
  controller: ReturnType<typeof useLocalOcrModels>;
  className?: string;
}
```

Use `LocalModelCapabilitySummary` for readiness and `LocalModelDetails` for source, component tags, legal notice, progress, checkpoint, error, and delete controls. Pass `forcedOpen` from `detailsForcedOpen()`.

- [ ] **Step 3: Replace `LocalModelsContent` with the shell**

```tsx
const LocalModelsContent: React.FC = () => {
  const { t } = useTranslation();
  const viewMode = useSettingsViewMode();
  const [activeCapability, setActiveCapability] = useState<LocalModelCapabilityKey>('text');
  const text = useLocalModels();
  const image = useLocalImageModels();
  const ocr = useLocalOcrModels();

  const refresh = activeCapability === 'image'
    ? image.refresh
    : activeCapability === 'ocr'
      ? ocr.refresh
      : text.refresh;

  return (
    <div className="flex min-h-0 flex-col bg-2 rd-16px px-24px py-16px">
      <header>{/* one title, subtitle, and refresh button */}</header>
      <LocalModelCapabilityTabs activeKey={activeCapability} activity={activity} onChange={setActiveCapability} />
      <NomiScrollArea className="mt-14px flex-1 min-h-0" disableOverflow={viewMode === 'page'}>
        <div hidden={activeCapability !== 'text'}><TextModelsPanel controller={text} /></div>
        <div hidden={activeCapability !== 'image'}><ImageModelsPanel controller={image} /></div>
        <div hidden={activeCapability !== 'ocr'}><OcrModelsPanel controller={ocr} /></div>
      </NomiScrollArea>
    </div>
  );
};
```

All three panels remain mounted so SWR polling, pending actions, and progress persist across tab switches. Derive `activity` with `capabilityActivity()` from each controller status and errors.

- [ ] **Step 4: Run focused and existing model view tests**

Run: `bun test ui/src/renderer/pages/modelHub/localModelCapabilityView.test.ts ui/src/renderer/pages/modelHub/localModelView.test.ts ui/src/renderer/pages/modelHub/imageModelView.test.ts ui/src/renderer/pages/modelHub/creationModels.test.ts`

Expected: all tests pass, 0 fail.

- [ ] **Step 5: Run typecheck and fix only refactor-related diagnostics**

Run: `bun run typecheck`

Expected: exit 0.

- [ ] **Step 6: Commit the tabbed shell and panel refactor**

```bash
git add ui/src/renderer/pages/modelHub/LocalModelsContent.tsx ui/src/renderer/pages/modelHub/TextModelsPanel.tsx ui/src/renderer/pages/modelHub/ImageModelsPanel.tsx ui/src/renderer/pages/modelHub/OcrModelsPanel.tsx
git commit -m "refactor(local-models): organize models by capability"
```

### Task 4: Visual QA, accessibility, and delivery verification

**Files:**
- Modify only files from Tasks 2–3 when QA reveals a scoped layout or accessibility defect.

**Interfaces:**
- Consumes: completed capability center.
- Produces: verified responsive, themed, keyboard-accessible UI.

- [ ] **Step 1: Run the complete focused test set**

Run: `bun test ui/src/renderer/pages/modelHub/*.test.ts`

Expected: all model-hub tests pass, 0 fail.

- [ ] **Step 2: Run repository UI checks**

Run: `bun run typecheck`

Expected: exit 0.

Run: `bun run check:i18n`

Expected: exit 0 with generated keys current.

Run: `bun run build:ui`

Expected: Vite production build exits 0.

- [ ] **Step 3: Verify the local page visually**

Start the existing web development flow with `bun run dev:web` or the narrowest existing project command that renders the model hub. Open the local-model page at approximately the supplied screenshot width. Verify:

- only one capability panel is visible;
- the tab bar remains readable without wrapping;
- planned tabs are disabled and explain why;
- details are collapsed for idle cards;
- active transfer/error details remain visible;
- actions wrap without overlap at narrow widths;
- light and dark themes use readable tokens;
- no duplicate page header, refresh button, or outer section border remains.

- [ ] **Step 4: Compare before and after at the same viewport**

Capture the implemented page at the same approximate viewport as `C:\Users\rika0\Pictures\Snipaste_2026-07-12_17-08-48.png`. Check spacing, borders, radii, text weights, clipping, and card density side by side. Apply only visual fixes required by the accepted design.

- [ ] **Step 5: Run final diff and worktree checks**

Run: `git diff --check && git status --short`

Expected: no whitespace errors; only intentional capability-center files are modified before the final commit.

- [ ] **Step 6: Commit QA fixes if any**

```bash
git add ui/src/renderer/pages/modelHub ui/src/renderer/services/i18n
git commit -m "fix(local-models): polish capability center layout"
```
