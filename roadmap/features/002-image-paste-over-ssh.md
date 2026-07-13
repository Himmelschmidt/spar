---
id: 2
title: Image paste over SSH
status: backlog
milestone: later
effort: M
priority: medium
dependencies: []
---

# 002 — Image paste over SSH

## Summary

Paste an image (screenshot) into an agent from the spar TUI, working even when spar
runs on a remote box over SSH. Deliver it as a **file path** the agent reads, not raw
bytes typed into the terminal.

Decisions: `DECISIONS.md` **W6** (chosen approach), **X7** (deferred thin-client alternative).

Depends on the workspace-terminal work (`DECISIONS.md` W1/W2/W3): the `tmux -L spar`
socket, control-mode client, and `send-keys` path. Do not start until Track A has landed.

## Problem

The clipboard lives on the user's **local** machine; spar runs **remote**. No terminal
reliably delivers pasted *image* bytes to a TUI over the wire — bracketed paste is
text-only, OSC 52 clipboard is text (image MIME isn't standard and read is usually
disabled), and the terminal graphics protocols (Kitty / iTerm2 / Sixel) are *output*,
not input. So spar must own the transport instead of relying on terminal paste.

## Prior art — herdr (verified from source, v0.7.3)

herdr ships exactly this, and its mechanism is the template: read clipboard bytes
locally → ship raw bytes over its own socket → stage a `0600` remote temp file → paste
the **file path** as bracketed-paste text into the agent PTY (Claude Code then reads the
local-to-it file). No OSC 52, no terminal graphics transfer, no SSH RemoteForward.

The catch: herdr only does this in **`herdr --remote` thin-client mode**, where the
herdr *client* runs locally and holds a byte channel to the remote server. If you `ssh`
in and run herdr server-side, it cannot reach the clipboard. spar today is the
ssh-then-run model (and the workspace initiative doubled down on tmux for persistence),
so we cannot copy herdr wholesale — the thing that makes it easy for them (a custom
client/server protocol) is the thing we chose not to build. That fork is captured as
**X7** and lives on the backlog.

## Approach — option A (this feature): local-companion bridge

Three separable pieces:

1. **Transport (local → remote).** A `spar clip` subcommand runs *locally*, reads the
   clipboard image (`arboard` cross-platform, or `wl-paste` / `xclip` / `pbpaste` /
   AppleScript), and ships raw bytes to the running remote spar over the existing SSH
   connection. Preferred: **ControlMaster-exec** (`ssh <host> spar clip-recv --session <id>`
   lands over the multiplexed connection and hands bytes to a per-session unix socket in
   `.spar/`). Alternative: a **forwarded socket** (`RemoteForward`). Cap payload (herdr
   uses 16 MiB); validate image magic bytes.
2. **Delivery (remote → agent).** spar writes the bytes to a `0600`
   `.spar/runs/<id>/pasted/<uuid>.<ext>` and hands the agent the **path** — either into
   the Composer for spar-native dispatch, or via Track A `send-keys` into the focused
   agent pane. Agents accept image file paths.
3. **Trigger (pull, not push).** A paste keybinding in the TUI asks the connected local
   companion to capture-and-ship *now* — feels native even though bytes crossed SSH.
   Clipboard reads only ever on explicit user action, never polled.

**Graceful degradation:** when spar runs *locally*, skip the transport — read the
clipboard directly with `arboard`, stage the file, done. Same delivery code; only the
source differs.

## Staging

- **MVP (80%, no daemon):** `spar clip` on the laptop pipes to `ssh box spar clip-recv`;
  spar drops the path into the Composer. One documented command, no keybinding magic.
- **Full:** connected companion + in-TUI paste keybinding + auto-inject into the focused
  pane; a `spar clip --setup` that prints the SSH `ControlMaster` / `RemoteForward` block.

## Constraints

- Needs a **local component** — no zero-install path over plain SSH (the clipboard is
  local and no terminal ships image bytes). Being one binary helps: it is the same `spar`,
  run as `spar clip` locally.
- Requires local OS clipboard tools (`wl-paste`/`xclip` on Linux, `pbpaste`/`osascript`
  on macOS) or `arboard`.
- Security: read the clipboard only on explicit user action.

## Acceptance (when built)

- With spar remote over SSH, invoking the paste path materializes the clipboard image as
  a `0600` remote temp file and the agent receives its path.
- Works via the documented `spar clip` MVP command with no daemon.
- When spar is local, the same paste path works with a direct `arboard` read (no SSH).
- No personal-integration or terminal-specific assumptions baked in; degrades cleanly
  when clipboard tools or the companion are absent (clear, actionable error).
