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
            &["claude", "--continue"]
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

/// What a restored screen gets before the new program prints: back to the
/// main screen with default modes (the old program may have left an alternate
/// screen, mouse reporting or a hidden cursor), a marked line, then everything
/// scrolled up into history, since a new console clears the screen it starts on.
pub fn mark_restored(screen: &mut ghostty_vt::Terminal, rows: u16) {
    screen.write(b"\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1004l\x1b[?25h\x1b[0m");
    let shown = screen.screen_rows();
    let Some(last) = shown.iter().rposition(|row| !row.trim().is_empty()) else { return };
    screen.write(restored(last as u16 + 1, rows).as_slice());
}

/// The bytes after the modes: the mark under the last line with text (row
/// `last`, from 1), then exactly the lines up to the mark scrolled into history.
fn restored(last: u16, rows: u16) -> Vec<u8> {
    let mut out = format!("\x1b[{last};1H\r\n\x1b[2m-- restored after restart --\x1b[0m").into_bytes();
    out.extend(format!("\x1b[{rows};1H").bytes());
    out.extend(std::iter::repeat_n(b'\n', (last + 1).min(rows) as usize));
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
    fn a_restored_screen_keeps_its_lines_in_history_with_a_mark_after_them() {
        let mut screen = ghostty_vt::Terminal::new(40, 6, 1 << 20);
        screen.write(b"$ echo one\r\none\r\n$ ");
        mark_restored(&mut screen, 6);
        // The new console clears the screen it starts on.
        screen.write(b"\x1b[2J\x1b[Hnew prompt");
        let text = screen.plain_text();
        let lines: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, ["$ echo one", "one", "$", "-- restored after restart --", "new prompt"]);
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
