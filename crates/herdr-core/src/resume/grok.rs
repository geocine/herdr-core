//! Grok (xAI's `grok` TUI) lists the sessions it has open, with the process
//! that has each, in `~/.grok/active_sessions.json` (`GROK_HOME` moves
//! `~/.grok`), and saves each as `sessions/<url-encoded folder>/<id>/`
//! (`chat_history.jsonl` is there from the start; `updates.jsonl` grows as
//! it goes on). Each prompt is noted, with its session, in the folder's
//! `prompt_history.jsonl`. `grok --resume <id>` opens one again.
//!
//! One process can have several open (`/new` keeps the one before open): it is
//! in the one it opened or was given a prompt in last. Switching back to one
//! still open (`/resume`) writes nothing anywhere, so until it is given a
//! prompt there, it comes back to the one before (its own still).

use super::{Foreground, SEEN_MS, SLACK_MS, home, now_ms, utc_ms, words};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// What grok was started with that matters here.
#[derive(Default, Debug, PartialEq)]
struct Args {
    /// `-r`/`--resume <id>`: the session it opened.
    id: Option<String>,
    /// `-c`/`--continue`.
    continued: bool,
    /// `grok dashboard`: the Agent Dashboard, which holds several.
    dashboard: bool,
}

pub(super) fn command(f: &Foreground) -> Option<Vec<String>> {
    if !is_app(&f.argv) {
        return None;
    }
    let a = args(&f.argv);
    if a.dashboard {
        return Some(words(&["grok", "dashboard"]));
    }
    let others = super::others_here("grok", f);
    Some(match session(f.pid) {
        Found::Session(id, true) => words(&["grok", "--resume", &id]),
        // Nothing saved: it starts fresh.
        Found::Session(_, false) => words(&["grok"]),
        // Listed nowhere yet (it lists its session a few seconds after it
        // starts), or the list can't be read: the one it was started in, else
        // the folder's last if it was started so and no other grok runs there.
        Found::Unlisted | Found::Unread => match (a.id, a.continued) {
            (Some(id), _) => words(&["grok", "--resume", &id]),
            (None, true) if !others => words(&["grok", "--continue"]),
            (None, _) => words(&["grok"]),
        },
    })
}

/// Whether this grok is the interactive app: not `grok sessions`, `grok -p`, ...
pub(super) fn is_app(argv: &[String]) -> bool {
    let from = argv.iter().position(|w| super::is_grok(w)).map_or(argv.len().min(1), |i| i + 1);
    let rest = &argv[from..];
    let command = rest.first().is_some_and(|w| {
        matches!(
            w.as_str(),
            "agent"
                | "clone"
                | "completions"
                | "doctor"
                | "du"
                | "disk-usage"
                | "export"
                | "help"
                | "inspect"
                | "leader"
                | "login"
                | "logout"
                | "mcp"
                | "memory"
                | "models"
                | "plugin"
                | "sessions"
                | "setup"
                | "trace"
                | "update"
                | "usage"
                | "version"
                | "v"
                | "worker"
                | "worktree"
                | "wrap"
        )
    });
    let headless = rest.iter().any(|w| {
        let k = w.split_once('=').map_or(w.as_str(), |(k, _)| k);
        matches!(k, "-p" | "--single" | "--prompt-file" | "--prompt-json" | "-h" | "--help" | "-v" | "--version")
    });
    !command && !headless
}

fn args(argv: &[String]) -> Args {
    let from = argv.iter().position(|w| super::is_grok(w)).map_or(argv.len().min(1), |i| i + 1);
    let rest = &argv[from..];
    let mut a = Args::default();
    for (i, w) in rest.iter().enumerate() {
        let next = rest.get(i + 1).map(String::as_str);
        match w.split_once('=') {
            Some(("--resume", id)) => a.id = Some(id.to_owned()),
            _ => match w.as_str() {
                "-r" | "--resume" => a.id = next.map(str::to_owned),
                "-c" | "--continue" => a.continued = true,
                // The subcommand, not an option's value.
                "dashboard" => a.dashboard |= i == 0 || !rest[i - 1].starts_with('-'),
                _ => {}
            },
        }
    }
    // Anything else after `--resume` is a title (or nothing: the last one).
    a.id = a.id.filter(|id| uuid(id));
    a
}

