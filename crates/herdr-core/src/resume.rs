//! Agents a lost kept terminal was running, started again in their resume mode
//! when the terminal comes back (after a reboot, logout or core crash), so the
//! window looks as it did: the agent's last conversation in the same folder.
//! Plain programs are never run again; only known agents that can resume.

use crate::foreground::Foreground;
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

mod codex;
mod grok;
mod opencode;
mod pi;
mod sqlite;

pub use pi::{prepare, tracked};

/// What the terminal knows about the agent in front, beside the process itself.
#[derive(Clone, Copy, Default)]
pub struct Context<'a> {
    /// Its title (OpenCode's says which conversation it shows).
    pub title: &'a str,
    /// The command the terminal was started with, and when (ms): an agent
    /// started right then is that command, options and all, even where the OS
    /// no longer shows them (pi renames its process over them).
    pub argv: &'a [String],
    pub started_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Agent {
    Claude,
    Codex,
    OpenCode,
    Pi,
    Grok,
}

/// The resume command for the agent in the foreground, if it is one we know.
pub fn command(f: &Foreground, t: &Context) -> Option<Vec<String>> {
    let cmd: &[&str] = match agent(f)? {
        // This process's own conversation, so two in one folder each get theirs back;
        // one with no message yet isn't saved, so that one starts fresh.
        Agent::Claude => match (claude_session(f.pid), others_here("claude", f)) {
            (Some((id, true)), _) => return Some(vec!["claude".into(), "--resume".into(), id]),
            (Some((_, false)), _) => &["claude"],
            // The folder's last may be another claude's here.
            (None, true) => &["claude"],
            (None, false) => &["claude", "--continue"],
        },
        Agent::Codex => return codex::command(f),
        Agent::OpenCode => return opencode::command(f, t.title),
        Agent::Pi => return pi::command(f, t),
        Agent::Grok => return grok::command(f),
    };
    Some(words(cmd))
}

/// Whether `f` is an agent's interactive app, from its command line alone
/// (not a one-shot `claude -p`, `codex exec`, `pi -p`, `opencode run`, ...).
pub fn interactive_agent(f: &Foreground) -> bool {
    match agent(f) {
        Some(Agent::Claude) => !f.argv.iter().any(|w| matches!(w.as_str(), "-p" | "--print")),
        Some(Agent::Codex) => codex::args(&f.argv).is_some(),
        Some(Agent::OpenCode) => opencode::args(&f.argv).is_some(),
        Some(Agent::Pi) => pi::args(&f.argv).is_some(),
        Some(Agent::Grok) => grok::is_app(&f.argv),
        None => false,
    }
}

/// Which agent `f` is: its program, or the script node runs.
fn agent(f: &Foreground) -> Option<Agent> {
    // Only the program counts (its name, or the script node runs), never a file
    // it was handed (`vim pi`, `less codex`) or the command a shell was started
    // with (the shell an agent came back in stays after the agent quits).
    // Their npm packages' paths are told apart anywhere.
    let script = f
        .argv
        .first()
        .filter(|w| matches!(base(w).1.as_str(), "node" | "bun" | "deno"))
        .and_then(|_| f.argv.iter().skip(1).position(|w| !w.starts_with('-')).map(|i| i + 1));
    let words = std::iter::once(f.name.as_str()).chain(f.argv.iter().map(String::as_str));
    for (i, word) in words.enumerate() {
        let program = i <= 1 || script == Some(i - 1);
        let (lower, base) = base(word);
        return Some(if (program && base == "claude") || lower.contains("@anthropic-ai/claude-code") {
            Agent::Claude
        } else if (program && base == "codex") || lower.contains("@openai/codex") {
            Agent::Codex
        } else if program && is_opencode(word) {
            Agent::OpenCode
        } else if program && is_pi(word) {
            Agent::Pi
        } else if program && (is_grok(word) || (base == "agent" && grok::is_own_program(f.pid))) {
            Agent::Grok
        } else {
            continue;
        });
    }
    None
}

