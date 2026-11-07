# herdr-core (geocine fork)

A fork of [herdrdev/herdr-core](https://github.com/herdrdev/herdr-core), the
core (`hc`) that runs terminals and coding agents for Herdr. For what it is
and how to use it, read **[the original README](https://github.com/herdrdev/herdr-core/blob/master/README.md)**.

The `geocine` branch is rebased on upstream `master`. It goes with the Herdr
app fork, [geocine/herdr-app](https://github.com/geocine/herdr-app/tree/geocine).

## Install (Windows)

Install it together with the app fork, in PowerShell:

```powershell
powershell -c "irm https://raw.githubusercontent.com/geocine/herdr-app/geocine/distribution/install-geocine.ps1 | iex"
```

That installs the app and this core from the latest
[fork release](https://github.com/geocine/herdr-app/releases), replacing an
installed herdr-core in place (running terminals keep running). See
[the app fork's README](https://github.com/geocine/herdr-app/tree/geocine#install-windows)
for details.

## What's new in this fork

- **Kept terminals come back after a reboot** (or a logout or core crash),
  with their screen, their folder and the agent that was running, in its
  resume mode. A restored screen carries on right under its old lines.
- **Each tab gets its own conversation back,** even with several tabs
  running one agent in one folder:
  - Claude Code (`claude --resume <id>`) and Codex (`codex resume <id>`) are
    exact. Codex is exact on Windows and Linux.
  - pi (`pi --session-id <id>`) is exact when herdr starts it. A pi you start
    yourself is matched by its sessions.
  - OpenCode (`opencode --session <id>`) is matched by its log and title.
  - Grok (`grok --resume <id>`) is exact.
  - When a tab's conversation can't be told for sure, the agent starts fresh
    rather than open another tab's.

  `resume_agents = false` in config.toml turns this off.
- **Windows: the program under the shell** is the terminal's foreground,
  including agents started from Git Bash through npm scripts.
- **Windows: Git Bash folders** are tracked from OSC 9;9 reports.
- **Where the build sits:** `server.status` (and `hc server status`) says
  which upstream commit the build is based on and how far the fork is ahead
  of it.

To build from source, see
[the original README](https://github.com/herdrdev/herdr-core/blob/master/README.md#build-from-source).