/// What grok's list says of a process.
#[derive(Debug, Clone, PartialEq)]
enum Found {
    /// Its session, and whether it is saved yet.
    Session(String, bool),
    /// The list says nothing of it.
    Unlisted,
    /// The list can't be read.
    Unread,
}

/// Grok `pid`'s session. A moment's failure to read the list (grok rewrites
/// it) keeps the last answer, and so does a grok the list no longer has a
/// row for once it had one: it takes its row off as it quits, then stops
/// its tools.
fn session(pid: i32) -> Found {
    /// (pid, when it started) -> (its last session, when). By start too: a
    /// pid used again is another process, maybe another tab's.
    type Last = HashMap<(i32, Option<u64>), (Option<(String, bool)>, u64)>;
    static LAST: LazyLock<Mutex<Last>> = LazyLock::new(Default::default);
    let now = now_ms();
    let start = crate::platform::process_started_ms(pid);
    let found = grok_home().and_then(|g| {
        let list = std::fs::read(g.join("active_sessions.json")).ok()?;
        let rows = rows(&list, pid, start)?;
        let sessions = g.join("sessions");
        Some(in_use(&sessions, rows).map(|(id, cwd, _)| {
            let saved = saved(&sessions, &cwd, &id);
            (id, saved)
        }))
    });
    let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
    last.retain(|_, (_, at)| now.saturating_sub(*at) < SEEN_MS);
    let answer = decide(found, last.get(&(pid, start)).and_then(|(s, _)| s.clone()));
    let keep = match &answer {
        Found::Session(id, saved) => Some((id.clone(), *saved)),
        _ => None,
    };
    last.insert((pid, start), (keep, now));
    answer
}

/// What grok's list says (None: it can't be read; Some(None): no row for this
/// process), with the session it last had (`before`).
fn decide(found: Option<Option<(String, bool)>>, before: Option<(String, bool)>) -> Found {
    match (found, before) {
        (Some(Some((id, saved))), _) | (_, Some((id, saved))) => Found::Session(id, saved),
        (Some(None), None) => Found::Unlisted,
        (None, None) => Found::Unread,
    }
}

/// Of the sessions a grok has open (`/new`, the dashboard, `/fork`), the one
/// opened or given a prompt last. A turn still running in one it left writes
/// on (its `updates.jsonl`), so writing says which only where no prompts are
/// noted.
fn in_use(sessions: &Path, rows: Vec<(String, String, String)>) -> Option<(String, String, String)> {
    let mut prompted: HashMap<String, Option<HashMap<String, u64>>> = HashMap::new();
    for (_, cwd, _) in &rows {
        prompted.entry(folder(cwd)).or_insert_with(|| last_prompts(&sessions.join(folder(cwd))));
    }
    rows.into_iter().max_by_key(|(id, cwd, opened)| {
        let opened_ms = rfc3339_ms(opened).unwrap_or(0);
        match prompted.get(&folder(cwd)) {
            Some(Some(prompts)) => (opened_ms.max(prompts.get(id).copied().unwrap_or(0)), None, opened.clone()),
            _ => (0, written(sessions, cwd, id), opened.clone()),
        }
    })
}

/// When each session of a folder (`group`) was last given a prompt, from the
/// end of its `prompt_history.jsonl`; None without one. Read again only when
/// it changed.
fn last_prompts(group: &Path) -> Option<HashMap<String, u64>> {
    type Cache = HashMap<PathBuf, ((u64, SystemTime), HashMap<String, u64>)>;
    static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);
    use std::io::{Read, Seek, SeekFrom};
    let path = group.join("prompt_history.jsonl");
    let m = std::fs::metadata(&path).ok()?;
    let stamp = (m.len(), m.modified().ok()?);
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((was, prompts)) = cache.get(&path)
        && *was == stamp
    {
        return Some(prompts.clone());
    }
    const TAIL: u64 = 64 * 1024;
    let mut file = std::fs::File::open(&path).ok()?;
    let from = stamp.0.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut tail = Vec::new();
    file.take(TAIL).read_to_end(&mut tail).ok()?;
    let prompts = prompt_times(&tail, from > 0);
    if cache.len() >= 64 {
        cache.clear();
    }
    cache.insert(path, (stamp, prompts.clone()));
    Some(prompts)
}

