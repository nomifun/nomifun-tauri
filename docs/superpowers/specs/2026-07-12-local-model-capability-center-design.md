# Local Model Capability Center Design

## Context

The local-model page currently renders text models, image generation, and OCR as consecutive full-size sections. Each section repeats its own heading, readiness cards, notices, model cards, status messages, and borders. Image generation also keeps every component row visible. The result is a long page with weak category boundaries, excessive nested containers, and no clear explanation of which local AI capabilities the product supports.

This redesign turns the page into a local AI capability center organized by model purpose. It preserves the existing download, pause, resume, verification, activation, deletion, and runtime behavior while replacing the presentation structure.

## Goals

- Make supported local model types visible at a glance.
- Separate text understanding, image generation, and OCR into clear capability views.
- Communicate planned speech recognition and speech synthesis without presenting dead-end pages.
- Reduce vertical length, nested borders, repeated notices, and always-visible technical details.
- Provide a consistent model-management experience across all implemented capability types.
- Preserve downloads and status updates when the user switches tabs.

## Non-goals

- Implement speech recognition or speech synthesis.
- Change backend routes, download orchestration, model catalogs, runtime behavior, or storage formats.
- Combine the existing text, image, and OCR hooks into one backend-facing state manager.
- Redesign unrelated model-hub pages.

## Information Architecture

The page uses a single header followed by a horizontal capability tab bar.

The tabs appear in this order:

1. Text Understanding
2. Image Generation
3. OCR
4. Speech Recognition — disabled, labeled “Planned”
5. Speech Synthesis — disabled, labeled “Planned”

The first three tabs are interactive. The planned tabs remain visible to communicate the product direction, but cannot be selected. Hover and focus affordances explain that support is planned.

The page header contains:

- the local-model title;
- a short local-processing and on-demand-download explanation;
- one refresh action for the active capability.

The active tab contains:

1. a compact capability summary;
2. the model list;
3. loading, empty, unsupported, or error feedback when applicable.

The page no longer renders image generation and OCR below the text model list.

## Layout and Visual Hierarchy

The page uses one primary surface rather than nested section cards. The header and capability tabs remain at the top of the content area while the model list scrolls.

The capability tab bar uses the product’s existing primary color. The active tab has a light primary background and emphasized text. Inactive tabs remain neutral. Planned tabs use reduced emphasis plus a small “Planned” label.

The active capability summary is one compact row containing the information that users need before choosing a model:

- available model count;
- installed or active model state;
- runtime readiness;
- transfer or error state when present.

Visual hierarchy follows these rules:

- one main border level around model cards;
- subtle fill changes and light shadows for depth;
- primary color reserved for the active tab, primary action, active state, and progress;
- semantic colors reserved for success, warning, and failure states;
- consistent spacing based on the existing 8, 12, and 16 pixel rhythm;
- no repeated full-width informational banners when the same information can be placed in the header, summary, tooltip, or details disclosure.

## Model Card Structure

All implemented capability tabs use a shared card anatomy.

The collapsed card shows:

- model name;
- recommended, installed, active, paused, or failed status;
- one short description;
- essential metadata such as format, download size, required memory, and license;
- concise capability chips;
- the primary action on the right;
- a “Model details” disclosure row.

The details disclosure contains secondary information:

- source and attribution;
- license details and third-party notices;
- runtime or component requirements;
- file composition;
- destructive actions such as removing installed files.

The disclosure is collapsed by default. It opens when the user requests it, when a transfer begins, or when a failure requires detail. After successful installation it returns to the compact state unless the user explicitly opened it.

## Download and Progress Presentation

Text and OCR downloads display one progress row with transferred bytes, total bytes, percentage, and rate when available.

Image generation displays one bundle-level progress bar. Its component list uses compact step rows for the runtime, diffusion model, text encoder, and VAE. Only the actively transferring component shows a sub-progress bar. Completed components show a success state; pending components remain visually quiet.

Paused, verifying, extracting, failed, and unsupported states are rendered inside the relevant card. The page does not add another full-width message for the same state.

Switching tabs does not stop active transfers. A non-active tab may show a small running or error indicator so the user can locate background work.

## Component Architecture

`LocalModelsContent` becomes the capability-center shell. It owns:

- the active capability key;
- the shared header;
- the capability tab configuration;
- the active panel selection;
- the active-panel refresh action.

The existing text, image, and OCR hooks remain independent and retain their backend contracts. Their panels become content views rather than complete nested sections.

Shared presentation components should cover:

- capability tabs and planned-tab labels;
- capability summary;
- model-card shell;
- status badges;
- details disclosure;
- transfer progress;
- loading, empty, unsupported, and failure states.

The shared components receive display data and callbacks. Capability-specific state transitions remain in the existing hooks and view helpers.

## Data and Interaction Flow

1. The page initializes with Text Understanding selected.
2. Each implemented capability hook may load independently so its transfer state remains current.
3. The active panel renders its catalog, status, and pending action.
4. Refresh invokes only the active capability’s refresh callback.
5. Download, pause, resume, retry, activate, deactivate, and delete actions remain scoped to the active model and existing hook.
6. Switching tabs changes only the visible panel. It does not recreate or cancel an in-progress transfer.
7. Tab indicators summarize hidden running or failed work without duplicating detailed progress.

## States and Error Handling

- **Initial loading:** show a compact skeleton or centered loading state inside the active content area.
- **Empty catalog:** explain that no models are available for the selected capability.
- **Unsupported platform:** disable unavailable actions and explain the platform limitation near the capability summary.
- **Runtime unavailable:** show the runtime state in the summary and the actionable explanation in the affected model card.
- **Transfer failure:** expand the affected card, show the reason, and offer retry when supported.
- **Paused transfer:** keep saved progress visible and expose resume as the primary action.
- **Busy runtime:** retain current operation status and disable conflicting destructive or install actions.
- **Planned capability:** keep the tab disabled and provide a tooltip; do not render an empty detail page.

## Responsive and Accessibility Requirements

- The tab bar may scroll horizontally when the content area is narrow; labels must not wrap into multiple lines.
- Model-card actions wrap below metadata when horizontal space is insufficient.
- Tab selection, disclosure controls, and actions must be keyboard accessible.
- Disabled planned tabs expose a readable explanation rather than relying on color alone.
- Progress and status text remain present alongside color indicators.
- Existing light and dark theme tokens are used; no fixed light-only surface colors are introduced.

## Testing Strategy

Add or update tests for:

- capability-tab ordering and default selection;
- disabled planned tabs and their labels;
- active-panel selection;
- per-tab running and failure indicators;
- details collapsed by default;
- details auto-expanded during transfer and failure;
- progress presentation for single-file and bundle downloads;
- action disabling for busy, unsupported, and pending states;
- tab switching without losing hook-owned transfer state.

Run the existing model view tests, frontend test suite, TypeScript typecheck, and production build. Perform visual verification at the same approximate content width as the supplied screenshot in both light and dark themes.

## Acceptance Criteria

- Only one capability panel is visible at a time.
- Text Understanding, Image Generation, and OCR can be selected and retain existing operations.
- Speech Recognition and Speech Synthesis are visible, disabled, and marked as planned.
- Image and OCR no longer render independent full-page headers and outer cards below text models.
- Secondary legal, source, and component details are collapsed by default.
- Downloading and failed cards reveal the information needed to understand and control the operation.
- Switching tabs does not interrupt transfers.
- The final page is materially shorter and uses no more than one primary model-card border layer.
- Existing backend behavior and protocol types remain unchanged.
