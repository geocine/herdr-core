//! Codex keeps every conversation (thread) it has open locked: its native
//! process (`codex`, under `node codex.js` when npm installed it) holds
//! `$CODEX_HOME/thread-writer-locks/<id>.lock` open, and once the thread is
//! saved (at its first message) its rollout,
//! `sessions/<y>/<m>/<d>/rollout-<time>-<id>.jsonl`, too. A lock is held by one
//! process at a time, so the threads a codex has open are its own: several in
//! one folder each come back to theirs (`codex resume <id>`). Beside its own
//! thread it holds its helpers' (guardian, subagents), whose rollouts say so.
//!
//! Where the files a process has open can't be read (macOS, a codex on a
//! shared app server, or not yet started), it comes back the old way, to the
//! folder's last (`codex resume --last`), but only while no other codex here
//! runs: two tabs never both get the folder's last.

use super::{Foreground, SEEN_MS, base, is_uuid, now_ms, words};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::UNIX_EPOCH;

/// The files a codex has open are read again this often at least, and at
/// once when a thread is locked or let go (the locks' folder changes).
const RESCAN_MS: u64 = 5_000;

/// What codex was started with that matters here.
#[derive(Default, Debug, PartialEq)]
pub(super) struct Args {
    /// `codex resume <id>`: the thread it opened.
    id: Option<String>,
    /// `codex agents`: the command center, which holds no one thread.
    agents: bool,
}

/// Options that take a value (`codex --help`).
const VALUED: &[&str] = &[
    "-c",
    "--config",
    "--enable",
    "--disable",
    "--remote",
    "--remote-auth-token-env",
    "-i",
    "--image",
    "-m",
    "--model",
    "--local-provider",
    "-p",
    "--profile",
    "-s",
    "--sandbox",
    "-C",
    "--cd",
    "--add-dir",
    "-a",
    "--ask-for-approval",
];

/// Subcommands that aren't the interactive app.
const NOT_APP: &[&str] = &[
    "exec",
    "e",
    "review",
    "login",
    "logout",
    "mcp",
    "mcp-server",
    "plugin",
    "app-server",
    "remote-control",
    "app",
    "completion",
    "update",
    "doctor",
    "sandbox",
    "debug",
    "apply",
    "a",
    "queue",
    "archive",
    "delete",
    "unarchive",
    "migrate-rollouts",
    "migrate-nfs-db",
    "cloud",
    "exec-server",
    "features",
    "help",
    "responses-api-proxy",
    "stdio-to-uds",
];

/// Whether a word of a command line is codex's program.
pub(super) fn is_codex(word: &str) -> bool {
    let (lower, base) = base(word);
    base == "codex" || lower.contains("@openai/codex")
}

/// codex's options, or None for a run that isn't the interactive app
/// (`codex exec`, `codex login`, ...).
pub(super) fn args(argv: &[String]) -> Option<Args> {
    let from = argv.iter().position(|w| is_codex(w)).map_or(argv.len().min(1), |i| i + 1);
    let mut words: Vec<&str> = Vec::new();
    let mut it = argv[from..].iter();
    while let Some(w) = it.next() {
        let w = w.as_str();
        if w == "--" {
            words.extend(it.map(String::as_str));
            break;
        }
        if w.len() > 1 && w.starts_with('-') {
            if matches!(w, "-h" | "--help" | "-V" | "--version") {
                return None;
            }
            if VALUED.contains(&w) {
                it.next();
            }
            continue;
        }
        words.push(w);
    }
    let mut a = Args::default();
    match words.first() {
        Some(sub) if NOT_APP.contains(sub) => return None,
        Some(&"resume") => a.id = words.get(1).filter(|id| is_uuid(id)).map(|id| (*id).to_owned()),
        Some(&"agents") => a.agents = true,
        // `fork`, a prompt, or nothing: the app, in a new thread.
        _ => {}
    }
    Some(a)
}

/// The threads a codex holds, from the files it has open.
#[derive(Default, Debug, Clone, PartialEq)]
struct Held {
    /// Every thread it holds the lock of.
    locks: Vec<String>,
    /// Saved ones: id and rollout.
    rollouts: Vec<(String, PathBuf)>,
}

/// What the files a codex holds say.
#[derive(Debug, PartialEq)]
enum Found {
    /// It is in this saved thread.
    Thread(String),
    /// It holds threads, none of its own saved yet (no message yet).
    Unsaved,
    /// It holds none: the files can't say (a shared app server, or starting).
    Nothing,
}

