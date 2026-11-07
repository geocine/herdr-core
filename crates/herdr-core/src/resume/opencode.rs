//! OpenCode keeps its conversations as rows of one SQLite database
//! (`~/.local/share/opencode/opencode.db`, on Windows too), each made at its
//! first message, and nothing there says which process has which. Two things
//! do. Its log (`log/opencode.log` there, one for every process) marks each
//! line with the process's own run id: which folder it opened as it started,
//! each conversation it made and each it was given a prompt in. A run is
//! bound to the opencode that started just before it in that folder (or that
//! was started in a conversation the run went on with), and its conversations
//! are then that opencode's, whatever the others do. And the terminal title:
//! the app shows `OC | <the conversation's title>`, which says which one is
//! shown even when it wasn't given a prompt. Without a bound run, one made
//! here after this opencode started is its own, when no other opencode there
//! fits better. `opencode --session <id>` opens one again.
//!
//! The database is read through the system's SQLite (`sqlite.rs`). A Linux
//! release build is static and can't load one: there, OpenCode comes back to
//! its run's last conversation, else the one it was started in, else the
//! folder's last (`--continue`) when no other opencode runs there.

use super::{Foreground, Instance, Known, SLACK_MS, home, now_ms, words};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

/// A run's first line comes this long after its process started at most.
const BIND_MS: u64 = 15_000;
/// Two opencodes started closer than this may have their runs' first lines
/// the other way round.
const APART_MS: u64 = 1_000;
/// At most this much of the log is read the first time.
const LOG_TAIL: u64 = 8 << 20;

/// The database changes with every word an agent writes: read it at most this
/// often (and when an opencode's title changes).
const REREAD_MS: u64 = 1_000;

#[derive(Debug, Clone, PartialEq)]
struct Row {
    id: String,
    dir: String,
    title: String,
    created: u64,
    updated: u64,
}

/// What opencode was started with that matters here.
#[derive(Default, Debug, PartialEq)]
pub(super) struct Args {
    /// `-s`/`--session` (without `--fork`): the conversation it opened.
    id: Option<String>,
    /// `-c`/`--continue`.
    continued: bool,
    /// `--mini`: the minimal interface.
    mini: bool,
}

/// What is known of a running opencode.
#[derive(Default, Debug, Clone)]
struct Oc {
    /// The conversation it was started in.
    id: Option<String>,
    /// Its terminal title when last seen.
    title: String,
    /// Its run in the log, once bound.
    run: Option<String>,
    /// The conversation it was last found in.
    last: Option<String>,
}

/// One process's run, from the log.
#[derive(Default, Debug, Clone, PartialEq)]
struct Run {
    /// When it opened its first folder, and which.
    first: u64,
    dir: String,
    /// The root conversations it made or was given a prompt in, and when last.
    ids: Vec<(String, u64)>,
}

impl Run {
    fn has(&self, id: &str) -> bool {
        self.ids.iter().any(|(i, _)| i == id)
    }

    fn at(&self, id: &str) -> Option<u64> {
        self.ids.iter().find(|(i, _)| i == id).map(|(_, at)| *at)
    }
}

/// The log as read so far.
#[derive(Default)]
struct Log {
    path: Option<PathBuf>,
    /// How far it was read.
    read: u64,
    runs: HashMap<String, Run>,
}

impl Oc {
    /// Whether it may be in conversation `r`: its title shows that one's, or none.
    fn may_show(&self, r: &Row) -> bool {
        self.title.strip_prefix("OC | ").is_none_or(|shown| title_matches(&r.title, shown))
    }
}

#[derive(Default)]
struct State {
    db: Option<PathBuf>,
    /// The database's and its log's sizes and times when last read.
    stamp: Option<[Option<(u64, SystemTime)>; 2]>,
    /// None when it couldn't be read.
    rows: Option<Vec<Row>>,
    read_at: u64,
    known: Known<Oc>,
    /// The opencode each conversation was last shown in, by its title.
    claims: HashMap<String, i32>,
    log: Log,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Default::default);

pub(super) fn command(f: &Foreground, title: &str) -> Option<Vec<String>> {
    let a = args(&f.argv)?;
    let Some(start) = crate::platform::process_started_ms(f.pid) else {
        return Some(fallback(&a, None, true));
    };
    let now = now_ms();
    let folder = norm(&f.cwd);
    let mut st = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let me = st.known.note(f.pid, start, &folder, now);
    let retitled = me.data.title != title;
    (me.data.id, me.data.title) = (a.id.clone(), title.to_owned());
    let known: HashSet<i32> = st.known.0.iter().map(|i| i.pid).collect();
    st.claims.retain(|_, pid| known.contains(pid));
    refresh_log(&mut st.log, log_path());
    let mut here = st.known.folder(&folder);
    bind(&st.log.runs, &mut here);
    for i in &here {
        if let Some(k) = st.known.0.iter_mut().find(|k| k.pid == i.pid && k.folder == i.folder) {
            k.data.run.clone_from(&i.data.run);
        }
    }
    let alone = !here.iter().any(|i| i.pid != f.pid && now.saturating_sub(i.seen) < super::SEEN_MS);
    let mine = here.iter().find(|i| i.pid == f.pid).and_then(|i| st.log.runs.get(i.data.run.as_ref()?)).cloned();
    // What the others' runs made or were prompted in is theirs.
    let mut claims = st.claims.clone();
    for i in here.iter().filter(|i| i.pid != f.pid) {
        for (id, _) in i.data.run.as_ref().and_then(|r| st.log.runs.get(r)).map_or(&[][..], |r| &r.ids[..]) {
            claims.insert(id.clone(), i.pid);
        }
    }
    let Some(db) = db_path() else { return Some(fallback(&a, mine.as_ref(), alone)) };
    refresh(&mut st, &db, now, retitled);
    let Some(rows) = &st.rows else { return Some(fallback(&a, mine.as_ref(), alone)) };
    let mini: &[&str] = if a.mini { &["--mini"] } else { &[] };
    let picked = match &mine {
        Some(run) => pick_bound(rows, &f.cwd, title, f.pid, run, &claims),
        None => pick(rows, &f.cwd, title, f.pid, &here, &claims),
    };
    let me_last = here.iter().find(|i| i.pid == f.pid).and_then(|i| i.data.last.clone());
    let Some((row, by_title)) = picked else {
        // A conversation is shown that can't be told (a subagent's, a plugin's page): the last it was in.
        if title.starts_with("OC | ")
            && let Some(last) = me_last.filter(|id| id_ok(id))
        {
            return Some(words(&[&["opencode"], mini, &["--session", &last]].concat()));
        }
        let cont = (a.id.is_some() || a.continued) && alone;
        let cmd: &[&str] = if cont { &["opencode", "--continue"] } else { &["opencode"] };
        return Some(words(&[cmd, mini].concat()));
    };
    let id = row.id.clone();
    if !id_ok(&id) {
        return Some(fallback(&a, mine.as_ref(), alone));
    }
    if by_title {
        st.claims.insert(id.clone(), f.pid);
    }
    if let Some(k) = st.known.0.iter_mut().find(|k| k.pid == f.pid && k.folder == folder) {
        k.data.last = Some(id.clone());
    }
    Some(words(&[&["opencode"], mini, &["--session", &id]].concat()))
}