/// A word of a command line in lower case with `/` for `\`, and its file name
/// without the extension.
fn base(word: &str) -> (String, String) {
    let lower = word.to_ascii_lowercase().replace('\\', "/");
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    let base = [".exe", ".cmd", ".ps1", ".js", ".mjs"].iter().fold(base, |b, ext| b.strip_suffix(ext).unwrap_or(b));
    let base = base.to_owned();
    (lower, base)
}

fn is_opencode(word: &str) -> bool {
    let (lower, base) = base(word);
    base == "opencode" || lower.contains("/opencode-ai/")
}

fn is_pi(word: &str) -> bool {
    let (lower, base) = base(word);
    base == "pi" || lower.contains("/pi-coding-agent/")
}

/// `agent` is another name grok installs itself under: by its path here, or
/// by where the program really is (`grok::is_own_program`).
fn is_grok(word: &str) -> bool {
    let (lower, base) = base(word);
    base == "grok" || (base == "agent" && lower.contains("/.grok/bin/"))
}

/// Whether a word is a UUID (`8-4-4-4-12` hex digits), as agents' ids are.
fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// A folder as one key: `/` for `\`, no slash at the end, in one case where
/// folders' case doesn't matter.
fn folder_key(p: &str) -> String {
    let p = p.replace('\\', "/");
    let p = p.trim_end_matches('/');
    if crate::platform::PATHS_IGNORE_CASE { p.to_ascii_lowercase() } else { p.to_owned() }
}

/// Whether another `kind` (an agent) than `f` runs in `f`'s folder now, of
/// those seen here lately; `f` is noted as running. Where an agent's own
/// record can't say which conversation is whose, the folder's last
/// (`--continue`) is for one alone there: two would both get the same one.
fn others_here(kind: &'static str, f: &Foreground) -> bool {
    static LIVE: LazyLock<Mutex<HashMap<&'static str, Known<()>>>> = LazyLock::new(Default::default);
    let Some(start) = crate::platform::process_started_ms(f.pid) else { return false };
    let now = now_ms();
    let folder = folder_key(&f.cwd);
    let mut live = LIVE.lock().unwrap_or_else(|p| p.into_inner());
    let known = live.entry(kind).or_default();
    known.note(f.pid, start, &folder, now);
    known.folder(&folder).iter().any(|i| i.pid != f.pid && now.saturating_sub(i.seen) < SEEN_MS)
}

/// The commands that bring back terminals lost together (terminal, folder,
/// command), made so no two open the same conversation: of those that would,
/// the first keeps it and the others start their agent fresh. Two that would
/// open "the folder's last" (`--continue`, `resume --last`) in one folder
/// would share it too.
pub fn distinct(cmds: &mut [(String, Option<Vec<String>>)]) {
    let mut seen: HashSet<(String, Vec<String>)> = HashSet::new();
    for (cwd, cmd) in cmds.iter_mut() {
        let Some(c) = cmd else { continue };
        let last = c.iter().any(|w| matches!(w.as_str(), "--continue" | "--last"));
        let names = c.windows(2).any(|w| {
            matches!(w[0].as_str(), "--resume" | "--session-id" | "--session" | "resume") && !w[1].starts_with('-')
        });
        let key = if last { (folder_key(cwd), c.clone()) } else { (String::new(), c.clone()) };
        if (last || names) && !seen.insert(key) {
            *cmd = Some(fresh(c));
        }
    }
}

/// A resume command's agent started fresh, with the options it is watched
/// and kept by (pi's extension and folder, OpenCode's interface).
fn fresh(cmd: &[String]) -> Vec<String> {
    let mut out = vec![cmd[0].clone()];
    let mut it = cmd[1..].iter();
    while let Some(w) = it.next() {
        match w.as_str() {
            "-e" | "--session-dir" => out.extend([Some(w), it.next()].into_iter().flatten().cloned()),
            "--mini" => out.push(w.clone()),
            _ => {}
        }
    }
    out
}