/// The threads in a list of open files. Paths only: the ids are checked.
fn held(files: &[String]) -> Held {
    let mut h = Held::default();
    for f in files {
        let path = f.replace('\\', "/");
        let Some((dir, name)) = path.rsplit_once('/') else { continue };
        if let Some(id) = name.strip_suffix(".lock").filter(|id| is_uuid(id))
            && dir.ends_with("/thread-writer-locks")
        {
            if !h.locks.iter().any(|l| l == id) {
                h.locks.push(id.to_owned());
            }
        } else if let Some(stem) = name.strip_prefix("rollout-").and_then(|n| n.strip_suffix(".jsonl"))
            && stem.len() > 36
            && path.contains("/sessions/")
            && let Some(id) = stem.get(stem.len() - 36..).filter(|id| is_uuid(id))
            && !h.rollouts.iter().any(|(r, _)| r == id)
        {
            h.rollouts.push((id.to_owned(), PathBuf::from(f.strip_prefix(r"\\?\").unwrap_or(f))));
        }
    }
    h
}

/// The thread a codex is in: of the saved ones it holds that are its own (not
/// a helper's, `main`), the one written to last. Right after `/new` it still
/// holds the one before, until the new one is saved: that one, or `last`
/// (what it was in before) while it still holds its lock.
fn found(h: &Held, main: impl Fn(&Path) -> bool, written: impl Fn(&Path) -> Option<u64>, last: Option<&str>) -> Found {
    let newest = h.rollouts.iter().filter(|(_, path)| main(path)).max_by_key(|(_, path)| written(path));
    if let Some((id, _)) = newest {
        return Found::Thread(id.clone());
    }
    match last {
        Some(last) if h.locks.iter().any(|l| l == last) => Found::Thread(last.to_owned()),
        _ if !h.locks.is_empty() => Found::Unsaved,
        _ => Found::Nothing,
    }
}

/// Whether a rollout is a thread of codex's own, not a helper's: its first
/// line (`session_meta`) has `source` as a word (`cli`, `vscode`, `exec`),
/// where a helper's is an object (`{"subagent":...}`). One that can't be read
/// counts as its own.
fn own_thread(path: &Path) -> Option<bool> {
    use std::io::Read;
    let mut head = Vec::with_capacity(32 * 1024);
    std::fs::File::open(path).ok()?.take(256 * 1024).read_to_end(&mut head).ok()?;
    let line = &head[..head.iter().position(|b| *b == b'\n').unwrap_or(head.len())];
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) {
        let source = &v["payload"]["source"];
        return (!source.is_null()).then(|| source.is_string());
    }
    // Longer than read: `source` comes before the long instructions.
    let text = String::from_utf8_lossy(line);
    let at = text.find("\"source\":")? + "\"source\":".len();
    Some(text[at..].trim_start().starts_with('"'))
}

fn written(path: &Path) -> Option<u64> {
    let m = std::fs::metadata(path).ok()?;
    Some(m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// A process: its pid and when it started.
type Proc = (i32, u64);

#[derive(Default)]
struct State {
    /// The files each native codex had open, when read, and the locks' folder's time then.
    scans: HashMap<Proc, (u64, Option<u64>, Vec<String>)>,
    /// Whether each rollout is codex's own thread (it never changes).
    own: HashMap<PathBuf, bool>,
    /// The thread each codex (the foreground process) was last found in.
    last: HashMap<Proc, (String, u64)>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

pub(super) fn command(f: &Foreground) -> Option<Vec<String>> {
    let a = args(&f.argv)?;
    if a.agents {
        return Some(words(&["codex", "agents"]));
    }
    let now = now_ms();
    let start = crate::platform::process_started_ms(f.pid);
    let native = native(f).and_then(|pid| Some((pid, crate::platform::process_started_ms(pid)?)));
    let files = native.and_then(|n| open_files(n, now));
    let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    st.last.retain(|_, (_, at)| now.saturating_sub(*at) < SEEN_MS);
    let me = start.map(|s| (f.pid, s));
    let last = me.and_then(|me| st.last.get(&me)).map(|(id, _)| id.clone());
    let found = files.map(|files| {
        let h = held(&files);
        if st.own.len() > 4096 {
            st.own.clear();
        }
        let mut mains: Vec<&Path> = Vec::new();
        for (_, path) in &h.rollouts {
            let own = match st.own.get(path) {
                Some(own) => *own,
                None => own_thread(path).inspect(|own| _ = st.own.insert(path.clone(), *own)).unwrap_or(true),
            };
            if own {
                mains.push(path);
            }
        }
        found(&h, |p| mains.contains(&p), written, last.as_deref())
    });
    drop(st);
    let alone = !super::others_here("codex", f);
    let cmd = decide(found, last, &a, alone);
    let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    if let (Some(me), Some(id)) = (me, resumed(&cmd)) {
        st.last.insert(me, (id.to_owned(), now));
    }
    Some(cmd)
}

/// The resume command, from what its files say (None: unreadable), the
/// thread it was last found in, how it was started, and whether it is the
/// only codex running in its folder.
fn decide(found: Option<Found>, last: Option<String>, a: &Args, alone: bool) -> Vec<String> {
    match found {
        Some(Found::Thread(id)) => words(&["codex", "resume", &id]),
        Some(Found::Unsaved) => words(&["codex"]),
        // A moment the files can't be read keeps what they last said.
        Some(Found::Nothing) | None => match last.as_deref().or(a.id.as_deref()) {
            Some(id) => words(&["codex", "resume", id]),
            None if alone => words(&["codex", "resume", "--last"]),
            // Another codex here: the folder's last may be its.
            None => words(&["codex"]),
        },
    }
}

/// The thread a resume command opens.
fn resumed(cmd: &[String]) -> Option<&str> {
    match cmd {
        [_, sub, id] if sub == "resume" && is_uuid(id) => Some(id),
        _ => None,
    }
}

/// The native codex: the foreground process itself, or its child under the
/// `node codex.js` npm runs.
fn native(f: &Foreground) -> Option<i32> {
    if base(&f.name).1 == "codex" {
        return Some(f.pid);
    }
    let children = crate::platform::child_processes(f.pid);
    children.into_iter().find(|(_, name)| base(name).1 == "codex").map(|(pid, _)| pid)
}

/// The files codex `n` has open, read again when its locks may have changed.
fn open_files(n: Proc, now: u64) -> Option<Vec<String>> {
    let locks_dir = {
        let st = STATE.lock().unwrap_or_else(|p| p.into_inner());
        st.scans.get(&n).and_then(|(_, _, files)| locks_dir(files))
    }
    .or_else(|| Some(codex_home()?.join("thread-writer-locks")));
    let stamp = locks_dir.as_deref().and_then(written);
    {
        let st = STATE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, was, files)) = st.scans.get(&n)
            && *was == stamp
            && now.saturating_sub(*at) < RESCAN_MS
        {
            return Some(files.clone());
        }
    }
    let files = crate::platform::open_files(n.0)?;
    let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    st.scans.retain(|_, (at, _, _)| now.saturating_sub(*at) < 6 * RESCAN_MS);
    st.scans.insert(n, (now, stamp, files.clone()));
    Some(files)
}

/// The locks' folder, from a lock among `files`.
fn locks_dir(files: &[String]) -> Option<PathBuf> {
    files.iter().find(|f| f.replace('\\', "/").contains("/thread-writer-locks/")).and_then(|f| {
        let f = f.strip_prefix(r"\\?\").unwrap_or(f);
        Path::new(f).parent().map(Path::to_path_buf)
    })
}

/// `CODEX_HOME`, else `~/.codex`.
fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(super::home()?.join(".codex")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| (*s).to_owned()).collect()
    }

    const ALPHA: &str = "01a1b4bb-7229-7b73-84be-dbf49b09d796";
    const BETA: &str = "01a1b4bf-8820-7c70-9b05-ff1a464cbb97";
    const GUARD: &str = "01a1b4bb-73bc-7000-8000-000000000001";
    const EPSILON: &str = "01a1b4c3-eaaa-7dc0-ae87-39e6b5e6e590";

    fn lock(id: &str) -> String {
        format!(r"\\?\C:\Users\x\.codex\thread-writer-locks\{id}.lock")
    }

    fn rollout(id: &str) -> String {
        format!(r"\\?\C:\Users\x\.codex\sessions\2026\11\07\rollout-2026-11-07T14-39-06-{id}.jsonl")
    }

    #[test]
    fn reads_what_codex_was_started_with() {
        let js = r"C:\Users\x\nodejs\node_modules\@openai\codex\bin\codex.js";
        assert_eq!(args(&argv(&["node", js])), Some(Args::default()));
        assert_eq!(args(&argv(&["codex", "fix the tests"])), Some(Args::default()));
        assert_eq!(args(&argv(&["codex", "-m", "gpt-5", "-p", "work", "hi"])), Some(Args::default()));
        assert_eq!(args(&argv(&["codex", "resume", ALPHA])).unwrap().id.as_deref(), Some(ALPHA));
        assert_eq!(args(&argv(&["codex", "resume", "--last"])).unwrap().id, None);
        assert_eq!(args(&argv(&["codex", "resume", "x; rm -rf /"])).unwrap().id, None);
        assert_eq!(args(&argv(&["codex", "-c", "model=o3", "resume", ALPHA])).unwrap().id.as_deref(), Some(ALPHA));
        assert!(args(&argv(&["codex", "agents"])).unwrap().agents);
        assert_eq!(args(&argv(&["codex", "fork", ALPHA])), Some(Args::default()), "a new thread");
        // An option's value is not a subcommand.
        assert_eq!(args(&argv(&["codex", "-C", "exec", "hi"])), Some(Args::default()));
        for sub in [&["codex", "exec", "hi"][..], &["codex", "e", "hi"], &["codex", "login"], &["codex", "--version"]] {
            assert_eq!(args(&argv(sub)), None, "{sub:?}");
        }
    }

    #[test]
    fn finds_the_threads_in_the_files_it_holds() {
        let files = [
            lock(ALPHA),
            rollout(ALPHA),
            lock(GUARD),
            r"C:\Users\x\.codex\thread-writer-locks\.coordination.lock".to_owned(),
            r"C:\Users\x\.codex\thread-writer-locks\x; rm -rf.lock".to_owned(),
            r"C:\Users\x\.codex\history.jsonl".to_owned(),
            format!(r"C:\elsewhere\rollout-x-{BETA}.jsonl"),
            "/home/u/.codex/sessions/2026/11/07/rollout-2026-11-07T14-39-06-".to_owned() + EPSILON + ".jsonl",
        ];
        let h = held(&files);
        assert_eq!(h.locks, [ALPHA, GUARD]);
        let ids: Vec<&str> = h.rollouts.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, [ALPHA, EPSILON]);
        assert!(h.rollouts[0].1.starts_with(r"C:\Users"), "without the \\\\?\\ prefix");
        assert_eq!(locks_dir(&files), Some(PathBuf::from(r"C:\Users\x\.codex\thread-writer-locks")));
    }

    /// Codex processes in one folder, each with the files it holds, as tabs.
    struct Tabs {
        files: HashMap<i32, Vec<String>>,
        /// Rollouts that are a helper's.
        helpers: Vec<String>,
        /// When each rollout was last written.
        times: HashMap<String, u64>,
        last: HashMap<i32, String>,
    }

    impl Tabs {
        fn new() -> Self {
            Tabs { files: HashMap::new(), helpers: Vec::new(), times: HashMap::new(), last: HashMap::new() }
        }

        fn resume(&mut self, pid: i32) -> Vec<String> {
            let found = self.files.get(&pid).map(|files| {
                let h = held(files);
                let helpers: Vec<PathBuf> =
                    self.helpers.iter().map(|id| held(&[rollout(id)]).rollouts[0].1.clone()).collect();
                let time = |p: &Path| {
                    self.times.iter().find(|(id, _)| p.to_string_lossy().contains(id.as_str())).map(|(_, t)| *t)
                };
                found(&h, |p| !helpers.iter().any(|x| x == p), time, self.last.get(&pid).map(String::as_str))
            });
            let cmd = decide(found, self.last.get(&pid).cloned(), &Args::default(), self.files.len() < 2);
            if let Some(id) = resumed(&cmd) {
                self.last.insert(pid, id.to_owned());
            }
            cmd
        }
    }

    #[test]
    fn tabs_in_one_folder_each_come_back_to_their_own_thread() {
        let mut t = Tabs::new();
        // A and B each said something; C is fresh (its thread's lock and its guardian's, nothing saved).
        t.files.insert(1, vec![lock(ALPHA), rollout(ALPHA), lock(GUARD)]);
        t.files.insert(2, vec![lock(BETA), rollout(BETA)]);
        t.files.insert(3, vec![lock("01a1b4c0-0000-7000-8000-000000000003")]);
        t.times.extend([(ALPHA.to_owned(), 10), (BETA.to_owned(), 20)]);
        assert_eq!(t.resume(1), ["codex", "resume", ALPHA]);
        assert_eq!(t.resume(2), ["codex", "resume", BETA]);
        assert_eq!(t.resume(3), ["codex"], "nothing saved: fresh, never another tab's");
        // A runs /new: it still holds alpha and a new lock until it says something.
        t.files.get_mut(&1).unwrap().push(lock(EPSILON));
        assert_eq!(t.resume(1), ["codex", "resume", ALPHA]);
        // Its first message saves epsilon and lets alpha go.
        t.files.insert(1, vec![lock(EPSILON), rollout(EPSILON), lock(GUARD)]);
        t.times.insert(EPSILON.to_owned(), 30);
        assert_eq!(t.resume(1), ["codex", "resume", EPSILON]);
        assert_eq!(t.resume(2), ["codex", "resume", BETA], "B is unaffected");
        // A subagent's saved thread, written last, is not A's.
        let sub = "01a1b4c4-0000-7000-8000-00000000000a";
        t.files.get_mut(&1).unwrap().extend([lock(sub), rollout(sub)]);
        t.helpers.push(sub.to_owned());
        t.times.insert(sub.to_owned(), 40);
        assert_eq!(t.resume(1), ["codex", "resume", EPSILON]);
        // B exits; C says something.
        t.files.remove(&2);
        let gamma = "01a1b4c0-0000-7000-8000-000000000003";
        t.files.insert(3, vec![lock(gamma), rollout(gamma)]);
        assert_eq!(t.resume(3), ["codex", "resume", gamma]);
    }

    #[test]
    fn without_its_files_it_keeps_its_last_and_never_shares_the_folders_last() {
        let a = Args::default();
        // Read once, then not: the thread it was in.
        assert_eq!(decide(None, Some(ALPHA.into()), &a, false), ["codex", "resume", ALPHA]);
        assert_eq!(decide(Some(Found::Nothing), Some(ALPHA.into()), &a, false), ["codex", "resume", ALPHA]);
        // Never read: alone in its folder, the folder's last, as before.
        assert_eq!(decide(None, None, &a, true), ["codex", "resume", "--last"]);
        // With another codex here, the folder's last may be the other's.
        assert_eq!(decide(None, None, &a, false), ["codex"]);
        // Started in a thread: that one.
        let given = Args { id: Some(BETA.into()), agents: false };
        assert_eq!(decide(None, None, &given, false), ["codex", "resume", BETA]);
    }

    #[test]
    fn a_helpers_rollout_says_so_on_its_first_line() {
        let dir = std::env::temp_dir().join(format!("herdr-codex-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let own = dir.join("own.jsonl");
        let helper = dir.join("helper.jsonl");
        let long = dir.join("long.jsonl");
        std::fs::write(&own, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"x\",\"source\":\"cli\"}}\n{}\n")
            .unwrap();
        let sub = "{\"type\":\"session_meta\",\"payload\":{\"source\":{\"subagent\":{\"other\":\"guardian\"}}}}\n";
        std::fs::write(&helper, sub).unwrap();
        let big =
            format!("{{\"payload\":{{\"source\":\"vscode\",\"base_instructions\":\"{}\"}}}}\n", "x".repeat(300_000));
        std::fs::write(&long, big).unwrap();
        assert_eq!(own_thread(&own), Some(true));
        assert_eq!(own_thread(&helper), Some(false));
        assert_eq!(own_thread(&long), Some(true), "longer than read");
        assert_eq!(own_thread(&dir.join("none.jsonl")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// This test's own process stands in for codex, holding a lock and a rollout open.
    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn a_running_codex_comes_back_to_the_thread_it_holds() {
        let home = std::env::temp_dir().join(format!("herdr-codex-run-{}", std::process::id()));
        let (locks, day) = (home.join("thread-writer-locks"), home.join("sessions").join("2026").join("11").join("07"));
        std::fs::create_dir_all(&locks).unwrap();
        std::fs::create_dir_all(&day).unwrap();
        let id = "01a1b4bb-7229-7b73-84be-000000000077";
        let _lock = std::fs::File::create(locks.join(format!("{id}.lock"))).unwrap();
        let path = day.join(format!("rollout-2026-11-07T14-39-06-{id}.jsonl"));
        std::fs::write(&path, "{\"type\":\"session_meta\",\"payload\":{\"source\":\"cli\"}}\n").unwrap();
        let _rollout = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        let f = Foreground {
            pid: std::process::id() as i32,
            name: "codex.exe".into(),
            argv: argv(&["codex"]),
            cwd: home.display().to_string(),
        };
        assert_eq!(command(&f).unwrap(), ["codex", "resume", id]);
        drop((_lock, _rollout));
        let _ = std::fs::remove_dir_all(&home);
    }
}
