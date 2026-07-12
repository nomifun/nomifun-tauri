# Scheduled Task More Actions Design QA

- Reference: `codex-clipboard-db57196a-7f7f-4137-b3e8-e8cbac38d78d.png`
- Desktop result: passed — the action header and inline switch are removed; the row action trigger is hidden by default, becomes visible on row hover or keyboard focus, and stays visible while its popup is open.
- Menu behavior: passed — enabled jobs expose pause and remove, paused jobs expose resume and remove, and manual-only jobs expose remove only. Removal is guarded by the existing confirmation dialog, and action interactions remain on the scheduled-task list route.
- Mobile result: passed — the existing 390 × 844 card layout remains intact, scheduled jobs retain their switch, manual-only jobs have no switch, desktop actions are hidden, and the page has no horizontal overflow.
- Visual comparison: passed — the implementation capture was compared side by side with the user reference at the same logical desktop viewport; the requested far-right More menu replaces the action column without adding list background or outer borders.
- Remaining P0/P1/P2 issues: none.

final result: passed
