# Windows Shell Console Isolation Design

## Problem

On Windows, Nomi's `Bash` and non-interactive `exec_command` shell paths use
pipe transport. The direct PowerShell process is created with
`CREATE_NO_WINDOW`, but a console child such as `cmd.exe` or a batch-file
shim has no pseudoconsole to inherit. It can therefore create a visible
console window. The Windows Job Object eventually terminates descendants, but
does not prevent the window from appearing while the child is alive.

Steering a conversation does not spawn a process itself. It can cause a later
model turn to execute a shell command, which makes the symptom appear tied to
steering.

## Goal

Prevent ordinary Windows shell descendants from opening a visible console,
while preserving supervised output, cancellation, and process-tree cleanup.
Do not alter macOS or Linux transport selection.

## Non-goals

- Prevent an arbitrary third-party executable from deliberately creating GUI
  windows through its own Win32 calls.
- Support detached application launch through the shell tools. Dedicated
  computer/launch functionality owns that behavior.
- Fall back silently to the prior Windows pipe transport when ConPTY setup
  fails.

## Design

### Windows transport selection

Introduce one Windows-only transport selector for shell commands. It returns a
fixed-size `Transport::Pty` on Windows and retains the current pipe/TTY choice
on Unix.

Apply it to:

1. `BashTool::run_supervised`.
2. Legacy `exec_command` shell invocations with `tty: false`.
3. Shell script mode in `exec_command`.

The existing execution kernel creates the pseudoconsole before the root
process is resumed, assigns the process to the Job Object, and uses the
pseudoconsole process attribute. Console descendants therefore inherit the
invisible ConPTY instead of allocating a visible `conhost` window.

If ConPTY creation fails, command startup fails with the existing supervised
spawn error. It must not retry through `Transport::Pipe`, because that would
reintroduce the defect without notifying the caller.

### Shell launch policy

Add a Windows-only validation step before a shell request is normalized. It
rejects command forms whose purpose is to create a separate interactive
console or detached GUI process:

- `start` / `cmd /c start`;
- PowerShell `Start-Process` and aliases that invoke it;
- `cmd /k`;
- PowerShell process starts using `UseShellExecute` or a `WindowStyle` other
  than hidden.

The policy returns a stable tool error that tells the caller to use the
dedicated launch tool for application/URL/file opening. Normal `cmd /c` and
batch-file invocation remain valid and attach to ConPTY.

This is an explicit product boundary, not a claim that static command
validation can restrain arbitrary native code. An executable that intentionally
creates a GUI is outside the shell tool contract.

### Lifecycle and output behavior

Do not change Windows Job Object ownership, cancellation, or cleanup. The
existing Job Object remains responsible for killing the entire process tree on
completion, timeout, or cancellation.

PTY output is one terminal stream, so stdout and stderr attribution is no
longer preserved for affected Windows shell commands. Existing output renderers
already accept `OutputStream::Pty`; tests will assert text/result behavior,
not stream separation. Programs may enable color or progress formatting when
they detect a terminal. The selector will set `TERM=dumb` and `NO_COLOR=1` for
these non-interactive Windows requests where compatible with the current
environment contract.

### Cross-platform behavior

macOS and Linux retain their current transport selection and watchdog/
process-group cleanup. They do not have the Windows `cmd.exe`/`conhost`
allocation behavior. The launch policy is Windows-only; Unix programs that
explicitly launch a graphical terminal remain outside the ordinary shell
execution contract.

## Tests and verification

1. Add unit tests for transport selection on Windows and non-Windows.
2. Add policy tests covering allowed `cmd /c` and rejected explicit launch
   forms, including case and whitespace variants.
3. Add Windows-gated execution coverage that starts a console child from a
   ConPTY shell and verifies supervised completion/cancellation reaps it.
4. Run the focused `nomi-tools` and `nomi-execution` test suites on the host.
5. Run the Windows-gated suites on an interactive Windows machine to validate
   that no visible console is created; CI alone cannot reliably inspect desktop
   pixels.

## Acceptance criteria

- Windows `Bash` and non-interactive `exec_command` shell work uses ConPTY,
  not pipe transport.
- A normal `cmd /c` command completes and returns output without a visible
  console.
- Explicit detached/window-launch command forms return an explanatory tool
  error before execution.
- Timeout, cancellation, and natural shell completion still prove that the
  Windows Job Object is empty.
- macOS/Linux execution behavior is unchanged.