/// Without the database: its run's last conversation, else the one it was
/// started in, else the folder's last when it is `alone` there (two would
/// share it).
fn fallback(a: &Args, run: Option<&Run>, alone: bool) -> Vec<String> {
    let mut cmd = words(&["opencode"]);
    if a.mini {
        cmd.push("--mini".into());
    }
    let latest = run.and_then(|r| r.ids.iter().max_by_key(|(_, at)| *at)).map(|(id, _)| id.clone());
    match latest.filter(|id| id_ok(id)).or_else(|| a.id.clone()) {
        Some(id) => cmd.extend(["--session".into(), id]),
        None if alone => cmd.push("--continue".into()),
        None => {}
    }
    cmd
}

/// `log/opencode.log` in OpenCode's data folder.
fn log_path() -> Option<PathBuf> {
    Some(data_dir()?.join("log").join("opencode.log"))
}

/// Reads what was added to the log since last time (all of it, up to
/// `LOG_TAIL` from its end, the first time; again from the start when it
/// shrank). Only whole lines; the rest waits for its end.
fn refresh_log(log: &mut Log, path: Option<PathBuf>) {
    use std::io::{Read, Seek, SeekFrom};
    let Some(path) = path else { return };
    if log.path.as_ref() != Some(&path) {
        *log = Log { path: Some(path.clone()), ..Log::default() };
        if let Ok(m) = std::fs::metadata(&path) {
            log.read = m.len().saturating_sub(LOG_TAIL);
        }
    }
    let Ok(m) = std::fs::metadata(&path) else { return };
    if m.len() < log.read {
        log.read = 0;
        log.runs.clear();
    }
    if m.len() == log.read {
        return;
    }
    let Ok(mut file) = std::fs::File::open(&path) else { return };
    if file.seek(SeekFrom::Start(log.read)).is_err() {
        return;
    }
    let mut new = Vec::new();
    if file.take(m.len() - log.read).read_to_end(&mut new).is_err() {
        return;
    }
    let Some(end) = new.iter().rposition(|b| *b == b'\n') else { return };
    // Read from the middle the first time: its first line is part of one.
    let from_middle = log.read > 0 && log.runs.is_empty();
    let lines = new[..end].split(|b| *b == b'\n').skip(usize::from(from_middle));
    for line in lines {
        note_line(&mut log.runs, &String::from_utf8_lossy(line));
    }
    log.read += end as u64 + 1;
    if log.runs.len() > 1024 {
        let mut by_age: Vec<(u64, String)> = log.runs.iter().map(|(k, r)| (r.first, k.clone())).collect();
        by_age.sort();
        for (_, k) in &by_age[..by_age.len() - 512] {
            log.runs.remove(k);
        }
    }
}

/// What one line of the log says of its run.
fn note_line(runs: &mut HashMap<String, Run>, line: &str) {
    if !line.contains(" run=") {
        return;
    }
    let fields = fields(line);
    let get = |k: &str| fields.iter().find(|(key, _)| *key == k).map(|(_, v)| v.as_str());
    let (Some(run), Some(message), Some(at)) =
        (get("run"), get("message"), get("timestamp").and_then(super::grok::rfc3339_ms))
    else {
        return;
    };
    if run.len() != 8 || !run.bytes().all(|b| b.is_ascii_hexdigit()) {
        return;
    }
    let id = match message {
        "creating instance" => {
            let r = runs.entry(run.to_owned()).or_default();
            if r.dir.is_empty() {
                (r.first, r.dir) = (at, norm(get("directory").unwrap_or_default()));
            }
            return;
        }
        // A root conversation it made.
        "created" if get("parentID") == Some("undefined") => get("id"),
        // A prompt in one (step 0: the prompt itself).
        "loop" if get("step") == Some("0") => get("session.id"),
        _ => None,
    };
    let Some(id) = id.filter(|id| id_ok(id)) else { return };
    let r = runs.entry(run.to_owned()).or_default();
    match r.ids.iter_mut().find(|(i, _)| i == id) {
        Some(seen) => seen.1 = seen.1.max(at),
        None => r.ids.push((id.to_owned(), at)),
    }
}

