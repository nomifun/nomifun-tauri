# Scheduled Task More Actions Design QA

- Reference: `codex-clipboard-db57196a-7f7f-4137-b3e8-e8cbac38d78d.png`
- Desktop result: passed — the action header and inline switch are removed; the row action trigger is hidden by default, becomes visible on row hover or keyboard focus, and stays visible while its popup is open.
- Menu behavior: passed — enabled jobs expose pause and remove, paused jobs expose resume and remove, and manual-only jobs expose remove only. Removal is guarded by the existing confirmation dialog, and action interactions remain on the scheduled-task list route.
- Mobile result: passed — the existing 390 × 844 card layout remains intact, scheduled jobs retain their switch, manual-only jobs have no switch, desktop actions are hidden, and the page has no horizontal overflow.
- Visual comparison: passed — the implementation capture was compared side by side with the user reference at the same logical desktop viewport; the requested far-right More menu replaces the action column without adding list background or outer borders.
- Remaining P0/P1/P2 issues: none.

final result: passed

---

# Local Model Capability Center — Design QA

## Source of truth

- Product brief: `docs/superpowers/specs/2026-07-12-local-model-capability-center-design.md`
- Selected direction: top-level capability tabs (方案 A)
- Baseline screenshot: `C:/Users/rika0/Pictures/Snipaste_2026-07-12_17-08-48.png`
- Implementation capture: `.superpowers/qa/local-model-capability-center-default.png`

## Verification coverage

- Desktop light theme: passed
- Desktop dark theme: passed
- 900 px viewport: passed; summaries stack and capability tabs remain horizontally navigable
- Capability switching: passed for Text Understanding, Image Generation, and OCR
- Planned capabilities: Speech Recognition and Speech Synthesis are disabled and labeled as planned
- Disclosure behavior: model details are collapsed by default and expand/collapse correctly
- State retention: all capability panels remain mounted while inactive
- Accessibility basics: semantic tablist/tab/tabpanel structure, disabled states, accessible labels, and keyboard-capable disclosure buttons are present
- Production checks: unit tests, typecheck, i18n contract, theme contract, and UI production build passed

## Findings

No open P0, P1, or P2 findings. The implementation preserves the selected direction's hierarchy: one page header, capability tabs, compact summaries, scannable model cards, and progressive disclosure for secondary metadata.

The browser console showed unrelated local QA-backend failures for `/api/agents`, `/api/cron/jobs`, and WebSocket bootstrap. No errors originated from the local-model capability center.

final result: passed