/// Session id -> its last prompt's time, from lines of `prompt_history.jsonl`
/// (`cut`: the first is part of one).
fn prompt_times(tail: &[u8], cut: bool) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    for line in tail.split(|b| *b == b'\n').skip(usize::from(cut)) {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { continue };
        let (Some(id), Some(at)) = (v["session_id"].as_str(), v["timestamp"].as_str().and_then(rfc3339_ms)) else {
            continue;
        };
        let t = out.entry(id.to_owned()).or_insert(0);
        *t = at.max(*t);
    }
    out
}

fn grok_home() -> Option<PathBuf> {
    std::env::var_os("GROK_HOME").filter(|d| !d.is_empty()).map(PathBuf::from).or_else(|| Some(home()?.join(".grok")))
}

/// Whether process `pid` is grok's, by where its program is (`agent` is a name
/// grok installs itself under too): in `$GROK_HOME/bin`.
pub(super) fn is_own_program(pid: i32) -> bool {
    let (Some(exe), Some(bin)) = (crate::platform::process_path(pid), grok_home().map(|g| g.join("bin"))) else {
        return false;
    };
    let (exe, bin) = (super::base(&exe).0, super::base(&bin.to_string_lossy()).0);
    exe.strip_prefix(bin.trim_end_matches('/')).is_some_and(|rest| rest.starts_with('/'))
}

/// This process's open sessions in grok's list (`list`): id, folder and when
/// opened, for ids that are UUIDs (grok takes anything else after `--resume`
/// for a title). One opened before the process started (`start`) is a gone
/// grok's. None if the list isn't one.
fn rows(list: &[u8], pid: i32, start: Option<u64>) -> Option<Vec<(String, String, String)>> {
    let v: serde_json::Value = serde_json::from_slice(list).ok()?;
    let rows = v
        .as_array()?
        .iter()
        .filter(|r| r["pid"].as_i64() == Some(pid as i64))
        .filter(|r| {
            let opened = r["opened_at"].as_str().and_then(rfc3339_ms);
            start.is_none_or(|s| opened.is_none_or(|o| o + SLACK_MS >= s))
        })
        .filter_map(|r| {
            let id = r["session_id"].as_str().filter(|id| uuid(id))?;
            let cwd = r["cwd"].as_str().unwrap_or_default();
            Some((id.to_owned(), cwd.to_owned(), r["opened_at"].as_str().unwrap_or_default().to_owned()))
        })
        .collect();
    Some(rows)
}

/// `2026-11-07T05:19:41.297163300Z` as UTC ms.
pub(super) fn rfc3339_ms(at: &str) -> Option<u64> {
    let b = at.as_bytes();
    if b.len() < 20 || !at.is_ascii() || b[4] != b'-' || b[10] != b'T' || !at.ends_with('Z') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| at[r].parse::<u64>().ok();
    // A fraction of a second, to the ms: `.5` is 500.
    let ms = match at.get(19..20) {
        Some(".") => format!("{:0<3}", &at[20..(at.len() - 1).min(23)]).parse::<u64>().ok()?,
        _ => 0,
    };
    Some(utc_ms(n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?)? + ms)
}

fn uuid(id: &str) -> bool {
    super::is_uuid(id)
}

