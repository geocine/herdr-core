//! Agents a lost kept terminal was running, started again in their resume mode
//! when the terminal comes back (after a reboot, logout or core crash), so the
//! window looks as it did: the agent's last conversation in the same folder.
//! Plain programs are never run again; only known agents that can resume.

use crate::foreground::Foreground;

/// The resume command for the agent in the foreground, if it is one we know.
pub fn command(f: &Foreground) -> Option<Vec<String>> {
    let words = std::iter::once(f.name.as_str()).chain(f.argv.iter().map(String::as_str));
    for word in words {
        let lower = word.to_ascii_lowercase().replace('\\', "/");
        let base = lower.rsplit('/').next().unwrap_or(&lower);
        let base = [".exe", ".cmd", ".ps1", ".js", ".mjs"].iter().fold(base, |b, ext| b.strip_suffix(ext).unwrap_or(b));
        let cmd: &[&str] = if base == "claude" || lower.contains("@anthropic-ai/claude-code") {
            // This process's own conversation, so two in one folder each get theirs back;
            // one with no message yet isn't saved, so that one starts fresh.
            match claude_session(f.pid) {
                Some((id, true)) => return Some(vec!["claude".into(), "--resume".into(), id]),
                Some((_, false)) => &["claude"],
                None => &["claude", "--continue"],
            }
        } else if base == "codex" || lower.contains("@openai/codex") {
            &["codex", "resume", "--last"]
        } else if base == "opencode" || lower.contains("opencode-ai") {
            &["opencode", "--continue"]
        } else {
            continue;
        };
        return Some(cmd.iter().map(|s| (*s).to_owned()).collect());
    }
    None
}

/// The conversation a running Claude Code is in, and whether it is saved yet:
/// Claude keeps `~/.claude/sessions/<pid>.json` current and saves a conversation,
/// from its first message, as `projects/<folder>/<id>.jsonl` (`CLAUDE_CONFIG_DIR`
/// moves `~/.claude`).
fn claude_session(pid: i32) -> Option<(String, bool)> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".claude")))?;
    let (id, cwd) = session_id(&std::fs::read(dir.join("sessions").join(format!("{pid}.json"))).ok()?, pid)?;
    let saved = dir.join("projects").join(project_folder(&cwd)).join(format!("{id}.jsonl")).is_file();
    Some((id, saved))
}

/// `sessionId` and `cwd` from a Claude Code session file, when it is this process's and the id looks like one.
fn session_id(file: &[u8], pid: i32) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_slice(file).ok()?;
    if v["pid"].as_i64() != Some(pid as i64) {
        return None;
    }
    let id = v["sessionId"].as_str()?;
    let cwd = v["cwd"].as_str().unwrap_or_default();
    (!id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .then(|| (id.to_owned(), cwd.to_owned()))
}