/// A conversation made this long before its agent's start counts as made
/// after it (clocks and start times are not exact).
const SLACK_MS: u64 = 2_000;
/// An instance not seen in this long has ended.
const SEEN_MS: u64 = 10_000;
/// How long after it was last seen an instance may still have written.
const WROTE_MS: u64 = 2_000;
/// Instances known per folder at most.
const PER_FOLDER: usize = 32;

/// One running agent process seen lately.
#[derive(Debug, Clone)]
struct Instance<T> {
    pid: i32,
    start: u64,
    /// Where its conversations are kept (one key per project).
    folder: String,
    seen: u64,
    data: T,
}

impl<T> Instance<T> {
    /// Whether it was running at `at` (ms).
    fn live_at(&self, at: u64) -> bool {
        self.start <= at && at <= self.seen + WROTE_MS
    }
}

/// The instances of one agent seen lately. One that ended stays known while
/// another runs in its folder: the bound between their conversations.
struct Known<T>(Vec<Instance<T>>);

impl<T> Default for Known<T> {
    fn default() -> Self {
        Known(Vec::new())
    }
}

impl<T: Default + Clone> Known<T> {
    const fn new() -> Self {
        Known(Vec::new())
    }

    /// Notes `pid` (started at `start`) as running in `folder` at `now`.
    fn note(&mut self, pid: i32, start: u64, folder: &str, now: u64) -> &mut Instance<T> {
        // A pid used again, by another process, is another instance.
        self.0.retain(|i| i.pid != pid || (i.start == start && i.folder == folder));
        let live: HashSet<String> = self
            .0
            .iter()
            .filter(|i| now.saturating_sub(i.seen) < SEEN_MS)
            .map(|i| i.folder.clone())
            .chain([folder.to_owned()])
            .collect();
        self.0.retain(|i| live.contains(&i.folder));
        let at = match self.0.iter().position(|i| i.pid == pid) {
            Some(at) => at,
            None => {
                // The longest gone go first.
                while self.0.iter().filter(|i| i.folder == folder).count() >= PER_FOLDER {
                    let gone =
                        self.0.iter().enumerate().filter(|(_, i)| i.folder == folder).min_by_key(|(_, i)| i.seen);
                    let Some((gone, _)) = gone else { break };
                    self.0.remove(gone);
                }
                self.0.push(Instance { pid, start, folder: folder.to_owned(), seen: now, data: T::default() });
                self.0.len() - 1
            }
        };
        self.0[at].seen = now;
        &mut self.0[at]
    }

    /// The instances known in `folder`, as they are now.
    fn folder(&self, folder: &str) -> Vec<Instance<T>> {
        self.0.iter().filter(|i| i.folder == folder).cloned().collect()
    }
}

fn words(w: &[&str]) -> Vec<String> {
    w.iter().map(|s| (*s).to_owned()).collect()
}

fn home() -> Option<std::path::PathBuf> {
    std::env::home_dir()
}

fn now_ms() -> u64 {
    crate::runtime::now_ms()
}

/// A UTC date and time as ms since 1970, if it is one.
fn utc_ms(y: u64, mo: u64, d: u64, h: u64, mi: u64, s: u64) -> Option<u64> {
    if !(1970..10_000).contains(&y) || !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60
    {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm), years from March.
    let (y, mo) = if mo <= 2 { (y - 1, mo + 9) } else { (y, mo - 3) };
    let (era, yoe) = (y / 400, y % 400);
    let doy = (153 * mo + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + s * 1000)
}

