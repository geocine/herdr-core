<p align="center">
  <img src="https://raw.githubusercontent.com/geocine/herdr-app/geocine/assets/icons/herdr-ui-icon-clean.png" alt="Herdr geocine fork logo" width="160">
</p>

<h1 align="center">herdr-core — geocine fork</h1>

<p align="center"><b>The Herdr core (<code>hc</code>), made for Windows.</b><br>
It runs your terminals and agents. They keep running, even after a reboot.</p>

---

## What is this?

- A fork of [herdr-core](https://github.com/herdrdev/herdr-core).
- Made and tested for **Windows** and **Git Bash**.
- Use it with the app fork: [geocine/herdr-app](https://github.com/geocine/herdr-app/tree/geocine).
- New to herdr-core? Read the [original README](https://github.com/herdrdev/herdr-core/blob/master/README.md).

## Install

Install it **with the app**. Open **PowerShell**. Paste this:

```powershell
powershell -c "irm https://raw.githubusercontent.com/geocine/herdr-app/geocine/distribution/install-geocine.ps1 | iex"
```

- It installs the app **and** this core.
- Your running terminals keep running.
- More details: [the app fork's README](https://github.com/geocine/herdr-app/tree/geocine#install).

## What's new in this fork

### Reboot-proof
- Kept terminals come back after a reboot, a logout or a crash.
- Same screen, same folder, and the agent that was running.
- The old screen stays, and the new one continues right below it.

### Each tab gets its own conversation back
Even with many tabs of the same agent in one folder.

| Agent | Comes back with | How sure |
|---|---|---|
| Claude Code | `claude --resume <id>` | Exact |
| Codex | `codex resume <id>` | Exact (Windows, Linux) |
| Grok | `grok --resume <id>` | Exact |
| pi | `pi --session-id <id>` | Exact when Herdr starts it. Started by you: matched by its sessions. |
| OpenCode | `opencode --session <id>` | Matched by its log and title |

- Not sure which conversation is a tab's? The agent starts fresh.
  It never opens another tab's conversation.
- Turn it off: `resume_agents = false` in `config.toml`.

### Windows + Git Bash
- The program under the shell counts as the terminal's program.
- Agents started from Git Bash (`pi`, `codex`, `opencode`) are seen.
  This also works for terminals opened before a core update.
- Git Bash folders are followed (OSC 9;9).

### Build info
- `hc server status` shows which upstream version this build is based on,
  and how far the fork is ahead.

## Build from source

See the [original README](https://github.com/herdrdev/herdr-core/blob/master/README.md#build-from-source).
