# Screenshot Manifest

This file tracks every screenshot referenced by the documentation. All images
are captured from the **real running NomiFun app** (not mockups) and saved into
this `docs/images/` directory.

## Naming scheme

```
<module-prefix>-<NN>-<slug>.png
```

- `module-prefix` — namespace owned by one doc area (see table below).
- `NN` — two-digit sequence within that module (`01`, `02`, …).
- `slug` — short kebab-case description.

## How to capture (current process)

The desktop app and the `nomifun-web` host render the **same** production SPA
(`ui/dist`), so in-app screens are pixel-identical between them. Capture in-app
screens from the web host (scriptable, no native window needed); capture native
window chrome from the installed app.

1. **Build the SPA** (if changed): `bun run build:ui`.
2. **Run a throwaway, no-auth host on an isolated data dir** (so it never
   touches your real data or collides with the desktop app's database):

   ```bash
   target/debug/nomifun-web --insecure-no-auth --port 8799 \
     --dist ui/dist --data-dir /tmp/nomifun-shots
   ```

3. **Seed synthetic demo data** through the local API (no real credentials),
   e.g. companions, requirements, a knowledge base, a terminal session — see
   `docs/images` git history for the exact `curl` payloads used.
4. **Drive a headless browser** (Python Playwright 1.51, already installed) at
   dark theme, `1440×900`, `device_scale_factor=2`:

   ```python
   ctx = browser.new_context(viewport={"width":1440,"height":900},
                             color_scheme="dark", device_scale_factor=2)
   ctx.add_init_script("localStorage.setItem('__nomifun_theme','dark')")
   page.goto("http://127.0.0.1:8799/#/<route>")
   ```

5. **Auth-mode screens** (login / first-run setup): run the host **without**
   `--insecure-no-auth`. No admin yet → first-run setup; pass
   `--admin-user/--admin-password` → login screen.
6. **Native window** (titlebar / tray): capture from the installed
   `/Applications/NomiFun.app` with macOS window capture. Requires Screen
   Recording permission for the capturing process.

## Module prefixes

| Prefix | Owner doc area |
| --- | --- |
| `gs-` | getting-started/ |
| `desktop-` | guides/desktop-app |
| `webserver-` | guides/web-server-deployment |
| `webui-` | guides/webui-remote-access |
| `terminal-` | guides/terminal |
| `autowork-` | guides/autowork-requirements |
| `cron-` | guides/scheduled-tasks |
| `channels-` | guides/channels |
| `mcp-` | guides/mcp-and-skills |
| `assistants-` | guides/assistants |
| `readme-` | root README showcase |

## Manifest

Routes use the `#/…` hash-router scheme. **Status** is `live` (captured from the
current app) or `pending` (needs an environment this machine can't provide —
see notes).

| Id | Route / screen | Host | Caption | Used in | Status |
| --- | --- | --- | --- | --- | --- |
| `readme-00-orchestration-hero.png` | desktop app / conversation orchestration | desktop | Header hero showing orchestration conversation, reusable roles, and DAG canvas | README / README.zh-CN | live |
| `readme-01-workbench-overview.png` | desktop app / conversation orchestration | desktop | Work orchestration with conversation, reusable roles, and DAG canvas | README / README.zh-CN | live |
| `readme-02-terminal-create.png` | `#/terminal-new` | desktop | Create terminal flow with expanded capability handoff panel | README / README.zh-CN | live |
| `readme-03-assistants.png` | `#/assistants?tab=assistants` | desktop | Assistants and Skills capability library | README / README.zh-CN | live |
| `readme-04-model-agents.png` | `#/models` | desktop | Models and Agents management with installed and supported CLI agents | README / README.zh-CN | live |
| `readme-05-companions.png` | `#/nomi` | desktop | Desktop companion overview with memory and growth state | README / README.zh-CN | live |
| `readme-06-knowledge.png` | `#/knowledge` | desktop | Knowledge base list and local domain context | README / README.zh-CN | live |
| `readme-07-requirements.png` | `#/requirements` | desktop | Requirements platform list and AutoWork entry | README / README.zh-CN | live |
| `gs-01-introduction-hero.png` | `#/guid` | web | Home / new-session page | getting-started/introduction | live |
| `gs-02-desktop-dev.png` | `#/guid` | web (interim) | Desktop app home — app content; native window chrome pending | getting-started/introduction | live\* |
| `gs-03-web-first-run-setup.png` | `#/login` (no admin) | web (auth) | First-run admin setup | getting-started/installation | live |
| `gs-04-quickstart-login.png` | `#/login` (admin exists) | web (auth) | Login screen | getting-started/quick-start | live |
| `gs-05-quickstart-guid.png` | `#/guid` | web | Home page (agent bar + input) | getting-started/quick-start | live |
| `gs-06-quickstart-model-settings.png` | `#/models` | web | Model & Agent settings | getting-started/quick-start | live |
| `desktop-01-main-window.png` | `#/guid` | web (interim) | Desktop main window content; native chrome pending | guides/desktop-app | live\* |
| `webui-01-settings-overview.png` | `#/open-capabilities` | web | Open Capabilities panel | guides/webui-remote-access | live |
| `webui-03-login-screen.png` | `#/login` | web (auth) | Login screen on a remote browser | guides/webui-remote-access | live |
| `webui-04-qr-login-phone.png` | `#/login` @ 390px | web (phone) | Login on a phone-width viewport | guides/webui-remote-access | live |
| `terminal-01-session.png` | `#/terminal/:id` | web | In-app terminal session | guides/terminal | live |
| `terminal-02-create-page.png` | `#/terminal-new` | web | Terminal create page | guides/terminal | live |
| `terminal-03-driving-session.png` | `#/terminal/:id` | web | Driving a terminal (live output) | guides/terminal | live |
| `autowork-01-tag-sessions.png` | `#/requirements/extensions?tab=autowork` | web | AutoWork tag-sessions overview | guides/autowork-requirements | live |
| `autowork-02-list.png` | `#/requirements` | web | Requirements list | guides/autowork-requirements | live |
| `autowork-03-kanban.png` | `#/requirements?view=board` | web | Requirements board (pending/in-progress/done/…) | guides/autowork-requirements | live |
| `autowork-04-tag-sessions.png` | `#/requirements/extensions?tab=autowork` | web | Tag-sessions table | guides/autowork-requirements | live |
| `autowork-05-webhook-binding.png` | `#/requirements/extensions?tab=notify` | web | Notify / webhook tab | guides/autowork-requirements | live |
| `cron-01-list.png` | `#/scheduled` | web | Scheduled Tasks list + keep-awake banner | guides/scheduled-tasks | live |
| `cron-02-create-dialog.png` | `#/scheduled` (New task) | web | Create scheduled task dialog | guides/scheduled-tasks | live |
| `cron-03-detail.png` | `#/scheduled/:job_id` | web | Job detail: schedule, Run now, history | guides/scheduled-tasks | live |
| `channels-01-overview.png` | `#/nomi?tab=remote` | web | Companion Remote tab — channel overview | guides/channels | live |
| `channels-02-pairing.png` | `#/nomi?tab=remote` (connect) | web | Channel connect / settings dialog | guides/channels | live |
| `mcp-01-capabilities.png` | `#/mcp` | web | MCP page | guides/mcp-and-skills | live |
| `mcp-03-skills.png` | `#/assistants?tab=skills` | web | Skills tab | guides/mcp-and-skills | live |
| `assistants-01-list.png` | `#/assistants?tab=assistants` | web | Assistants list (builtin library) | guides/assistants | live |
| `assistants-02-editor.png` | `#/assistants?tab=assistants` (edit) | web | Assistant editor drawer | guides/assistants | live |
| `webserver-02-first-run-setup.png` | `#/login` (no admin) | web (auth) | First-run admin setup | guides/web-server-deployment | live |

## Not screenshots (rendered as commands / diagrams)

These were intentionally **not** captured as screenshots — they read better as
copy-pasteable commands or an ASCII diagram, and several are platform-specific
(Linux/Windows) and not reproducible on a macOS dev machine:

- `web-server-deployment`: high-level architecture (ASCII diagram), `docker
  compose up` output, `systemctl status` output.
- `desktop-app`: `bun run build` output, the per-OS titlebar note, and the data
  directory layout (shown as a directory tree).

## Pending screenshots

`gs-02` and `desktop-01` are marked `live\*`: they currently show the **real
current app content** captured from the web host, but **without the native
window chrome** (titlebar / traffic-lights / tray). Replace them with a true
native capture from the installed `/Applications/NomiFun.app` (needs Screen
Recording permission, or a manual `Cmd+Shift+4` window capture) when convenient.

A few data-heavy views are intentionally **not** screenshotted — a live
conversation reply, the per-job cron skill editor, the channel model selector,
an AutoWork-bound terminal, and the desktop-only WebUI-enabled state. They need
a configured LLM provider, a live IM bot, or the desktop LAN feature; the
surrounding guide prose covers them instead.