/// The conversation a running Claude Code is in, and whether it is saved yet:
/// Claude keeps `~/.claude/sessions/<pid>.json` current and saves a conversation,
/// from its first message, as `projects/<folder>/<id>.jsonl` (`CLAUDE_CONFIG_DIR`
/// moves `~/.claude`).
fn claude_session(pid: i32) -> Option<(String, bool)> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".claude")))?;
    let file = std::fs::read(dir.join("sessions").join(format!("{pid}.json"))).ok()?;
    let (id, cwd) = session_id(&file, pid, crate::platform::process_started_ms(pid))?;
    let saved = dir.join("projects").join(project_folder(&cwd)).join(format!("{id}.jsonl")).is_file();
    Some((id, saved))
}

/// `sessionId` and `cwd` from a Claude Code session file, when it is this
/// process's (`pid`, started at `start`: one an older process of that pid left
/// started before it) and the id looks like one.
fn session_id(file: &[u8], pid: i32, start: Option<u64>) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_slice(file).ok()?;
    if v["pid"].as_i64() != Some(pid as i64) {
        return None;
    }
    if let (Some(at), Some(start)) = (v["startedAt"].as_u64(), start)
        && at + SLACK_MS < start
    {
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
        // No such process.
        Foreground { pid: -1, name: name.into(), argv: argv.iter().map(|s| (*s).to_owned()).collect(), cwd: "/".into() }
    }

    // Which agent, from the command line alone: what each then reads is its own
    // module's to test, with scratch files, never this machine's agents.
    #[test]
    fn knows_agents_by_program_or_script() {
        use Agent::*;
        assert_eq!(agent(&fg("claude.exe", &["claude"])), Some(Claude));
        assert_eq!(
            agent(&fg("node.exe", &["node", r"C:\npm\node_modules\@anthropic-ai\claude-code\cli.js"])),
            Some(Claude)
        );
        assert_eq!(
            command(&fg("codex", &["codex", "fix it"]), &Context::default()).unwrap(),
            ["codex", "resume", "--last"],
            "nothing readable, alone: the folder's last, as before"
        );
        assert_eq!(agent(&fg("node", &["node", "/usr/lib/node_modules/@openai/codex/bin/codex.js"])), Some(Codex));
        let pi = r"C:\Users\x\nodejs/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js";
        assert_eq!(agent(&fg("node.exe", &[r"C:\Users\x\nodejs\node.exe", pi])), Some(Pi));
        assert_eq!(agent(&fg("pi", &["pi"])), Some(Pi));
        assert_eq!(agent(&fg("grok.exe", &[r"C:\Users\x\.grok\bin\grok.exe"])), Some(Grok));
        assert_eq!(agent(&fg("agent.exe", &[r"C:\Users\x\.grok\bin\agent.exe"])), Some(Grok));
        assert_eq!(agent(&fg("opencode.exe", &[])), Some(OpenCode));
        assert_eq!(agent(&fg("bash.exe", &["bash", "--login", "-i"])), None);
        assert_eq!(agent(&fg("vim", &["vim", "claude.md"])), None);
        // Short names count only as the program; a bare `agent` only where grok put it.
        let back = ["bash", "-lic", "\"$@\"; exec \"$0\" -li", "bash", "codex", "resume", "--last"];
        for argv in [
            &["vim", "pi"][..],
            &["vim", "grok"],
            &["less", "/x/pi-coding-agent/README.md"],
            &["agent"],
            &["less", "codex"],
            &["git", "log", "claude"],
            // The shell an agent came back in, after the agent quit.
            &back,
        ] {
            assert_eq!(agent(&fg(argv[0], argv)), None, "{argv:?}");
        }
        for name in ["pip", "pipenv", "pi.txt", "grokker"] {
            assert_eq!(agent(&fg(name, &[name])), None, "{name}");
        }
        // One-shot runs are not the app.
        assert!(interactive_agent(&fg("node.exe", &["node", pi])));
        assert!(interactive_agent(&fg("codex.exe", &["codex", "-p", "work"])), "codex's -p is a profile");
        for (name, argv) in [
            ("node", &["node", "--no-warnings", pi, "-p", "hi"][..]),
            ("opencode.exe", &["opencode", "run", "hi"]),
            ("grok.exe", &["grok", "-p", "hi"]),
            ("claude.exe", &["claude", "-p", "hi"]),
            ("codex.exe", &["codex", "exec", "fix it"]),
            ("codex.exe", &["codex", "login"]),
            ("node.exe", &["node", "server.js"]),
        ] {
            assert!(!interactive_agent(&fg(name, argv)), "{argv:?}");
        }
    }

    #[test]
    fn instances_are_known_while_their_folder_runs() {
        let mut known = Known::<()>::new();
        known.note(1, 100_000, "a", 0);
        known.note(2, 200_000, "a", 0);
        known.note(3, 50_000, "b", 0);
        // 2 ended; 1 runs on, so 2 is still the bound between them.
        known.note(1, 100_000, "a", SEEN_MS * 3);
        assert_eq!(known.folder("a").len(), 2);
        assert!(known.folder("b").is_empty(), "nothing runs there any more");
        // The same pid with another start is another process.
        known.note(1, 900_000, "a", SEEN_MS * 3);
        let a = known.folder("a");
        assert_eq!(a.iter().map(|i| (i.pid, i.start)).collect::<Vec<_>>(), [(2, 200_000), (1, 900_000)]);
    }

    #[test]
    fn reads_utc_times() {
        assert_eq!(utc_ms(1970, 1, 1, 0, 0, 0), Some(0));
        assert_eq!(utc_ms(2000, 3, 1, 0, 0, 0), Some(951_868_800_000));
        assert_eq!(utc_ms(2026, 11, 7, 5, 19, 41), Some(1_794_028_781_000));
        assert_eq!(utc_ms(2026, 13, 1, 0, 0, 0), None);
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
        let (id, cwd) = session_id(file, 19204, None).unwrap();
        assert_eq!(id, "a6525362-cde8-43e2-8b43-74775173664b");
        assert_eq!(project_folder(&cwd), "C--x");
        assert_eq!(project_folder(r"C:\Aivan\PL\herdr"), "C--Aivan-PL-herdr");
        assert_eq!(session_id(file, 1, None), None, "another process's file");
        assert_eq!(session_id(br#"{"pid":1,"sessionId":"x; rm -rf /"}"#, 1, None), None);
        // Left by an older process under the same pid: another tab's, maybe.
        let dated = br#"{"pid":7,"sessionId":"a6525362-cde8-43e2-8b43-74775173664b","startedAt":1000000}"#;
        assert!(session_id(dated, 7, Some(1_000_500)).is_some(), "its own, written as it started");
        assert_eq!(session_id(dated, 7, Some(1_000_000 + SLACK_MS + 1)), None);
    }

    #[test]
    fn tabs_lost_together_never_come_back_to_one_conversation() {
        let c = |w: &[&str]| Some(w.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        let mut cmds = vec![
            ("/p".to_owned(), c(&["claude", "--resume", "a6525362-cde8-43e2-8b43-74775173664b"])),
            ("/p".to_owned(), c(&["claude", "--resume", "a6525362-cde8-43e2-8b43-74775173664b"])),
            ("/p".to_owned(), c(&["codex", "resume", "--last"])),
            ("/p/".to_owned(), c(&["codex", "resume", "--last"])),
            ("/q".to_owned(), c(&["codex", "resume", "--last"])),
            ("/p".to_owned(), c(&["claude"])),
            ("/p".to_owned(), c(&["claude"])),
            ("/p".to_owned(), None),
        ];
        distinct(&mut cmds);
        let got: Vec<Option<String>> = cmds.iter().map(|(_, c)| c.as_ref().map(|c| c.join(" "))).collect();
        assert_eq!(
            got,
            [
                Some("claude --resume a6525362-cde8-43e2-8b43-74775173664b".into()),
                Some("claude".into()),
                Some("codex resume --last".into()),
                Some("codex".into()),
                Some("codex resume --last".into()),
                Some("claude".into()),
                Some("claude".into()),
                None,
            ]
        );
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