/// `key=value` and `key="quoted \"value\""` pairs of a log line.
fn fields(line: &str) -> Vec<(&str, String)> {
    let mut out = Vec::new();
    let mut rest = line.trim_start();
    while let Some(eq) = rest.find('=') {
        let key = &rest[..eq];
        if key.contains(' ') {
            // Not a pair: skip the word.
            rest = rest[key.find(' ').unwrap_or(key.len())..].trim_start();
            continue;
        }
        rest = &rest[eq + 1..];
        let value = if let Some(quoted) = rest.strip_prefix('"') {
            let mut value = String::new();
            let mut chars = quoted.char_indices();
            let mut end = quoted.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next().map(|(_, c)| c)),
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    c => value.push(c),
                }
            }
            rest = &quoted[end.min(quoted.len())..];
            value
        } else {
            let end = rest.find(' ').unwrap_or(rest.len());
            let value = rest[..end].to_owned();
            rest = &rest[end..];
            value
        };
        out.push((key, value));
        rest = rest.trim_start();
    }
    out
}

/// Binds runs to the opencodes `here` (one folder) that have none yet: the
/// run that went on with the conversation an opencode was started in, else
/// the one that opened this folder just after an opencode started, when no
/// other opencode or run fits too (several started apart get theirs in turn).
fn bind(runs: &HashMap<String, Run>, here: &mut [Instance<Oc>]) {
    let bound: HashSet<String> = here.iter().filter_map(|i| i.data.run.clone()).collect();
    let free = |k: &String, bound: &HashSet<String>| !bound.contains(k);
    let mut bound = bound;
    for i in here.iter_mut().filter(|i| i.data.run.as_ref().is_none_or(|r| !runs.contains_key(r))) {
        i.data.run = None;
        let Some(id) = i.data.id.clone() else { continue };
        let went_on: Vec<&String> = runs
            .iter()
            .filter(|(k, r)| free(k, &bound) && r.first + SLACK_MS >= i.start && r.has(&id))
            .map(|(k, _)| k)
            .collect();
        if let [one] = went_on[..] {
            bound.insert(one.clone());
            i.data.run = Some(one.clone());
        }
    }
    let fits = |i: &Instance<Oc>, r: &Run| r.dir == i.folder && i.start <= r.first && r.first <= i.start + BIND_MS;
    let mut open: Vec<usize> = (0..here.len()).filter(|i| here[*i].data.run.is_none()).collect();
    open.sort_by_key(|i| here[*i].start);
    let mut cand: Vec<(&String, &Run)> =
        runs.iter().filter(|(k, r)| free(k, &bound) && open.iter().any(|i| fits(&here[*i], r))).collect();
    cand.sort_by_key(|(k, r)| (r.first, (*k).clone()));
    let want: Vec<usize> = open.iter().copied().filter(|i| cand.iter().any(|(_, r)| fits(&here[*i], r))).collect();
    // As many runs as opencodes that could have them, each in turn, the opencodes well apart.
    let apart = want.windows(2).all(|w| here[w[1]].start >= here[w[0]].start + APART_MS);
    if !want.is_empty()
        && want.len() == cand.len()
        && apart
        && want.iter().zip(&cand).all(|(i, (_, r))| fits(&here[*i], r))
    {
        for (i, (k, _)) in want.iter().zip(&cand) {
            here[*i].data.run = Some((*k).clone());
        }
        return;
    }
    // Else only pairs that fit nothing else.
    for i in want {
        let mine: Vec<&(&String, &Run)> = cand.iter().filter(|(_, r)| fits(&here[i], r)).collect();
        if let [(k, r)] = mine[..]
            && here.iter().filter(|o| o.data.run.is_none() && fits(o, r)).count() == 1
        {
            here[i].data.run = Some((*k).clone());
        }
    }
}

/// The conversation an opencode with a bound run (`run`) is in: the one its
/// title shows (of several so titled, one its run made or was prompted in,
/// else one no other opencode's is), else the one its run was last in.
fn pick_bound<'a>(
    rows: &'a [Row],
    cwd: &str,
    title: &str,
    pid: i32,
    run: &Run,
    claims: &HashMap<String, i32>,
) -> Option<(&'a Row, bool)> {
    let others = |r: &Row| claims.get(&r.id).is_some_and(|p| *p != pid);
    let latest = || rows.iter().filter(|r| run.has(&r.id)).max_by_key(|r| run.at(&r.id));
    if let Some(shown) = title.strip_prefix("OC | ") {
        let found: Vec<&Row> = rows.iter().filter(|r| title_matches(&r.title, shown)).collect();
        let own = found.iter().filter(|r| run.has(&r.id)).max_by_key(|r| run.at(&r.id));
        if let Some(own) = own {
            return Some((own, false));
        }
        let mut free: Vec<&Row> = found.into_iter().filter(|r| !others(r)).collect();
        if free.len() > 1 && free.iter().any(|r| same_dir(&r.dir, cwd)) {
            free.retain(|r| same_dir(&r.dir, cwd));
        }
        match free[..] {
            [one] => return Some((one, true)),
            [] => {}
            ref several => {
                let best = several
                    .iter()
                    .find(|r| claims.get(&r.id) == Some(&pid))
                    .or_else(|| several.iter().max_by_key(|r| r.updated));
                return best.map(|r| (*r, false));
            }
        }
    }
    latest().map(|r| (r, false))
}

/// opencode's options, or None when it isn't the app (`opencode run`, `serve`,
/// `attach`, ...). `opencode pr <n>` runs the app once the branch is out.
pub(super) fn args(argv: &[String]) -> Option<Args> {
    let from = argv.iter().position(|w| super::is_opencode(w)).map_or(argv.len().min(1), |i| i + 1);
    let rest = &argv[from..];
    if rest.first().is_some_and(|w| {
        matches!(
            w.as_str(),
            "run"
                | "serve"
                | "web"
                | "acp"
                | "attach"
                | "session"
                | "db"
                | "export"
                | "import"
                | "stats"
                | "models"
                | "providers"
                | "auth"
                | "agent"
                | "mcp"
                | "upgrade"
                | "uninstall"
                | "debug"
                | "github"
                | "plugin"
                | "plug"
                | "completion"
                | "-h"
                | "--help"
                | "-v"
                | "--version"
        )
    }) {
        return None;
    }
    let (mut a, mut fork) = (Args::default(), false);
    let mut it = rest.iter();
    while let Some(w) = it.next() {
        match w.as_str() {
            "-s" | "--session" => a.id = it.next().cloned(),
            "-c" | "--continue" => a.continued = true,
            "--fork" => fork = true,
            "--mini" => a.mini = true,
            w => {
                if let Some(id) = w.strip_prefix("--session=") {
                    a.id = Some(id.to_owned());
                }
            }
        }
    }
    // A fork is a new conversation, made from that one.
    a.id = a.id.filter(|id| id_ok(id) && !fork);
    a.continued |= fork;
    Some(a)
}

