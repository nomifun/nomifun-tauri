# Computer Use And Browser Use

NomiFun exposes two optional automation capability families to agents:

- **Computer use**: screenshots, mouse/keyboard input, window enumeration, and
  focus control through the in-process Rust implementation (`nomi-computer`,
  with accessibility helpers in `nomi-a11y`).
- **Browser use**: Chrome automation through the in-process Rust CDP engine
  (`nomi-browser-engine`) and the tool facade (`nomi-browser`).

Both are high-privilege capabilities. In the desktop product UI they are
compiled in and enabled by default so a user can opt out from Settings. In
headless/server hosts they are omitted or disabled unless the host explicitly
enables the relevant build feature and runtime flag.

## Current Architecture

The old external `@playwright/mcp` sidecar path and its boot-time Node/npm/
Chromium provisioning have been removed. Browser use now runs through the
native CDP engine. ACP/Codex-style sessions can reach the same engine through
the `mcp-browser-stdio` bridge.

Computer use is desktop-oriented. It can observe the screen and synthesize
input, so it is compiled into desktop/Nomi CLI builds but omitted from the
headless web/server build.

## Enabling And Disabling Capabilities

### Desktop Settings

The desktop app exposes both toggles under System Settings:

- **Browser Use** (`/settings/browser-use`)
- **Computer Use** (`/settings/computer-use`)

Current desktop builds default both toggles to **on** when the corresponding
feature is compiled. Turning either toggle off persists a user preference and
prevents new sessions from receiving that capability.

### Per Session

Create or update a session with capability flags in `extra`:

```json
{ "computerUse": true, "browserUse": true }
```

Both camelCase and snake_case keys are accepted by compatibility paths.

### Host Environment

```bash
NOMIFUN_COMPUTER_USE=1
NOMIFUN_BROWSER_USE=1
```

These set default availability for Nomi-engine sessions in the host where they
are read. They do not bypass build-time feature gates.

### Nomi Engine Config

`~/.nomi/config.toml` or project `.nomi/config.toml`:

```toml
[tools]
max_recent_images = 3

[tools.computer]
enabled = true
max_screenshot_edge = 1568

[tools.browser]
enabled = true
headless = false
allowed_origins = []
```

`browser_path` and `idle_timeout_secs` are legacy compatibility fields; the
native engine manages browser acquisition and lifecycle itself. On first use,
the engine can acquire Chrome for Testing into its own user-data area without
requiring Node, npm, or Playwright.

## Build Matrix

| Host | Computer use | Browser use |
| --- | --- | --- |
| `nomifun-desktop` | Compiled by the `computer-use` feature | Compiled by the `browser-use` feature |
| `nomi` CLI | Enabled in the current `nomi-cli` build | Not enabled in the current `nomi-cli` manifest |
| `nomifun-web` / Docker | Not compiled | Not compiled in the current headless web host |

Web/server builds should not promise desktop or managed-browser control. If a
config enables these tools in a host that was built without the relevant
features, the backend should warn rather than expose a non-working tool.

## macOS Permissions

Computer use needs OS permissions the first time it is used:

- **Accessibility**: required for mouse/keyboard input and accessibility tree
  operations.
- **Screen Recording**: required for screenshots. A black screenshot usually
  means this permission is missing.

These run **in-process inside the desktop app**, so the permission must be
granted to **NomiFun itself** (the entry named "NomiFun" in System Settings),
not to the terminal/editor — and a freshly-granted permission only takes effect
after the app is **completely quit and reopened** (macOS does not hot-load TCC
grants into a running process). Permission-failure messages name "NomiFun"
explicitly so the guidance is unambiguous.

Settings → Computer Use surfaces a live status panel (macOS): it shows whether
Accessibility / Screen Recording are *in effect for the running process* —
which is authoritative, since a System Settings toggle bound to a stale
code-signing identity reads "Not in effect" even while it looks on — with
buttons that deep-link to the exact Privacy pane and trigger the OS prompt.
Backed by `GET/POST /api/computer/permissions[/request|/open-settings]`
(`nomi_computer::permissions` → `AXIsProcessTrusted` /
`CG*ScreenCaptureAccess`).

> **Stale grant.** If a toggle is clearly on yet computer use still fails, the
> grant is bound to an older build's identity. Quit NomiFun, run
> `tccutil reset Accessibility com.nomifun.desktop` and
> `tccutil reset ScreenCapture com.nomifun.desktop`, relaunch, re-grant, and
> fully restart once more.

## Approval Semantics

- Read-only computer actions such as `screenshot`, `cursor_position`,
  `list_windows`, and `wait` are treated as info-level operations.
- Mutating computer actions such as click, type, scroll, drag, and
  `focus_window` are execution-level operations and require approval in default
  modes.
- Plan mode hides the whole computer-use tool.
- Browser actions derive approval from behavior: observation is info-level;
  navigation, clicking, typing, and other page mutations are execution-level.

Recommended loop: observe with a screenshot or browser snapshot, perform one
small operation, then observe again.

## Image And Token Hygiene

- Screenshots are downsampled to a maximum long edge of
  `max_screenshot_edge` pixels, with coordinates mapped back to real screen
  coordinates.
- The conversation keeps only the most recent `max_recent_images` individual
  tool-result images, with a provider-compatible ceiling of 20 per request.
  Excess attachments are stripped while their text and an omission note remain.
- OpenAI-compatible tool messages cannot carry images directly; image data is
  sent as a following user message with a source call id. Anthropic, Bedrock,
  and Vertex use native image blocks where supported.
- External MCP image results pass through the same image pipeline with a
  per-image size cap.

## Related Docs

- [Agent Engine](../architecture/agent-engine.md)
- [MCP And Skills](mcp-and-skills.md)
- [Remote Capability API](remote-capability-api.md)
