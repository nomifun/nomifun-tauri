# Workspace Tool Rail Red-Dot Indicator

## Goal

Replace the large numeric change-count badge on the desktop workspace tool rail with a small red dot, so a pending change remains noticeable without obscuring the Changes icon.

## Scope

- The desktop right-side `WorkspaceToolRail` Changes entry only.
- Keep the existing `changeCount > 0` data condition.
- Render one red dot when the count is positive; render nothing when it is zero.
- Do not alter the workspace panel behaviour, counts, mobile trigger, or other status indicators.

## Design

`WorkspaceToolRail` will still receive the numeric `changeCount`, but will use it solely as a boolean signal. The Changes item will render the existing badge element only for a positive count, without text content.

The scoped badge CSS will change from a pill-sized, primary-coloured number badge to a fixed circular red dot. It will remain absolutely positioned at the upper-right edge of the 28px icon button, with a background-colour border that preserves separation from the rail.

## Accessibility

The visible label continues to identify the button. The red dot is decorative because the button already has an accessible name and the pending-change count is not exposed today as a separate accessible label.

## Testing

- Extend the existing `workspaceToolRail.test.ts` structural coverage to require a positive-count condition without rendering count text.
- Assert the badge CSS remains circular, compact, and red.
- Run the focused Bun test and UI typecheck.

## Error Handling

No new data paths or error states are introduced. Missing or zero `changeCount` continues to hide the indicator.