/// OpenCode's data folder: `$XDG_DATA_HOME/opencode` or `~/.local/share/opencode`.
fn data_dir() -> Option<PathBuf> {
    let data = match std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => home()?.join(".local").join("share"),
    };
    Some(data.join("opencode"))
}

/// `OPENCODE_DB` (a path, or one in its data folder), else `opencode.db` there.
fn db_path() -> Option<PathBuf> {
    let data = data_dir()?;
    match std::env::var("OPENCODE_DB") {
        Ok(db) if db == ":memory:" => None,
        Ok(db) if !db.is_empty() => Some(data.join(db)),
        _ => Some(data.join("opencode.db")),
    }
}

/// Reads the conversations again when the database changed (its new rows
/// are in the `-wal` log first): at most once a second, but at once when an
/// opencode's title changed (it just named a conversation the rows read may
/// not have named yet). A failed read is tried again.
fn refresh(st: &mut State, db: &Path, now: u64, retitled: bool) {
    let stat = |p: &Path| std::fs::metadata(p).ok().and_then(|m| Some((m.len(), m.modified().ok()?)));
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    let stamp = [stat(db), stat(Path::new(&wal))];
    let read = st.db.as_deref() == Some(db) && st.rows.is_some();
    let same = read && st.stamp == Some(stamp);
    if same || (read && !retitled && now.saturating_sub(st.read_at) < REREAD_MS) {
        return;
    }
    if st.rows.is_none() && st.db.as_deref() == Some(db) && now.saturating_sub(st.read_at) < REREAD_MS {
        return;
    }
    st.rows = super::sqlite::rows(
        db,
        c"SELECT id, directory, title, time_created, time_updated FROM session \
          WHERE parent_id IS NULL AND time_archived IS NULL ORDER BY time_updated DESC LIMIT 1000",
        5,
    )
    .map(|rows| {
        rows.into_iter()
            .filter_map(|r| {
                let n = |i: usize| r[i].as_deref()?.parse::<u64>().ok();
                Some(Row {
                    id: r[0].clone()?,
                    dir: r[1].clone().unwrap_or_default(),
                    title: r[2].clone().unwrap_or_default(),
                    created: n(3)?,
                    updated: n(4)?,
                })
            })
            .collect()
    });
    (st.db, st.stamp, st.read_at) = (Some(db.to_owned()), Some(stamp), now);
}

/// The conversation opencode `pid` is in (of the opencodes `known` in its
/// folder, `cwd`), and whether its title alone said so:
/// 1. the one the terminal title shows. Of several with that title: one here
///    no other opencode showed, then its guess, then the one it showed before,
///    then the one written to last;
/// 2. else its guess (`guess`).
fn pick<'a>(
    rows: &'a [Row],
    cwd: &str,
    title: &str,
    pid: i32,
    known: &[Instance<Oc>],
    claims: &HashMap<String, i32>,
) -> Option<(&'a Row, bool)> {
    let me = known.iter().find(|i| i.pid == pid)?;
    let here = |r: &Row| same_dir(&r.dir, cwd);
    let others = |r: &Row| claims.get(&r.id).is_some_and(|p| *p != pid);
    let guess = guess(rows, cwd, pid, known, claims);
    if let Some(shown) = title.strip_prefix("OC | ") {
        let mut found: Vec<&Row> = rows.iter().filter(|r| title_matches(&r.title, shown)).collect();
        if found.len() > 1 && found.iter().any(|r| here(r)) {
            found.retain(|r| here(r));
        }
        if found.len() > 1 && found.iter().any(|r| !others(r)) {
            found.retain(|r| !others(r));
        }
        // One made here since it started with no title yet, read before it got the one shown,
        // when no other opencode here could have made it.
        let others_ran =
            |r: &Row| known.iter().any(|i| i.pid != pid && i.start <= r.created + SLACK_MS && i.live_at(r.created));
        let naming = rows
            .iter()
            .filter(|r| here(r) && r.created + SLACK_MS >= me.start && untitled(&r.title) && !others(r))
            .filter(|r| !others_ran(r))
            .max_by_key(|r| r.created);
        match found[..] {
            // Only one elsewhere: likelier the new one here, just being named.
            [one] if !here(one) && naming.is_some() => return naming.map(|r| (r, false)),
            [one] => return Some((one, true)),
            [] => {}
            ref several => {
                let best = several
                    .iter()
                    .find(|r| guess.is_some_and(|g| g.id == r.id))
                    .or_else(|| several.iter().find(|r| claims.get(&r.id) == Some(&pid)))
                    .or_else(|| several.iter().max_by_key(|r| r.updated));
                return best.map(|r| (*r, false));
            }
        }
    }
    guess.map(|r| (r, false))
}