/// When session `id` (of folder `cwd`) was last written to, if it was.
fn written(sessions: &Path, cwd: &str, id: &str) -> Option<u64> {
    let m = std::fs::metadata(sessions.join(folder(cwd)).join(id).join("updates.jsonl")).ok()?;
    Some(m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// Whether session `id` (of folder `cwd`) is saved (grok 1.0.77 writes its
/// history as it opens it, so a new one is too, and opens again by id).
fn saved(sessions: &Path, cwd: &str, id: &str) -> bool {
    let has = |group: &Path| group.join(id).join("chat_history.jsonl").is_file();
    let group = sessions.join(folder(cwd));
    if group.join(id).is_dir() {
        return has(&group);
    }
    // A folder grok named another way (a long path gets a short name): look in each.
    std::fs::read_dir(sessions).into_iter().flatten().flatten().any(|e| has(&e.path()))
}

/// Grok's name for a folder: URL-encoded, keeping letters, digits and `-._~`.
fn folder(cwd: &str) -> String {
    cwd.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &[u8] = br#"[
      {"session_id":"01a1b472-b88f-7c83-96bf-c8dd7a821540","pid":35920,"cwd":"C:\\Users\\Aivan\\cm-re","opened_at":"2026-11-07T05:19:41.297163300Z"},
      {"session_id":"01a0473f-2eda-7032-a4dd-a2bfa5415ef6","pid":35920,"cwd":"C:\\Users\\Aivan\\cm-re","opened_at":"2026-11-07T05:30:00.000000000Z"},
      {"session_id":"x; rm -rf /","pid":7,"cwd":"C:\\x","opened_at":"2026-11-07T05:19:41Z"}
    ]"#;

    fn argv(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn finds_its_own_open_sessions() {
        let ids = |pid, start| rows(LIST, pid, start).map(|r| r.into_iter().map(|(id, ..)| id).collect::<Vec<_>>());
        assert_eq!(
            ids(35920, None).unwrap(),
            ["01a1b472-b88f-7c83-96bf-c8dd7a821540", "01a0473f-2eda-7032-a4dd-a2bfa5415ef6"]
        );
        assert_eq!(rows(LIST, 35920, None).unwrap()[0].1, r"C:\Users\Aivan\cm-re");
        assert_eq!(ids(1, None).unwrap(), [""; 0], "another process's");
        assert_eq!(ids(7, None).unwrap(), [""; 0], "not an id");
        // Started after both were opened: a gone grok's rows, under a pid used again.
        assert_eq!(ids(35920, rfc3339_ms("2026-11-07T06:00:00Z")).unwrap(), [""; 0]);
        assert_eq!(ids(35920, rfc3339_ms("2026-11-07T05:25:00Z")).unwrap(), ["01a0473f-2eda-7032-a4dd-a2bfa5415ef6"]);
        assert_eq!(rows(b"not json", 35920, None), None);
        assert_eq!(rows(b"{}", 35920, None), None);
    }

    #[test]
    fn reads_grok_times_and_folders() {
        assert_eq!(rfc3339_ms("1970-01-01T00:00:01.5Z"), Some(1_500));
        assert_eq!(rfc3339_ms("2026-11-07T05:19:41.297163300Z"), Some(1_794_028_781_297));
        assert_eq!(rfc3339_ms("yesterday"), None);
        assert_eq!(folder(r"C:\Users\Aivan\cm-re"), "C%3A%5CUsers%5CAivan%5Ccm-re");
        assert_eq!(folder(r"E:\Reverse\Assembly\Week 1"), "E%3A%5CReverse%5CAssembly%5CWeek%201");
        assert!(!uuid("01a1b472-b88f-7c83-96bf-c8dd7a82154"));
    }

    #[test]
    fn knows_the_app_from_its_commands() {
        assert!(is_app(&argv(&[r"C:\Users\x\.grok\bin\grok.exe"])));
        assert!(is_app(&argv(&["grok", "fix the tests"])));
        assert!(is_app(&argv(&["grok", "-m", "grok-4", "dashboard"])));
        assert!(!is_app(&argv(&["grok", "sessions", "list"])));
        assert!(!is_app(&argv(&["grok", "-p", "hi"])));
        assert!(!is_app(&argv(&["grok", "--prompt-file=x.md"])));
        let id = "01a1b472-b88f-7c83-96bf-c8dd7a821540";
        assert_eq!(args(&argv(&["grok", "-r", id])).id.as_deref(), Some(id));
        assert_eq!(args(&argv(&["grok", &format!("--resume={id}")])).id.as_deref(), Some(id));
        assert_eq!(args(&argv(&["grok", "--resume", "my title"])).id, None);
        assert!(args(&argv(&["grok", "-c"])).continued);
        assert!(args(&argv(&["grok", "dashboard"])).dashboard);
        assert!(args(&argv(&["grok", "-m", "grok-4", "dashboard"])).dashboard);
        assert!(!args(&argv(&["grok", "--cwd", "dashboard"])).dashboard, "a folder named so");
    }

    #[test]
    fn a_grok_off_the_list_keeps_its_session() {
        let s = |id: &str| Some((id.to_owned(), true));
        assert_eq!(decide(Some(s("a")), s("b")), Found::Session("a".into(), true));
        // Quitting: its row is gone, the process not yet.
        assert_eq!(decide(Some(None), s("b")), Found::Session("b".into(), true));
        assert_eq!(decide(None, s("b")), Found::Session("b".into(), true), "a moment's failure to read");
        // Just started: not listed yet.
        assert_eq!(decide(Some(None), None), Found::Unlisted);
        assert_eq!(decide(None, None), Found::Unread);
    }

    #[test]
    fn of_several_open_the_one_opened_or_prompted_last() {
        let sessions = std::env::temp_dir().join(format!("herdr-grok-prompted-{}", std::process::id()));
        let group = sessions.join(folder(r"C:\x"));
        let (old, new) = ("01a1b4bb-d034-7492-9800-4f69b95393ba", "01a1b4c0-ce2c-7000-8000-04d86a52d2e5");
        for id in [old, new] {
            std::fs::create_dir_all(group.join(id)).unwrap();
        }
        let list = format!(
            r#"[{{"session_id":"{old}","pid":1,"cwd":"C:\\x","opened_at":"2026-11-07T06:40:00Z"}},
                {{"session_id":"{new}","pid":1,"cwd":"C:\\x","opened_at":"2026-11-07T06:44:58Z"}}]"#
        );
        let rows = || rows(list.as_bytes(), 1, None).unwrap();
        let prompt =
            |id: &str, at: &str| format!("{{\"timestamp\":\"{at}\",\"session_id\":\"{id}\",\"prompt\":\"x\"}}\n");
        // A long turn started in the old one, then `/new`: the old one is written on, the new one shown.
        std::fs::write(group.join("prompt_history.jsonl"), prompt(old, "2026-11-07T06:44:53Z")).unwrap();
        std::fs::write(group.join(new).join("updates.jsonl"), "{}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(group.join(old).join("updates.jsonl"), "{}\n").unwrap();
        assert_eq!(in_use(&sessions, rows()).unwrap().0, new, "opened after the last prompt");
        // A prompt given in the old one again: that one.
        let both = prompt(old, "2026-11-07T06:44:53Z") + &prompt(old, "2026-11-07T06:50:00Z");
        std::fs::write(group.join("prompt_history.jsonl"), both).unwrap();
        assert_eq!(in_use(&sessions, rows()).unwrap().0, old);
        std::fs::remove_dir_all(&sessions).unwrap();
        // The end of a long history: its first line cut off.
        let tail = format!("t\":\"x\"}}\n{}", prompt(new, "2026-11-07T06:44:53.5Z"));
        assert_eq!(prompt_times(tail.as_bytes(), true).get(new), rfc3339_ms("2026-11-07T06:44:53.5Z").as_ref());
    }

    #[test]
    fn of_several_open_the_one_written_to_last() {
        let sessions = std::env::temp_dir().join(format!("herdr-grok-several-{}", std::process::id()));
        let (old, new) = ("01a1b472-b88f-7c83-96bf-c8dd7a821540", "01a0473f-2eda-7032-a4dd-a2bfa5415ef6");
        for id in [old, new] {
            std::fs::create_dir_all(sessions.join(folder(r"C:\x")).join(id)).unwrap();
        }
        std::fs::write(sessions.join(folder(r"C:\x")).join(new).join("updates.jsonl"), "{}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(sessions.join(folder(r"C:\x")).join(old).join("updates.jsonl"), "{}\n").unwrap();
        let list = format!(
            r#"[{{"session_id":"{old}","pid":1,"cwd":"C:\\x","opened_at":"2026-11-07T05:00:00Z"}},
                {{"session_id":"{new}","pid":1,"cwd":"C:\\x","opened_at":"2026-11-07T06:00:00Z"}}]"#
        );
        assert_eq!(in_use(&sessions, rows(list.as_bytes(), 1, None).unwrap()).unwrap().0, old, "opened first, in use");
        std::fs::remove_dir_all(sessions.join(folder(r"C:\x")).join(old)).unwrap();
        std::fs::remove_file(sessions.join(folder(r"C:\x")).join(new).join("updates.jsonl")).unwrap();
        assert_eq!(in_use(&sessions, rows(list.as_bytes(), 1, None).unwrap()).unwrap().0, new, "else opened last");
        std::fs::remove_dir_all(&sessions).unwrap();
    }

    #[test]
    fn a_session_is_saved_once_it_has_a_message() {
        let sessions = std::env::temp_dir().join(format!("herdr-grok-test-{}", std::process::id()));
        let id = "01a1b472-b88f-7c83-96bf-c8dd7a821540";
        let dir = sessions.join(folder(r"C:\x y")).join(id);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!saved(&sessions, r"C:\x y", id));
        std::fs::write(dir.join("chat_history.jsonl"), "{}\n").unwrap();
        assert!(saved(&sessions, r"C:\x y", id));
        // Found under a folder named another way too.
        assert!(saved(&sessions, r"C:\a much longer path", id));
        std::fs::remove_dir_all(&sessions).unwrap();
    }
}