/// Claude's folder name for a project: every character but letters and digits as `-`.
fn project_folder(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// `shell` running `cmd`, then staying as an interactive shell when it ends;
/// None for a shell we don't know how to do that in (it starts plain).
pub fn launch(shell: &[String], cmd: &[String]) -> Option<Vec<String>> {
    let exe = shell.first()?;
    let name = exe.rsplit(['/', '\\']).next().unwrap_or(exe).to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    match name {
        "bash" | "sh" | "zsh" => {
            let mut argv = vec![exe.clone(), "-lic".into(), "\"$@\"; exec \"$0\" -li".into(), exe.clone()];
            argv.extend(cmd.iter().cloned());
            Some(argv)
        }
        "fish" => {
            let mut argv =
                vec![exe.clone(), "-lic".into(), "command $argv[2..]; exec $argv[1] -li".into(), exe.clone()];
            argv.extend(cmd.iter().cloned());
            Some(argv)
        }
        "pwsh" | "powershell" => {
            let quoted: Vec<String> = cmd.iter().map(|w| format!("'{}'", w.replace('\'', "''"))).collect();
            Some(vec![
                exe.clone(),
                "-NoLogo".into(),
                "-NoExit".into(),
                "-Command".into(),
                format!("& {}", quoted.join(" ")),
            ])
        }
        _ => None,
    }
}

/// The old program's modes, off: what it asked of the terminal is not what the
/// new shell wants. Left on, they make the terminal type at the shell: color
/// scheme reports (2031: `997;1n` at the prompt when the app sets colors),
/// mouse, focus and resize reports, kitty keys. Then the main screen, a shown
/// cursor and plain text. Kitty key flags on both screens (each keeps its own).
const OLD_MODES_OFF: &[u8] = b"\x1b[?2026l\x1b[?1049h\x1b[=0;1u\x1b[?1049l\x1b[?1047l\x1b[?47l\x1b[=0;1u\x1b[>4;0m\
\x1b[?1l\x1b[?66l\x1b[?9l\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\
\x1b[?1016l\x1b[?2004l\x1b[?2031l\x1b[?2033l\x1b[?2048l\x1b[?25h\x1b[0m";

/// What a restored screen gets before the new program prints: the old
/// program's modes off (`OLD_MODES_OFF`), then its lines scrolled up into
/// history, since a new console clears the screen it starts on. The new
/// program's output follows right under them, as if it had carried on.
pub fn keep_restored(screen: &mut ghostty_vt::Terminal, rows: u16) {
    screen.write(OLD_MODES_OFF);
    let shown = screen.screen_rows();
    let Some(last) = shown.iter().rposition(|row| !row.trim().is_empty()) else { return };
    screen.write(restored(last as u16 + 1, rows).as_slice());
}

/// The bytes after the modes: exactly the lines up to the last one with text
/// (row `last`, from 1) scrolled into history, the cursor home.
fn restored(last: u16, rows: u16) -> Vec<u8> {
    let mut out = format!("\x1b[{rows};1H").into_bytes();
    out.extend(std::iter::repeat_n(b'\n', last.min(rows) as usize));
    out.extend(b"\x1b[H");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fg(name: &str, argv: &[&str]) -> Foreground {
        Foreground { pid: 1, name: name.into(), argv: argv.iter().map(|s| (*s).to_owned()).collect(), cwd: "/".into() }
    }

    #[test]
    fn knows_agents_by_program_or_script() {
        assert_eq!(command(&fg("claude.exe", &["claude"])).unwrap(), ["claude", "--continue"]);
        assert_eq!(
            command(&fg("node.exe", &["node", r"C:\npm\node_modules\@anthropic-ai\claude-code\cli.js"])).unwrap(),
            ["claude", "--continue"]
        );
        assert_eq!(command(&fg("codex", &["codex", "fix it"])).unwrap(), ["codex", "resume", "--last"]);
        assert_eq!(
            command(&fg("node", &["node", "/usr/lib/node_modules/@openai/codex/bin/codex.js"])).unwrap(),
            ["codex", "resume", "--last"]
        );
        assert_eq!(command(&fg("opencode.exe", &[])).unwrap(), ["opencode", "--continue"]);
        assert_eq!(command(&fg("bash.exe", &["bash", "--login", "-i"])), None);
        assert_eq!(command(&fg("vim", &["vim", "claude.md"])), None);
    }

    #[test]
    fn a_restored_screen_keeps_its_lines_in_history_right_above_the_new_ones() {
        let mut screen = ghostty_vt::Terminal::new(40, 6, 1 << 20);
        screen.write(b"$ echo one\r\none\r\n$ ");
        keep_restored(&mut screen, 6);
        // The new console clears the screen it starts on.
        screen.write(b"\x1b[2J\x1b[Hnew prompt");
        let text = screen.plain_text();
        let mut lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        while lines.last() == Some(&"") {
            lines.pop();
        }
        // No gap and no mark: the new prompt carries on right under the old lines.
        assert_eq!(lines, ["$ echo one", "one", "$", "new prompt"]);
    }

    #[test]
    fn a_restored_screen_does_not_answer_for_the_old_program() {
        // Claude in its alternate screen, reporting color scheme changes, mouse and focus.
        let mut screen = ghostty_vt::Terminal::new(40, 6, 1 << 20);
        screen.write(b"$ claude\r\n\x1b[?1049h\x1b[?2031h\x1b[?1000h\x1b[?1004h\x1b[>1uClaude");
        screen.take_responses();
        keep_restored(&mut screen, 6);
        assert!(!screen.alt_screen());
        assert!([2031, 1000, 1004].iter().all(|&m| !screen.dec_mode(m)));
        // The app setting its colors no longer types `997;1n` at the new shell's prompt.
        screen.set_display(Some([1, 2, 3]), Some([255; 3]), None, None);
        assert_eq!(screen.take_responses(), b"");
    }

    #[test]
    fn claude_resumes_its_own_session() {
        let file = br#"{"pid":19204,"sessionId":"a6525362-cde8-43e2-8b43-74775173664b","cwd":"C:\\x"}"#;
        let (id, cwd) = session_id(file, 19204).unwrap();
        assert_eq!(id, "a6525362-cde8-43e2-8b43-74775173664b");
        assert_eq!(project_folder(&cwd), "C--x");
        assert_eq!(project_folder(r"C:\Aivan\PL\herdr"), "C--Aivan-PL-herdr");
        assert_eq!(session_id(file, 1), None, "another process's file");
        assert_eq!(session_id(br#"{"pid":1,"sessionId":"x; rm -rf /"}"#, 1), None);
    }

    #[test]
    fn launches_in_the_shell_and_stays() {
        let cmd = ["claude".to_owned(), "--continue".to_owned()];
        let bash = launch(&[r"C:\Program Files\Git\bin\bash.exe".into(), "--login".into(), "-i".into()], &cmd).unwrap();
        assert_eq!(&bash[1..4], ["-lic", "\"$@\"; exec \"$0\" -li", r"C:\Program Files\Git\bin\bash.exe"]);
        assert_eq!(&bash[4..], cmd);
        let pwsh = launch(&["pwsh.exe".into()], &cmd).unwrap();
        assert_eq!(pwsh.last().unwrap(), "& 'claude' '--continue'");
        assert_eq!(launch(&["nu".into()], &cmd), None);
    }
}