/// The conversation opencode `pid` is guessed to be in: the one it was started
/// in, or one made here since it started that no title showed. Each of those
/// goes, oldest first, to the opencode here that started last before it was
/// made and has none yet (else just the last to start), of those whose title
/// shows that one or none. Of its own, the one written to last.
fn guess<'a>(
    rows: &'a [Row],
    cwd: &str,
    pid: i32,
    known: &[Instance<Oc>],
    claims: &HashMap<String, i32>,
) -> Option<&'a Row> {
    let me = known.iter().find(|i| i.pid == pid)?;
    let others = |r: &Row| claims.get(&r.id).is_some_and(|p| *p != pid);
    let mut mine: Vec<&Row> = rows.iter().filter(|r| me.data.id.as_deref() == Some(&r.id) && !others(r)).collect();
    let earliest = known.iter().map(|i| i.start).min().unwrap_or(me.start);
    let mut open: Vec<&Row> = rows
        .iter()
        .filter(|r| same_dir(&r.dir, cwd) && r.created + SLACK_MS >= earliest && !claims.contains_key(&r.id))
        .filter(|r| !known.iter().any(|i| i.data.id.as_deref() == Some(&r.id)))
        .collect();
    open.sort_by_key(|r| r.created);
    let mut has = HashSet::new();
    for r in open {
        // Of those running when it was made (one that had ended never takes it).
        let fits =
            || known.iter().filter(|i| i.data.may_show(r) && i.start <= r.created + SLACK_MS && i.live_at(r.created));
        let to =
            fits().filter(|i| !has.contains(&i.pid)).max_by_key(|i| i.start).or_else(|| fits().max_by_key(|i| i.start));
        if let Some(to) = to {
            has.insert(to.pid);
            if to.pid == pid {
                mine.push(r);
            }
        }
    }
    mine.into_iter().max_by_key(|r| r.updated)
}

/// The title OpenCode gives a conversation until it names it.
fn untitled(title: &str) -> bool {
    let Some(at) = title.strip_prefix("New session - ").or_else(|| title.strip_prefix("Child session - ")) else {
        return false;
    };
    let b = at.as_bytes();
    b.len() == 24 && b[10] == b'T' && b[23] == b'Z' && b[..4].iter().all(u8::is_ascii_digit)
}

/// A folder as the database writes it (with `/`), in one case where folders'
/// case doesn't matter.
fn norm(p: &str) -> String {
    let p = p.replace('\\', "/");
    let p = p.trim_end_matches('/');
    if crate::platform::PATHS_IGNORE_CASE { p.to_ascii_lowercase() } else { p.to_owned() }
}

fn same_dir(a: &str, b: &str) -> bool {
    norm(a) == norm(b)
}

/// The title shows a long one's first 37 characters and `…` (as JavaScript
/// counts them: UTF-16 units).
fn title_matches(saved: &str, shown: &str) -> bool {
    saved == shown
        || shown.strip_suffix('…').is_some_and(|start| {
            let units: Vec<u16> = saved.encode_utf16().collect();
            units.len() > 40 && !start.is_empty() && start.encode_utf16().eq(units.iter().take(37).copied())
        })
}

/// An OpenCode conversation id that is safe to start it with: `ses_` and letters and digits.
fn id_ok(id: &str) -> bool {
    id.len() > 4 && id.len() <= 64 && id.starts_with("ses_") && id[4..].bytes().all(|b| b.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| (*s).to_owned()).collect()
    }

    fn row(id: &str, dir: &str, title: &str, created: u64, updated: u64) -> Row {
        Row { id: id.into(), dir: dir.into(), title: title.into(), created, updated }
    }

    fn oc(pid: i32, start: u64, title: &str) -> Instance<Oc> {
        let data = Oc { title: title.into(), ..Oc::default() };
        // Running from its start until long after.
        Instance { pid, start, folder: "f".into(), seen: 10_000_000, data }
    }

    const EXE: &str = r"C:\Users\x\nodejs\node_modules\opencode-ai\bin\opencode.exe";

    #[test]
    fn reads_what_opencode_was_started_with() {
        assert_eq!(args(&argv(&[EXE])), Some(Args::default()));
        let id = "ses_e4b8dbe50ffevi9h6JoAPr2hfa";
        assert_eq!(args(&argv(&[EXE, "-s", id])).unwrap().id.as_deref(), Some(id));
        assert_eq!(args(&argv(&["opencode", &format!("--session={id}")])).unwrap().id.as_deref(), Some(id));
        let fork = args(&argv(&["opencode", "-s", id, "--fork"])).unwrap();
        assert_eq!((fork.id, fork.continued), (None, true));
        assert_eq!(args(&argv(&["opencode", "-s", "ses_x; rm -rf /"])).unwrap().id, None);
        assert!(args(&argv(&["opencode", "-c", "../proj"])).unwrap().continued);
        assert!(args(&argv(&["opencode", "--mini"])).unwrap().mini);
        assert_eq!(args(&argv(&["opencode", "pr", "12"])), Some(Args::default()), "the app, once the branch is out");
        // Not the app.
        for sub in ["run", "serve", "attach", "web", "session"] {
            assert_eq!(args(&argv(&[EXE, sub, "x"])), None, "{sub}");
        }
        let mini = Args { id: Some(id.into()), mini: true, ..Args::default() };
        assert_eq!(fallback(&mini, None, false), ["opencode", "--mini", "--session", id]);
        let plain = Args::default();
        assert_eq!(fallback(&plain, None, true), ["opencode", "--continue"], "alone: the folder's last");
        assert_eq!(fallback(&plain, None, false), ["opencode"], "another here: it may be its");
        let run = Run { ids: vec![("ses_a".into(), 5), ("ses_b".into(), 9)], ..Run::default() };
        assert_eq!(fallback(&plain, Some(&run), false), ["opencode", "--session", "ses_b"], "its run's last");
    }

    #[test]
    fn only_safe_ids_are_used() {
        assert!(id_ok("ses_e4b8dbe50ffevi9h6JoAPr2hfa"));
        for bad in ["ses_", "ses_x; rm -rf /", "ses_a-b", "msg_abc", "abc", &format!("ses_{}", "a".repeat(61))] {
            assert!(!id_ok(bad), "{bad}");
        }
    }

    #[test]
    fn folders_and_titles_compare_as_opencode_shows_them() {
        assert!(same_dir("/home/a/x/", "/home/a/x"));
        assert!(!same_dir("/home/a/x", "/home/a/y"));
        assert_eq!(same_dir("C:/Users/A/x", r"C:\users\a\x"), crate::platform::PATHS_IGNORE_CASE);
        let long = "Refactor the resume code for every agent we know";
        assert!(title_matches(long, &format!("{}…", long.chars().take(37).collect::<String>())));
        assert!(title_matches("Greeting", "Greeting"));
        // Counted as JavaScript does: an emoji is two.
        let wide = format!("{}{}", "😀".repeat(5), "a".repeat(35));
        assert!(title_matches(&wide, &format!("{}{}…", "😀".repeat(5), "a".repeat(27))));
        assert!(!title_matches("Greeting", "Greet…"), "short titles are shown whole");
        assert!(!title_matches("Greeting", "Other"));
        assert!(untitled("New session - 2026-11-07T05:19:41.297Z"));
        assert!(!untitled("New session - with a title"));
    }

    #[test]
    fn finds_its_own_conversation() {
        let cwd = "/u/cm-re";
        let rows = [
            row("ses_new", "/u/cm-re", "Greeting", 20_000, 30_000),
            row("ses_other", "/u/cm-re", "Fix the build", 25_000, 40_000),
            row("ses_old", "/u/cm-re", "Old work", 1_000, 2_000),
            row("ses_far", "/elsewhere", "Far away", 30_000, 50_000),
        ];
        let none = HashMap::new();
        let pick_id = |title: &str, start, claims: &HashMap<String, i32>| {
            let known = [oc(7, start, title)];
            pick(&rows, cwd, title, 7, &known, claims).map(|(r, t)| (r.id.as_str(), t))
        };
        // The title says which, even one from before it started or (nothing new here) elsewhere.
        assert_eq!(pick_id("OC | Greeting", 10_000, &none), Some(("ses_new", true)));
        assert_eq!(pick_id("OC | Old work", 10_000, &none), Some(("ses_old", true)));
        assert_eq!(pick_id("OC | Far away", 10_000, &none), Some(("ses_far", true)));
        // No title yet: the newest made here since it started that no other opencode showed.
        assert_eq!(pick_id("OpenCode", 10_000, &none), Some(("ses_other", false)));
        let shown = HashMap::from([("ses_other".to_owned(), 8)]);
        assert_eq!(pick_id("OpenCode", 10_000, &shown), Some(("ses_new", false)));
        // Nothing made since it started: nothing saved yet.
        assert_eq!(pick_id("OpenCode", 60_000, &none), None);
        // Started in one: that one, unless a newer one is its own.
        let started_in = |start| {
            let data = Oc { id: Some("ses_old".into()), ..Oc::default() };
            let known = [Instance { data, ..oc(7, start, "") }];
            pick(&rows, cwd, "OpenCode", 7, &known, &none).map(|(r, _)| r.id.as_str())
        };
        assert_eq!(started_in(60_000), Some("ses_old"));
        assert_eq!(started_in(15_000), Some("ses_other"));
        // Two with the shown title: the one in this folder.
        let twins = [row("ses_a", "/x", "Same", 1, 1), row("ses_b", "/u/cm-re", "Same", 1, 1)];
        let twin = pick(&twins, cwd, "OC | Same", 7, &[oc(7, 0, "")], &none).unwrap();
        assert_eq!(twin.0.id, "ses_b");
    }

    #[test]
    fn two_in_one_folder_each_get_their_own_whoever_is_read_first() {
        let cwd = "/u/p";
        let claims = HashMap::new();
        let ids = |rows: &[Row], titles: [&str; 2]| {
            // A in the older terminal, started first; B later.
            let known = [oc(1, 1_000, titles[0]), oc(2, 5_000, titles[1])];
            let get = |pid, t| pick(rows, cwd, t, pid, &known, &claims).map(|(r, _)| r.id.clone());
            (get(1, titles[0]), get(2, titles[1]))
        };
        // B made one, no title yet: B's; A has none.
        let rows = vec![row("ses_SB", "/u/p", "New session - 2026-11-07T05:19:41.297Z", 10_000, 10_500)];
        assert_eq!(ids(&rows, ["OpenCode", "OpenCode"]), (None, Some("ses_SB".into())));
        // Then A made one, and both are named alike.
        let rows =
            vec![row("ses_SB", "/u/p", "Greeting", 10_000, 10_500), row("ses_SA", "/u/p", "Greeting", 12_000, 13_000)];
        let both = ids(&rows, ["OC | Greeting", "OC | Greeting"]);
        assert_eq!(both, (Some("ses_SA".into()), Some("ses_SB".into())));
        // B started over (`/new`, no title yet) while A shows its own: the new one is B's.
        let mut rows = rows;
        rows.push(row("ses_SB2", "/u/p", "New session - 2026-11-07T05:30:00.000Z", 20_000, 20_100));
        let claims_a = HashMap::from([("ses_SA".to_owned(), 1)]);
        let known = [oc(1, 1_000, "OC | Greeting"), oc(2, 5_000, "OpenCode")];
        assert_eq!(pick(&rows, cwd, "OpenCode", 2, &known, &claims_a).map(|(r, _)| r.id.as_str()), Some("ses_SB2"));
    }

    #[test]
    fn a_title_just_shown_doesnt_take_an_older_conversation() {
        let cwd = "/u/p";
        let none = HashMap::new();
        let known = [oc(7, 10_000, "OC | Greeting")];
        // Rows read before the new one was named: the only "Greeting" is elsewhere.
        let stale = [
            row("ses_mine", "/u/p", "New session - 2026-11-07T05:19:41.297Z", 20_000, 20_500),
            row("ses_far", "/y", "Greeting", 1_000, 2_000),
        ];
        let got = pick(&stale, cwd, "OC | Greeting", 7, &known, &none).unwrap();
        assert_eq!((got.0.id.as_str(), got.1), ("ses_mine", false));
        // Read again: an older one here with that title, shown by it before, and the new one.
        let fresh =
            [row("ses_mine", "/u/p", "Greeting", 20_000, 20_500), row("ses_old", "/u/p", "Greeting", 1_000, 2_000)];
        let before = HashMap::from([("ses_old".to_owned(), 7)]);
        assert_eq!(pick(&fresh, cwd, "OC | Greeting", 7, &known, &before).unwrap().0.id, "ses_mine");
    }

    #[test]
    fn a_failed_read_is_tried_again() {
        let mut st = State::default();
        let db = std::env::temp_dir().join(format!("herdr-oc-none-{}.db", std::process::id()));
        refresh(&mut st, &db, 10_000, false);
        assert!(st.rows.is_none());
        st.rows = Some(Vec::new());
        st.read_at = 0;
        // Unchanged since a read that worked: kept.
        refresh(&mut st, &db, 20_000, false);
        assert_eq!(st.rows, Some(Vec::new()));
        // After one that failed, read again a second later though nothing changed.
        (st.rows, st.read_at) = (None, 20_000);
        refresh(&mut st, &db, 20_500, false);
        assert_eq!(st.read_at, 20_000, "not yet");
        refresh(&mut st, &db, 21_000, false);
        assert_eq!(st.read_at, 21_000);
    }

    /// Log lines as opencode 1.19 writes them.
    fn line(at: &str, run: &str, rest: &str) -> String {
        format!("timestamp={at} level=INFO run={run} {rest}")
    }

    fn instance(dir: &str) -> String {
        format!("message=\"creating instance\" directory=\"{}\"", dir.replace('\\', "\\\\"))
    }

    fn created(id: &str) -> String {
        format!(
            "message=created id={id} slug=sunny-lagoon version=1.19.0 projectID=global directory=\"C:\\\\u\\\\p\" \
             path=\"\" workspaceID=undefined parentID=undefined title=\"New session - 2026-11-07T05:19:09.231Z\" agent=build"
        )
    }

    fn prompt(id: &str) -> String {
        format!("message=loop session.id={id} step=0")
    }

    fn runs(lines: &[String]) -> HashMap<String, Run> {
        let mut runs = HashMap::new();
        for l in lines {
            note_line(&mut runs, l);
        }
        runs
    }

    const T0: &str = "2026-11-07T05:18:42.220Z";

    fn ms(at: &str) -> u64 {
        crate::resume::grok::rfc3339_ms(at).unwrap()
    }

    #[test]
    fn reads_each_runs_folder_and_conversations_from_the_log() {
        let lines = [
            line(T0, "7a1a3a97", &instance(r"C:\u\p")),
            line("2026-11-07T05:18:43.000Z", "7a1a3a97", &instance(r"C:\other")),
            line("2026-11-07T05:19:09.231Z", "7a1a3a97", &created("ses_e4b8dbe50ffevi9h6JoAPr2hfa")),
            line("2026-11-07T05:19:09.289Z", "7a1a3a97", &prompt("ses_e4b8dbe50ffevi9h6JoAPr2hfa")),
            line(
                "2026-11-07T05:19:12.248Z",
                "7a1a3a97",
                "message=loop session.id=ses_e4b8dbe50ffevi9h6JoAPr2hfa step=1",
            ),
            // A subagent's child conversation is not a root one.
            line(
                "2026-11-07T05:20:00.000Z",
                "7a1a3a97",
                "message=created id=ses_child1 parentID=ses_e4b8dbe50ffevi9h6JoAPr2hfa",
            ),
            line("2026-11-07T05:20:00.000Z", "bad", &prompt("ses_x")),
            "not a log line".to_owned(),
        ];
        let runs = runs(&lines);
        assert_eq!(runs.len(), 1);
        let r = &runs["7a1a3a97"];
        assert_eq!((r.first, r.dir.as_str()), (ms(T0), norm(r"C:\u\p").as_str()), "the first folder it opened");
        assert_eq!(r.ids, [("ses_e4b8dbe50ffevi9h6JoAPr2hfa".to_owned(), ms("2026-11-07T05:19:09.289Z"))]);
        let f = fields(r#"a=1 message="x \"y\" z" b=2"#);
        assert_eq!(f, [("a", "1".to_owned()), ("message", "x \"y\" z".to_owned()), ("b", "2".to_owned())]);
    }

    #[test]
    fn reads_the_log_as_it_grows() {
        let path = std::env::temp_dir().join(format!("herdr-oc-log-{}.log", std::process::id()));
        let first = line(T0, "7a1a3a97", &instance("/u/p")) + "\n";
        std::fs::write(&path, &first).unwrap();
        let mut log = Log::default();
        refresh_log(&mut log, Some(path.clone()));
        assert_eq!(log.runs["7a1a3a97"].dir, "/u/p");
        // A line half written waits for its end.
        let half = line("2026-11-07T05:19:09.289Z", "7a1a3a97", &prompt("ses_a"));
        std::fs::write(&path, format!("{first}{}", &half[..20])).unwrap();
        refresh_log(&mut log, Some(path.clone()));
        assert!(log.runs["7a1a3a97"].ids.is_empty());
        std::fs::write(&path, format!("{first}{half}\n")).unwrap();
        refresh_log(&mut log, Some(path.clone()));
        assert_eq!(log.runs["7a1a3a97"].ids.len(), 1);
        std::fs::remove_file(&path).unwrap();
    }

    /// Opencodes in one folder, started at `starts` (ms), as tabs.
    fn tabs(starts: &[u64]) -> Vec<Instance<Oc>> {
        starts.iter().enumerate().map(|(i, s)| Instance { folder: "/u/p".into(), ..oc(i as i32 + 1, *s, "") }).collect()
    }

    fn run(first: u64, ids: &[(&str, u64)]) -> Run {
        Run { first, dir: "/u/p".into(), ids: ids.iter().map(|(i, at)| ((*i).to_owned(), *at)).collect() }
    }

    fn bound(here: &[Instance<Oc>]) -> Vec<Option<&str>> {
        here.iter().map(|i| i.data.run.as_deref()).collect()
    }

    #[test]
    fn each_run_is_bound_to_the_opencode_that_started_it() {
        // Started 4 s apart; each opens its folder about 1.8 s after it starts.
        let runs = HashMap::from([("aaaaaaaa".to_owned(), run(1_800, &[])), ("bbbbbbbb".to_owned(), run(5_800, &[]))]);
        let mut here = tabs(&[0, 4_000]);
        bind(&runs, &mut here);
        assert_eq!(bound(&here), [Some("aaaaaaaa"), Some("bbbbbbbb")]);
        // Started 300 ms apart: either run may be either's, so neither is bound.
        let runs = HashMap::from([("aaaaaaaa".to_owned(), run(1_800, &[])), ("bbbbbbbb".to_owned(), run(2_000, &[]))]);
        let mut here = tabs(&[0, 300]);
        bind(&runs, &mut here);
        assert_eq!(bound(&here), [None, None]);
        // One started long after the other's run: only its own fits it.
        let runs = HashMap::from([("aaaaaaaa".to_owned(), run(1_800, &[])), ("bbbbbbbb".to_owned(), run(61_800, &[]))]);
        let mut here = tabs(&[0, 60_000]);
        here[0].data.run = Some("aaaaaaaa".into());
        bind(&runs, &mut here);
        assert_eq!(bound(&here), [Some("aaaaaaaa"), Some("bbbbbbbb")]);
        // Brought back in the same moment, each in its conversation: the run that went on with it.
        let runs = HashMap::from([
            ("old00000".to_owned(), run(1_000, &[("ses_SA", 2_000)])),
            ("aaaaaaaa".to_owned(), run(101_800, &[("ses_SA", 120_000)])),
            ("bbbbbbbb".to_owned(), run(101_810, &[("ses_SB", 125_000)])),
        ]);
        let mut here = tabs(&[100_000, 100_001]);
        here[0].data.id = Some("ses_SA".into());
        here[1].data.id = Some("ses_SB".into());
        bind(&runs, &mut here);
        assert_eq!(bound(&here), [Some("aaaaaaaa"), Some("bbbbbbbb")]);
    }

    #[test]
    fn tabs_with_bound_runs_each_get_their_own_whatever_the_titles() {
        let cwd = "/u/p";
        // A messaged first (SA), then B (SB); both came to be titled alike.
        let rows =
            vec![row("ses_SA", "/u/p", "Greeting", 10_000, 13_000), row("ses_SB", "/u/p", "Greeting", 12_000, 14_000)];
        let ra = run(1_800, &[("ses_SA", 10_000)]);
        let rb = run(5_800, &[("ses_SB", 12_000)]);
        // What each other's run had is the other's.
        let claims_for = |pid: i32| -> HashMap<String, i32> {
            let other = if pid == 1 { (&rb, 2) } else { (&ra, 1) };
            other.0.ids.iter().map(|(id, _)| (id.clone(), other.1)).collect()
        };
        let get = |pid: i32, title: &str, rows: &[Row]| {
            let r = if pid == 1 { &ra } else { &rb };
            pick_bound(rows, cwd, title, pid, r, &claims_for(pid)).map(|(r, _)| r.id.clone())
        };
        for titles in [["OpenCode", "OpenCode"], ["OC | Greeting", "OC | Greeting"]] {
            let got = (get(1, titles[0], &rows), get(2, titles[1], &rows));
            assert_eq!(got, (Some("ses_SA".into()), Some("ses_SB".into())), "{titles:?}");
        }
        // Titles off, A /new and messages: A's new one; B stays.
        let mut rows = rows;
        rows.push(row("ses_SA2", "/u/p", "New session - 2026-11-07T05:30:00.000Z", 20_000, 20_500));
        let ra = run(1_800, &[("ses_SA", 10_000), ("ses_SA2", 20_000)]);
        let got = pick_bound(&rows, cwd, "OpenCode", 1, &ra, &claims_for(1)).map(|(r, _)| r.id.as_str());
        assert_eq!(got, Some("ses_SA2"));
        assert_eq!(get(2, "OpenCode", &rows).as_deref(), Some("ses_SB"));
        // A tab that never messaged has none, never another's.
        let idle = run(9_800, &[]);
        assert_eq!(pick_bound(&rows, cwd, "OpenCode", 3, &idle, &claims_for(3)).map(|(r, _)| r.id.as_str()), None);
        // Showing a subagent's conversation (no root row has that title): its own last one.
        let sub = "OC | Explore the code (@explore subagent)";
        assert_eq!(pick_bound(&rows, cwd, sub, 1, &ra, &claims_for(1)).map(|(r, _)| r.id.as_str()), Some("ses_SA2"));
        // An old conversation it switched to by the list, uniquely titled: that one.
        rows.push(row("ses_OLD", "/u/p", "Old work", 1, 2));
        assert_eq!(pick_bound(&rows, cwd, "OC | Old work", 1, &ra, &claims_for(1)).unwrap().0.id, "ses_OLD");
    }

    #[test]
    fn an_opencode_that_ended_never_takes_a_new_conversation() {
        let cwd = "/u/p";
        let none = HashMap::new();
        // C started last and ended; A and B run on. B, then A, message (no titles yet).
        let mut c = oc(3, 9_000, "OpenCode");
        c.seen = 9_500;
        let known = [oc(1, 1_000, "OpenCode"), oc(2, 5_000, "OpenCode"), c];
        let rows = [row("ses_SB", cwd, "New session - 2026-11-07T05:19:41.297Z", 20_000, 20_500)];
        let get = |pid| pick(&rows, cwd, "OpenCode", pid, &known, &none).map(|(r, _)| r.id.clone());
        assert_eq!((get(1), get(2), get(3)), (None, Some("ses_SB".into()), None));
    }
}
