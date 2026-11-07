//! pi (`@earendil-works/pi-coding-agent`) saves a conversation, from its first
//! message, as `<agent dir>/sessions/--<folder>--/<made>_<id>.jsonl`, and keeps
//! no record of which process has which.
//!
//! Every pi herdr starts (bringing one back, `hc run pi`) loads a small
//! extension herdr keeps in its own state folder (`pi -e`), which notes the
//! session that pi is in, at every start, `/new`, `/resume` and `/fork`, in
//! `<state>/agents/pi/<pid>.json`: those are exact, however many share a
//! folder. Started with `--session-id <id>`, a pi is in that one (and makes it
//! if there is none), so one with no message yet comes back on a new id.
//!
//! A pi started by hand has no such record. pi's session ids are UUIDv7s whose
//! random part, in one process, counts up from a random start: the sessions
//! one process made lie together, those of others far apart. A pi makes its
//! first id as it starts, so the group made right after a pi started is that
//! pi's, and later ones of the group are too (`/new`, `/fork`). A group that
//! fits no one pi alone (two started within a second, one of them with no
//! message yet; one started outside herdr) is nobody's: a tab then comes back
//! to the last session it was sure of, else a new one, never another tab's.

use super::{Context, Foreground, Instance, Known, SEEN_MS, SLACK_MS, WROTE_MS, home, now_ms, utc_ms, words};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::UNIX_EPOCH;

/// A pi started this long after its terminal is the command that started it.
const LAUNCH_MS: u64 = 30_000;
/// pi makes the session it starts with within this long of starting.
const STARTUP_MS: u64 = 10_000;
/// pi makes its first id this soon after it starts when it isn't slow to start.
const QUICK_MS: u64 = 1_500;
/// Ids one process made are at most this far apart in their counted part.
const SAME_PROCESS: u64 = 1 << 20;

/// What pi was started with that matters here.
#[derive(Default, Debug, PartialEq)]
pub(super) struct Args {
    /// `--session-id`: the session it is in (how herdr resumes it).
    id: Option<String>,
    /// `--session <path|id>`: a session it opened, by path or (part of) its id.
    session: Option<String>,
    /// `--session-dir`, as given.
    dir: Option<String>,
    /// `--no-session`: nothing is saved.
    ephemeral: bool,
    /// Opened a saved session (`-c`, `-r`, `--session`), so it made none as it started.
    opened: bool,
}

/// What is known of a running pi.
#[derive(Default, Debug, Clone)]
struct Pi {
    /// The session it was started in (`--session-id`, `--session`).
    id: Option<String>,
    /// It opened one (`-c`, `-r`): it made none as it started.
    opened: bool,
    /// The id it starts again with if it has no message yet.
    fresh: Option<String>,
    /// The counted part of an id it made: its group of sessions.
    anchor: Option<u64>,
    /// When this core first saw it running.
    first_seen: u64,
    /// The session it was last found in.
    last: Option<String>,
}

static KNOWN: Mutex<Known<Pi>> = Mutex::new(Known::new());

/// Where herdr's pi extension and its records are, once `prepare`d.
static AGENTS: OnceLock<PathBuf> = OnceLock::new();

/// The extension: on every session start it notes, atomically, which session
/// this pi is in; it takes the note back as pi quits.
fn extension_source(records: &Path) -> String {
    let dir = serde_json::to_string(&records.to_string_lossy()).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"// herdr: notes which session this pi is in, so a terminal lost with it comes back to that one.
import {{ mkdirSync, renameSync, unlinkSync, writeFileSync }} from "node:fs";
const DIR = {dir};
const FILE = DIR + "/" + process.pid + ".json";
export default function (pi) {{
  pi.on("session_start", (_event, ctx) => {{
    try {{
      const sm = ctx.sessionManager;
      const rec = {{ pid: process.pid, sessionId: sm.getSessionId(), sessionFile: sm.getSessionFile() ?? null, cwd: ctx.cwd, at: Date.now() }};
      mkdirSync(DIR, {{ recursive: true }});
      writeFileSync(FILE + ".tmp", JSON.stringify(rec));
      renameSync(FILE + ".tmp", FILE);
    }} catch {{}}
  }});
  pi.on("session_shutdown", (event) => {{
    if (event?.reason !== "quit") return;
    try {{ unlinkSync(FILE); }} catch {{}}
  }});
}}
"#
    )
}

/// Writes herdr's pi extension into the core's state folder (`state`), once
/// at start, and clears the records of pis no longer running.
pub fn prepare(state: &Path) {
    let agents = state.join("agents");
    let records = agents.join("pi");
    if std::fs::create_dir_all(&records).is_err() {
        return;
    }
    let file = agents.join("pi-herdr.mjs");
    let source = extension_source(&records);
    let same = std::fs::read_to_string(&file).is_ok_and(|s| s == source);
    let aside = file.with_extension("mjs.new");
    if !same && (std::fs::write(&aside, &source).is_err() || std::fs::rename(&aside, &file).is_err()) {
        let _ = std::fs::remove_file(&aside);
        return;
    }
    for e in std::fs::read_dir(&records).into_iter().flatten().flatten() {
        let name = e.file_name();
        let pid = name.to_str().and_then(|n| n.strip_suffix(".json")).and_then(|p| p.parse::<i32>().ok());
        if pid.is_some_and(|pid| crate::platform::process_started_ms(pid).is_none()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    let _ = AGENTS.set(agents);
}

/// The extension's file, if it is there.
fn extension() -> Option<PathBuf> {
    let file = AGENTS.get()?.join("pi-herdr.mjs");
    let ok = file.is_file() && !file.to_string_lossy().chars().any(char::is_control);
    ok.then_some(file)
}

/// `argv` (a program to start), with herdr's extension when it is pi's app,
/// so herdr knows which session it is in from the start.
pub fn tracked(mut argv: Vec<String>) -> Vec<String> {
    let is_pi = argv.first().is_some_and(|w| super::base(w).1 == "pi");
    if is_pi
        && args(&argv).is_some_and(|a| !a.ephemeral)
        && let Some(ext) = extension()
    {
        argv.splice(1..1, ["-e".to_owned(), ext.display().to_string()]);
    }
    argv
}

/// The session pi `pid` (started at `start`) noted it is in, by herdr's extension.
fn noted(pid: i32, start: u64) -> Option<String> {
    let file = std::fs::read(AGENTS.get()?.join("pi").join(format!("{pid}.json"))).ok()?;
    noted_in(&file, pid, start)
}

/// The id in a record, when it is this process's (one an older process of
/// that pid left was written before this one started) and safe.
fn noted_in(file: &[u8], pid: i32, start: u64) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(file).ok()?;
    let at = v["at"].as_u64()?;
    let id = v["sessionId"].as_str()?;
    (v["pid"].as_i64() == Some(pid as i64) && at + SLACK_MS >= start && id_ok(id)).then(|| id.to_owned())
}

pub(super) fn command(f: &Foreground, t: &Context) -> Option<Vec<String>> {
    let start = crate::platform::process_started_ms(f.pid);
    let mut a = args(started_with(f, t, start))?;
    if a.ephemeral {
        return Some(words(&["pi"]));
    }
    let dir_flag = a.dir.clone().filter(|d| !d.chars().any(char::is_control));
    let ext = extension();
    // Every pi it starts again carries the extension: exact from then on.
    let cmd = |id: Option<&str>| {
        let mut cmd = words(&["pi"]);
        match id {
            Some(id) => cmd.extend(["--session-id".into(), id.to_owned()]),
            None => cmd.push("--continue".into()),
        }
        if let Some(e) = &ext {
            cmd.extend(["-e".into(), e.display().to_string()]);
        }
        if let Some(d) = &dir_flag {
            cmd.extend(["--session-dir".into(), d.clone()]);
        }
        cmd
    };
    let noted = start.and_then(|s| noted(f.pid, s));
    let found_dir = session_dir(&a, &f.cwd);
    if let (Some(given), Some((dir, _))) = (a.session.clone(), &found_dir) {
        a.id = a.id.take().or_else(|| session_by(&given, dir));
    }
    let given = a.id.clone().filter(|id| id_ok(id));
    let (Some(start), Some((dir, shared))) = (start, found_dir) else {
        return Some(cmd(noted.as_deref().or(given.as_deref())));
    };
    let folder = format!("{}\n{}", dir.display(), f.cwd);
    let now = now_ms();
    let found = if noted.is_some() { Vec::new() } else { sessions(&dir) };
    let mut all = KNOWN.lock().unwrap_or_else(|p| p.into_inner());
    let me = all.note(f.pid, start, &folder, now);
    if me.data.first_seen == 0 {
        me.data.first_seen = now;
    }
    // Its extension says: the others here know it as in that one, its group as its own.
    if let Some(id) = noted {
        if me.data.anchor.is_none() && v7_ms(&id).is_some_and(|made| made + SLACK_MS >= start) {
            me.data.anchor = v7_seq(&id);
        }
        me.data.id = Some(id.clone());
        return Some(cmd(Some(&id)));
    }
    me.data.id = given.clone();
    me.data.opened = a.opened && given.is_none();
    let fresh = me.data.fresh.get_or_insert_with(|| new_id(start)).clone();
    let owner = owners(&found, &mut all.0, &folder);
    let mut mine: Vec<&Session> =
        found.iter().zip(&owner).filter(|(_, o)| **o == Some(f.pid)).map(|(s, _)| s).collect();
    mine.sort_by_key(|s| std::cmp::Reverse(s.changed));
    // The one it wrote to last, if pi itself would find it by that id.
    let cwd = shared.then_some(f.cwd.as_str());
    let chosen = mine.iter().find(|s| id_ok(&s.id) && header_ok(&s.path, &s.id, cwd)).map(|s| s.id.clone());
    let others = all.folder(&folder).iter().any(|i| i.pid != f.pid && now.saturating_sub(i.seen) < SEEN_MS);
    let me = all.0.iter_mut().find(|i| i.pid == f.pid && i.folder == folder)?;
    if chosen.is_some() {
        me.data.last = chosen.clone();
    }
    let id = chosen
        // Nothing saved yet: the one it was started in, which pi makes if it must.
        .or(given)
        // None it is sure of now: the last it was.
        .or_else(|| me.data.last.clone());
    Some(match id {
        Some(id) => cmd(Some(&id)),
        None if !a.opened => cmd(Some(&fresh)),
        // It opened one it hasn't written to: the folder's last, unless that may be another's.
        None if others => cmd(Some(&fresh)),
        None => cmd(None),
    })
}

/// pi's command line. On Linux and macOS a running pi shows only `pi`: it
/// renames its process, over its arguments. One its terminal started just then
/// (`hc run pi …`, or herdr starting it again) is that command.
fn started_with<'a>(f: &'a Foreground, t: &Context<'a>, start: Option<u64>) -> &'a [String] {
    let renamed = matches!(&f.argv[..], [only] if only == "pi");
    let launched = t.argv.iter().any(|w| super::is_pi(w))
        && start.zip(t.started_ms).is_some_and(|(s, at)| s + super::SLACK_MS >= at && s <= at + LAUNCH_MS);
    if renamed && launched { t.argv } else { &f.argv }
}

/// pi's options, or None for a run that isn't the interactive app (a command
/// like `pi install`, `-p`, rpc mode).
pub(super) fn args(argv: &[String]) -> Option<Args> {
    // After the program: `pi ...`, or `node .../pi-coding-agent/.../cli.js ...`.
    let from = argv.iter().position(|w| super::is_pi(w)).map_or(argv.len().min(1), |i| i + 1);
    let mut a = Args::default();
    let mut it = argv[from..].iter();
    if it.as_slice().first().is_some_and(|w| {
        matches!(w.as_str(), "install" | "remove" | "uninstall" | "update" | "list" | "config" | "auth" | "mcp")
    }) {
        return None;
    }
    while let Some(w) = it.next() {
        let (k, v) = match w.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k, Some(v.to_owned())),
            _ => (w.as_str(), None),
        };
        let mut value = || v.clone().or_else(|| it.next().cloned());
        match k {
            "--" => break,
            "--session-id" => a.id = value(),
            "--session-dir" => a.dir = value(),
            "--no-session" => a.ephemeral = true,
            "-c" | "--continue" | "-r" | "--resume" => a.opened = true,
            "--session" => {
                a.opened = true;
                a.session = value();
            }
            // A new session, made from that one as it starts.
            "--fork" | "-e" | "--extension" => _ = value(),
            "--mode" => {
                if value().as_deref() != Some("text") {
                    return None;
                }
            }
            "-p" | "--print" | "--export" | "--list-models" | "-h" | "--help" | "-v" | "--version" => return None,
            _ => {}
        }
    }
    Some(a)
}

/// The session `--session` names: a file (`<made>_<id>.jsonl`), else the one
/// session in `dir` whose id starts so.
fn session_by(given: &str, dir: &Path) -> Option<String> {
    let name = given.rsplit(['/', '\\']).next().unwrap_or(given);
    if let Some((_, id)) = name_parts(name) {
        return Some(id.to_owned());
    }
    if !id_ok(given) {
        return None;
    }
    let all = sessions(dir);
    let mut found = all.iter().filter(|s| s.id.starts_with(given));
    match (found.next(), found.next()) {
        (Some(one), None) => Some(one.id.clone()),
        _ => None,
    }
}

/// pi's own folder: `PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
fn agent_dir() -> Option<PathBuf> {
    match std::env::var("PI_CODING_AGENT_DIR") {
        Ok(d) if !d.is_empty() => expand_tilde(&d),
        _ => Some(home()?.join(".pi").join("agent")),
    }
}

/// Where this pi keeps its sessions, and whether every project shares that
/// folder (one pi was told to use) rather than it being this project's own.
fn session_dir(a: &Args, cwd: &str) -> Option<(PathBuf, bool)> {
    let chosen = match a.dir.clone().or_else(|| std::env::var("PI_CODING_AGENT_SESSION_DIR").ok()) {
        Some(d) if !d.is_empty() => Some(d),
        _ => {
            let settings: Option<serde_json::Value> =
                std::fs::read(agent_dir()?.join("settings.json")).ok().and_then(|s| serde_json::from_slice(&s).ok());
            settings.and_then(|s| s["sessionDir"].as_str().filter(|d| !d.is_empty()).map(str::to_owned))
        }
    };
    Some(match chosen {
        Some(d) => (Path::new(cwd).join(expand_tilde(&d)?), true),
        None => (agent_dir()?.join("sessions").join(folder(cwd)), false),
    })
}

fn expand_tilde(path: &str) -> Option<PathBuf> {
    match path.strip_prefix('~') {
        Some("") => home(),
        Some(rest) if rest.starts_with(['/', '\\']) => Some(home()?.join(&rest[1..])),
        _ => Some(PathBuf::from(path)),
    }
}

/// pi's folder name for a project: `--`, the path without its first slash and
/// with `/`, `\` and `:` as `-`, then `--`.
fn folder(cwd: &str) -> String {
    let path = cwd.strip_prefix(['/', '\\']).unwrap_or(cwd);
    format!("--{}--", path.replace(['/', '\\', ':'], "-"))
}

/// A new UUIDv7, as pi makes its own: made at `ms`, then random.
fn new_id(ms: u64) -> String {
    let mut b = [0u8; 16];
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    crate::platform::random_bytes(&mut b[6..]);
    b[6] = 0x70 | (b[6] & 0x0f);
    b[8] = 0x80 | (b[8] & 0x3f);
    let hex: String = b.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// When one of pi's UUIDv7 ids was made (its first 48 bits, ms).
fn v7_ms(id: &str) -> Option<u64> {
    v7_seq(id)?;
    u64::from_str_radix(&id.replace('-', "")[..12], 16).ok()
}

/// The counted part of one of pi's UUIDv7 ids (`uuidv7` in pi-ai): 41 bits
/// after the time, one process's counting up by one per id from where it began.
fn v7_seq(id: &str) -> Option<u64> {
    if !super::is_uuid(id) {
        return None;
    }
    let hex: Vec<u8> = id.bytes().filter(|b| *b != b'-').collect();
    if hex[12] != b'7' || !matches!(hex[16].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b') {
        return None;
    }
    let byte = |i: usize| u64::from_str_radix(std::str::from_utf8(&hex[2 * i..2 * i + 2]).ok()?, 16).ok();
    Some(
        (byte(6)? & 0x0f) << 37
            | byte(7)? << 29
            | (byte(8)? & 0x3f) << 23
            | byte(9)? << 15
            | byte(10)? << 7
            | byte(11)? >> 1,
    )
}

#[derive(Debug, Clone)]
struct Session {
    path: PathBuf,
    id: String,
    /// When pi made it (from its name).
    made: u64,
    /// When it was last written to.
    changed: u64,
}

/// The sessions saved in `dir`, from their names and times alone: their
/// contents can be megabytes.
fn sessions(dir: &Path) -> Vec<Session> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let (made, id) = name_parts(name.to_str()?)?;
            let changed = e.metadata().ok()?.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
            Some(Session { path: e.path(), id: id.to_owned(), made, changed })
        })
        .collect()
}

/// `2026-11-01T16-05-21-427Z_<id>.jsonl`: when it was made (UTC ms) and its id.
fn name_parts(name: &str) -> Option<(u64, &str)> {
    let (at, id) = name.strip_suffix(".jsonl")?.split_once('_')?;
    if at.len() != 24 || !at.ends_with('Z') || !at.is_ascii() {
        return None;
    }
    let n = |r: std::ops::Range<usize>| at[r].parse::<u64>().ok();
    let ms = utc_ms(n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?)? + n(20..23)?;
    Some((ms, id))
}

/// The sessions one process made, as indexes into `sessions`, oldest first:
/// those whose counted parts lie together and count up as they were made.
/// One without such an id is a group of its own.
fn groups(sessions: &[Session], skip: &[bool]) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..sessions.len()).filter(|i| !skip[*i]).collect();
    order.sort_by_key(|i| (sessions[*i].made, sessions[*i].id.clone()));
    let mut groups: Vec<(Option<u64>, Vec<usize>)> = Vec::new();
    for i in order {
        let seq = v7_seq(&sessions[i].id);
        let joins = seq.and_then(|seq| {
            groups.iter().position(|(last, _)| last.is_some_and(|l| seq > l && seq - l <= SAME_PROCESS))
        });
        match joins {
            Some(g) => {
                groups[g].0 = seq;
                groups[g].1.push(i);
            }
            None => groups.push((seq, vec![i])),
        }
    }
    groups.into_iter().map(|(_, g)| g).collect()
}

/// Which pi each of `sessions` is (by pid), of the pis known in `folder`
/// (among `known`, whose groups it notes as it finds them):
/// 1. the one a pi was started in is that pi's;
/// 2. a group with an id of a pi's own group is that pi's;
/// 3. a group made as pis started goes to the pi that started just then and
///    has none (of several, the one that started a usual moment before, when
///    the others started much longer before); pis started together each get
///    one, in the order they started (pi takes about as long each time to make
///    its first), once each has one;
/// 4. a group made later (the first `/new` of a pi that made none as it
///    started) goes to the one pi running then without one, when it is alone so;
/// 5. an older one written to since a pi started, while no other ran, is that pi's.
///
/// Anything else is nobody's.
fn owners(sessions: &[Session], known: &mut [Instance<Pi>], folder: &str) -> Vec<Option<i32>> {
    let mut owner: Vec<Option<i32>> = vec![None; sessions.len()];
    let here: Vec<usize> = (0..known.len()).filter(|i| known[*i].folder == folder).collect();
    for (s, o) in sessions.iter().zip(owner.iter_mut()) {
        *o = here.iter().map(|i| &known[*i]).find(|i| i.data.id.as_deref() == Some(s.id.as_str())).map(|i| i.pid);
    }
    let pinned: Vec<bool> = owner.iter().map(Option::is_some).collect();
    let groups = groups(sessions, &pinned);
    let seqs = |g: &[usize]| g.iter().filter_map(|i| v7_seq(&sessions[*i].id)).collect::<Vec<u64>>();
    let mut group_owner: Vec<Option<usize>> = vec![None; groups.len()];
    // 2. Its own group.
    for (g, members) in groups.iter().enumerate() {
        let seqs = seqs(members);
        group_owner[g] = here
            .iter()
            .copied()
            .find(|i| known[*i].data.anchor.is_some_and(|a| seqs.iter().any(|s| s.abs_diff(a) <= SAME_PROCESS)));
    }
    // 3. Made as a pi started.
    let made = |g: usize| sessions[groups[g][0]].made;
    let taken = |group_owner: &[Option<usize>], i: usize| group_owner.contains(&Some(i));
    let starter = |i: &Instance<Pi>| i.data.id.is_none() && !i.data.opened && i.data.anchor.is_none();
    let fits = |i: &Instance<Pi>, at: u64| starter(i) && i.start <= at && at <= i.start + STARTUP_MS && i.live_at(at);
    loop {
        let mut changed = false;
        for g in 0..groups.len() {
            if group_owner[g].is_some() {
                continue;
            }
            let at = made(g);
            let can: Vec<usize> =
                here.iter().copied().filter(|i| !taken(&group_owner, *i) && fits(&known[*i], at)).collect();
            // One alone fits, or one made it as soon as pi does and the others would have been slow.
            let mut quick: Vec<(u64, usize)> = can.iter().map(|i| (at - known[*i].start, *i)).collect();
            quick.sort_unstable();
            let one = match quick[..] {
                [(_, one)] => Some(one),
                [(first, one), (second, _), ..] if first <= QUICK_MS && second >= first + QUICK_MS => Some(one),
                _ => None,
            };
            if let Some(one) = one {
                group_owner[g] = Some(one);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // Pis started together: one group each, in the order they started.
    let open: Vec<usize> = (0..groups.len())
        .filter(|g| group_owner[*g].is_none())
        .filter(|g| here.iter().any(|i| !taken(&group_owner, *i) && fits(&known[*i], made(*g))))
        .collect();
    let mut can: Vec<usize> = here
        .iter()
        .copied()
        .filter(|i| !taken(&group_owner, *i) && open.iter().any(|g| fits(&known[*i], made(*g))))
        .collect();
    can.sort_by_key(|i| known[*i].start);
    if !open.is_empty() && open.len() == can.len() && open.iter().zip(&can).all(|(g, i)| fits(&known[*i], made(*g))) {
        for (g, i) in open.iter().zip(&can) {
            group_owner[*g] = Some(*i);
        }
    }
    // 4. Made later, by the one pi then without a group (seen since before then).
    for g in 0..groups.len() {
        if group_owner[g].is_some() {
            continue;
        }
        let at = made(g);
        let live: Vec<usize> = here
            .iter()
            .copied()
            .filter(|i| known[*i].live_at(at) && known[*i].data.first_seen <= at + WROTE_MS)
            .collect();
        let without: Vec<usize> = live.iter().copied().filter(|i| !taken(&group_owner, *i)).collect();
        let watched = here.iter().all(|i| known[*i].live_at(at) == live.contains(i));
        if let ([one], true) = (&without[..], watched) {
            group_owner[g] = Some(*one);
        }
    }
    for (g, members) in groups.iter().enumerate() {
        let Some(i) = group_owner[g] else { continue };
        if known[i].data.anchor.is_none() {
            known[i].data.anchor = seqs(members).first().copied();
        }
        for m in members {
            owner[*m] = Some(known[i].pid);
        }
    }
    // 5. An older one it alone wrote to since it started (`/resume`).
    for (s, o) in sessions.iter().zip(owner.iter_mut()) {
        if o.is_some() {
            continue;
        }
        let writers: Vec<&Instance<Pi>> = here.iter().map(|i| &known[*i]).filter(|i| i.live_at(s.changed)).collect();
        if let [one] = writers[..]
            && s.made < one.start
        {
            *o = Some(one.pid);
        }
    }
    owner
}

/// A session id that is safe to start pi with (letters, digits, `-`, `_`, `.`,
/// starting and ending with a letter or digit, as pi requires).
fn id_ok(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

/// Whether the file starts with the session header pi looks sessions up by
/// (and, in a folder every project shares, is this project's): without it,
/// `--session-id` would start an empty session instead. Only its first line
/// is read; one found good in a project's own folder stays good.
fn header_ok(path: &Path, id: &str, cwd: Option<&str>) -> bool {
    static GOOD: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);
    if cwd.is_none() && GOOD.lock().unwrap_or_else(|p| p.into_inner()).contains(path) {
        return true;
    }
    let ok = first_line(path).is_some_and(|line| header_says(&line, id, cwd));
    if ok && cwd.is_none() {
        let mut good = GOOD.lock().unwrap_or_else(|p| p.into_inner());
        if good.len() >= 512 {
            good.clear();
        }
        good.insert(path.to_owned());
    }
    ok
}

fn first_line(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut head = Vec::with_capacity(4096);
    std::fs::File::open(path).ok()?.take(4096).read_to_end(&mut head).ok()?;
    let end = head.iter().position(|b| *b == b'\n').unwrap_or(head.len());
    head.truncate(end);
    Some(head)
}

fn header_says(line: &[u8], id: &str, cwd: Option<&str>) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { return false };
    v["type"] == "session" && v["id"] == id && cwd.is_none_or(|c| v["cwd"].as_str() == Some(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| (*s).to_owned()).collect()
    }

    const CLI: &str = r"C:\Users\x\nodejs/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js";

    #[test]
    fn reads_what_pi_was_started_with() {
        assert_eq!(args(&argv(&[r"C:\nodejs\node.exe", CLI])), Some(Args::default()));
        let a = args(&argv(&["node", CLI, "--session-id", "abc", "--session-dir=~/s", "fix it"])).unwrap();
        assert_eq!((a.id.as_deref(), a.dir.as_deref(), a.opened), (Some("abc"), Some("~/s"), false));
        assert!(args(&argv(&["pi", "-c"])).unwrap().opened);
        let opened = args(&argv(&["pi", "--session", "01a1", "--model", "x"])).unwrap();
        assert_eq!((opened.opened, opened.session.as_deref()), (true, Some("01a1")));
        assert!(args(&argv(&["pi", "--no-session"])).unwrap().ephemeral);
        assert_eq!(args(&argv(&["pi", "--", "--no-session"])), Some(Args::default()), "a message, not an option");
        // herdr's extension and a fork's source are values, not messages.
        assert_eq!(args(&argv(&["pi", "-e", "x.mjs", "--fork", "01a1"])), Some(Args::default()));
        // Not the interactive app: nothing to resume.
        assert_eq!(args(&argv(&["pi", "install", "npm:x"])), None);
        assert_eq!(args(&argv(&["pi", "-p", "hi"])), None);
        assert_eq!(args(&argv(&["pi", "--mode", "rpc"])), None);
        assert!(args(&argv(&["pi", "--mode=text"])).is_some());
    }

    #[test]
    fn a_renamed_pi_is_the_command_its_terminal_started() {
        // Linux shows a running pi as just `pi`.
        let f = Foreground { pid: 1, name: "pi".into(), argv: argv(&["pi"]), cwd: "/p".into() };
        let restored = argv(&["/bin/bash", "-lic", "\"$@\"; exec \"$0\" -li", "/bin/bash", "pi", "--session-id", "x1"]);
        let t = Context { title: "", argv: &restored, started_ms: Some(100_000) };
        assert_eq!(args(started_with(&f, &t, Some(100_400))).unwrap().id.as_deref(), Some("x1"));
        assert_eq!(args(started_with(&f, &t, Some(100_000 + LAUNCH_MS + 1))), Some(Args::default()), "started later");
        let worker = argv(&["pi", "--mode", "rpc"]);
        assert_eq!(args(started_with(&f, &Context { argv: &worker, ..t }, Some(100_050))), None, "not the app");
        // Where the OS shows its options, they are what count.
        let shown = Foreground { argv: argv(&["node", CLI]), ..f };
        assert_eq!(args(started_with(&shown, &t, Some(100_400))), Some(Args::default()));
    }

    #[test]
    fn names_folders_and_sessions_as_pi_does() {
        assert_eq!(folder(r"C:\llama"), "--C--llama--");
        assert_eq!(folder(r"C:\Program Files (x86)\Steam\steamapps"), "--C--Program Files (x86)-Steam-steamapps--");
        assert_eq!(folder(r"D:\PL\llama.cpp"), "--D--PL-llama.cpp--");
        assert_eq!(folder("/home/u/p"), "--home-u-p--");
        let (made, id) = name_parts("2026-11-01T16-05-21-427Z_01a197db-b793-7429-9441-784cd63d9317.jsonl").unwrap();
        assert_eq!(id, "01a197db-b793-7429-9441-784cd63d9317");
        // The id is a UUIDv7: its first 48 bits are the same time.
        assert_eq!(made, 0x01a197dbb793);
        assert_eq!(name_parts("notes.jsonl"), None);
        assert_eq!(name_parts("2026-11-01T16-05-21-427Z_x.json"), None);
        let new = new_id(0x01a197dbb793);
        assert!(new.starts_with("01a197db-b793-7") && id_ok(&new) && new.len() == 36, "{new}");
        assert_ne!(new, new_id(0x01a197dbb793));
    }

    #[test]
    fn only_safe_ids_are_used() {
        assert!(id_ok("01a197db-b793-7429-9441-784cd63d9317"));
        assert!(id_ok("my.session_1"));
        for bad in ["", "x; rm -rf /", "-a", "a-", "a b", "$(x)", &"a".repeat(65)] {
            assert!(!id_ok(bad), "{bad}");
        }
    }

    #[test]
    fn ids_one_process_made_count_up_together() {
        // One pi's /new, twice (seen on a real machine): one apart.
        let a = v7_seq("01a1b4bd-87c0-76c7-87d0-3b782897a872").unwrap();
        let b = v7_seq("01a1b4bd-8bf0-76c7-87d0-3b7ad2e26a4e").unwrap();
        assert_eq!(b - a, 1);
        assert_eq!(v7_seq("01a1b4bd-87c0-46c7-87d0-3b782897a872"), None, "not a v7");
        assert_eq!(v7_seq("herdr-pinned-1"), None);
    }

    #[test]
    fn herdrs_extension_notes_each_pis_session() {
        let rec = br#"{"pid":7,"sessionId":"01a1b4bd-87c0-76c7-87d0-3b782897a872","at":100500}"#;
        assert_eq!(noted_in(rec, 7, 100_000).as_deref(), Some("01a1b4bd-87c0-76c7-87d0-3b782897a872"));
        assert_eq!(noted_in(rec, 8, 100_000), None, "another process's");
        assert_eq!(noted_in(rec, 7, 100_500 + SLACK_MS + 1), None, "left by an older process of that pid");
        assert_eq!(noted_in(br#"{"pid":7,"sessionId":"x; rm -rf /","at":100500}"#, 7, 100_000), None);
        let source = extension_source(Path::new(r"C:\herdr state\agents\pi"));
        assert!(source.contains(r#"const DIR = "C:\\herdr state\\agents\\pi";"#), "{source}");
        assert!(source.contains("session_start") && source.contains("getSessionId()"));
    }

    // One pi process's ids: `seed`, counted up `n` times, made at `ms`.
    fn v7(ms: u64, seed: u64, n: u64) -> String {
        let seq = seed + n;
        let mut b = [0u8; 16];
        b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
        b[6] = 0x70 | ((seq >> 37) & 0x0f) as u8;
        b[7] = (seq >> 29) as u8;
        b[8] = 0x80 | ((seq >> 23) & 0x3f) as u8;
        b[9] = (seq >> 15) as u8;
        b[10] = (seq >> 7) as u8;
        b[11] = ((seq << 1) & 0xfe) as u8;
        let hex: String = b.iter().map(|b| format!("{b:02x}")).collect();
        format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
    }

    /// Pis in one folder, as tabs, and the sessions on disk.
    struct Folder {
        known: Vec<Instance<Pi>>,
        sessions: Vec<Session>,
    }

    impl Folder {
        fn new() -> Self {
            Folder { known: Vec::new(), sessions: Vec::new() }
        }

        fn start(&mut self, pid: i32, at: u64, id: Option<&str>) {
            let data = Pi { id: id.map(str::to_owned), first_seen: at, ..Pi::default() };
            self.known.push(Instance { pid, start: at, folder: "f".into(), seen: at, data });
        }

        fn at(&mut self, now: u64, ended: &[i32]) {
            for i in &mut self.known {
                if !ended.contains(&i.pid) {
                    i.seen = now;
                }
            }
        }

        fn save(&mut self, id: &str, made: u64, changed: u64) {
            self.sessions.retain(|s| s.id != id);
            self.sessions.push(Session { path: PathBuf::new(), id: id.into(), made, changed });
        }

        /// Each pi's newest own session.
        fn resume(&mut self, pids: &[i32]) -> Vec<Option<String>> {
            let owner = owners(&self.sessions, &mut self.known, "f");
            pids.iter()
                .map(|p| {
                    let mine = self.sessions.iter().zip(&owner).filter(|(_, o)| **o == Some(*p));
                    mine.max_by_key(|(s, _)| s.changed).map(|(s, _)| s.id.clone())
                })
                .collect()
        }
    }

    const A: u64 = 0x10_0000_0000;
    const B: u64 = 0x80_0000_0000;
    const C: u64 = 0x04_0000_0000;

    fn some(ids: &[&str]) -> Vec<Option<String>> {
        ids.iter().map(|i| (!i.is_empty()).then(|| (*i).to_owned())).collect()
    }

    #[test]
    fn tabs_each_keep_theirs_through_new_and_exits() {
        let mut f = Folder::new();
        // A starts at 100 s and chats; B starts at 160 s and chats.
        f.start(1, 100_000, None);
        let a1 = v7(100_250, A, 0);
        f.save(&a1, 100_250, 110_000);
        f.start(2, 160_000, None);
        let b1 = v7(160_250, B, 0);
        f.at(170_000, &[]);
        f.save(&b1, 160_250, 170_000);
        assert_eq!(f.resume(&[1, 2]), some(&[&a1, &b1]));
        // A runs /new at 220 s: the new one is A's, though B started later.
        let a2 = v7(220_000, A, 1);
        f.at(225_000, &[]);
        f.save(&a2, 220_000, 225_000);
        assert_eq!(f.resume(&[1, 2]), some(&[&a2, &b1]));
        // B /forks, then quits; A runs /new again.
        let b2 = v7(230_000, B, 1);
        f.at(231_000, &[]);
        f.save(&b2, 230_000, 231_000);
        f.at(300_000, &[2]);
        let a3 = v7(310_000, A, 2);
        f.save(&a3, 310_000, 311_000);
        f.at(311_000, &[2]);
        assert_eq!(f.resume(&[1, 2]), some(&[&a3, &b2]), "an ended pi's are still its own");
        // A third starts and chats.
        f.start(3, 320_000, None);
        let c1 = v7(320_300, C, 0);
        f.save(&c1, 320_300, 321_000);
        f.at(321_000, &[2]);
        assert_eq!(f.resume(&[1, 2, 3]), some(&[&a3, &b2, &c1]));
    }

    #[test]
    fn tabs_started_together_each_get_theirs_once_both_chat() {
        let mut f = Folder::new();
        // Two started 100 ms apart: each makes its first id about 250 ms after it starts.
        f.start(1, 100_000, None);
        f.start(2, 100_100, None);
        let (a1, b1) = (v7(100_250, A, 0), v7(100_350, B, 0));
        // Only B chatted yet: either could have made it, so it is nobody's.
        f.save(&b1, 100_350, 130_000);
        f.at(130_000, &[]);
        assert_eq!(f.resume(&[1, 2]), some(&["", ""]));
        // A chats too: one each, in the order they started.
        f.save(&a1, 100_250, 140_000);
        f.at(140_000, &[]);
        assert_eq!(f.resume(&[1, 2]), some(&[&a1, &b1]));
        // Then known for good: B's /new is B's.
        let b2 = v7(150_000, B, 1);
        f.save(&b2, 150_000, 151_000);
        f.at(151_000, &[]);
        assert_eq!(f.resume(&[1, 2]), some(&[&a1, &b2]));
    }

    #[test]
    fn an_idle_tab_never_takes_a_later_ones_session() {
        let mut f = Folder::new();
        // A starts and says nothing; B starts 3 s later and chats.
        f.start(1, 100_000, None);
        f.start(2, 103_000, None);
        let b1 = v7(103_250, B, 0);
        f.save(&b1, 103_250, 110_000);
        f.at(110_000, &[]);
        assert_eq!(f.resume(&[1, 2]), some(&["", &b1]));
        // A new group later: A's /new (it had made none it saved), never B's, which has its own.
        let a2 = v7(200_250, A, 1);
        f.save(&a2, 200_250, 201_000);
        f.at(201_000, &[]);
        assert_eq!(f.resume(&[1, 2]), some(&[&a2, &b1]));
        // Started 200 ms apart, one chatting: either may have made it, so nobody's yet.
        let mut g = Folder::new();
        g.start(1, 100_000, None);
        g.start(2, 100_200, None);
        let x = v7(100_450, C, 0);
        g.save(&x, 100_450, 101_000);
        g.at(101_000, &[]);
        assert_eq!(g.resume(&[1, 2]), some(&["", ""]));
    }

    #[test]
    fn sessions_of_a_pi_that_ended_before_the_core_started_stay_its() {
        let mut f = Folder::new();
        // The core starts at 150 s: B (started at 60 s, gone at 100 s) is not known.
        f.start(1, 0, None);
        f.known[0].data.first_seen = 150_000;
        let (a1, b1) = (v7(250, A, 0), v7(60_250, B, 0));
        f.save(&a1, 250, 50_000);
        f.save(&b1, 60_250, 99_000);
        f.at(150_000, &[]);
        assert_eq!(f.resume(&[1]), some(&[&a1]));
    }

    #[test]
    fn tabs_brought_back_keep_their_sessions_and_a_later_new_is_nobodys_when_unsure() {
        let mut f = Folder::new();
        // herdr brings two back in one ms, each in its own session.
        let (a1, b1) = (v7(1_000, A, 5), v7(2_000, B, 9));
        f.save(&a1, 1_000, 2_000);
        f.save(&b1, 2_000, 3_000);
        f.start(1, 100_000, Some(&a1));
        f.start(2, 100_001, Some(&b1));
        f.at(110_000, &[]);
        assert_eq!(f.resume(&[1, 2]), some(&[&a1, &b1]));
        // A runs /new: either could have (neither made an id as it started). Neither takes it.
        let a2 = v7(120_000, 0x20_0000_0000, 0);
        f.save(&a2, 120_000, 121_000);
        f.at(121_000, &[]);
        assert_eq!(f.resume(&[1, 2]), some(&[&a1, &b1]));
        // With only A running, its /new is its own.
        let mut g = Folder::new();
        g.save(&a1, 1_000, 2_000);
        g.start(1, 100_000, Some(&a1));
        g.save(&a2, 120_000, 121_000);
        g.at(121_000, &[]);
        assert_eq!(g.resume(&[1]), some(&[&a2]));
    }

    #[test]
    fn an_old_session_is_a_tabs_only_if_it_alone_wrote_it() {
        let mut f = Folder::new();
        let old = v7(10_000, C, 0);
        f.start(1, 100_000, None);
        let a1 = v7(100_250, A, 0);
        f.save(&a1, 100_250, 101_000);
        // Alone, it /resumes an old one and writes to it.
        f.save(&old, 10_000, 105_000);
        f.at(105_000, &[]);
        assert_eq!(f.resume(&[1]), some(&[&old]));
        // With B running too, either may have: A keeps its own.
        f.start(2, 106_000, None);
        f.save(&old, 10_000, 107_000);
        f.at(107_000, &[]);
        assert_eq!(f.resume(&[1, 2]), some(&[&a1, ""]));
    }

    #[test]
    fn resumes_its_own_saved_session_from_disk() {
        let dir = std::env::temp_dir().join(format!("herdr-pi-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = "01a197db-b793-7429-9441-784cd63d9317";
        let name = format!("2026-11-01T16-05-21-427Z_{good}.jsonl");
        let header = format!(r#"{{"type":"session","version":3,"id":"{good}","timestamp":"x","cwd":"C:\\llama"}}"#);
        std::fs::write(dir.join(&name), format!("{header}\n{{\"type\":\"message\"}}\n")).unwrap();
        // No header: pi wouldn't find it by id.
        let bare = "2026-11-02T16-46-32-221Z_01a19d27-0000-7000-8000-000000000000.jsonl";
        std::fs::write(dir.join(bare), "{\"type\":\"message\"}\n").unwrap();
        let found = sessions(&dir);
        assert_eq!(found.len(), 2);
        let s = found.iter().find(|s| s.id == good).unwrap();
        assert!(header_ok(&s.path, good, None));
        assert!(header_ok(&s.path, good, Some(r"C:\llama")));
        assert!(!header_says(header.as_bytes(), good, Some(r"C:\other")), "another project's, in a shared folder");
        let b = found.iter().find(|s| s.id != good).unwrap();
        assert!(!header_ok(&b.path, &b.id, None));
        // `--session` by its file or a unique start of its id.
        assert_eq!(session_by(&dir.join(&name).display().to_string(), &dir).as_deref(), Some(good));
        assert_eq!(session_by("01a197db", &dir).as_deref(), Some(good));
        assert_eq!(session_by("01a19", &dir), None, "two start so");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_running_pi_comes_back_to_its_session_or_starts_on_a_new_one() {
        // This test's own process stands in for pi, its sessions in a folder of its own.
        let dir = std::env::temp_dir().join(format!("herdr-pi-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.display().to_string();
        let f = Foreground {
            pid: std::process::id() as i32,
            name: "pi".into(),
            argv: argv(&["pi", "--session-dir", &d]),
            cwd: r"C:\proj".into(),
        };
        let t = Context::default();
        // No message yet: a new session, by an id of its own, the same each time.
        let fresh = command(&f, &t).unwrap();
        assert_eq!((&fresh[..2], &fresh[3..]), (&argv(&["pi", "--session-id"])[..], &argv(&["--session-dir", &d])[..]));
        assert!(id_ok(&fresh[2]));
        assert_eq!(command(&f, &t).unwrap(), fresh);
        // Made as it started (a time to come stands in for now).
        let start = crate::platform::process_started_ms(f.pid).unwrap();
        let id = v7(start + 250, A, 0);
        let header = format!(r#"{{"type":"session","version":3,"id":"{id}","timestamp":"x","cwd":"C:\\proj"}}"#);
        let made = chrono_name(start + 250);
        std::fs::write(dir.join(format!("{made}_{id}.jsonl")), format!("{header}\n")).unwrap();
        assert_eq!(command(&f, &t).unwrap(), ["pi", "--session-id", &id, "--session-dir", &d]);
        // Opened one (`-c`) and wrote nothing since: the folder's last, in its own folder.
        let c = Foreground { argv: argv(&["pi", "-c", "--session-dir", &d]), cwd: r"C:\other".into(), ..f };
        assert_eq!(command(&c, &t).unwrap(), ["pi", "--continue", "--session-dir", &d]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// pi's file name time for `ms`: `2026-11-01T16-05-21-427Z`.
    fn chrono_name(ms: u64) -> String {
        let (days, rest) = (ms / 86_400_000, ms % 86_400_000);
        // Civil date from days (Howard Hinnant's algorithm).
        let z = days + 719_468;
        let (era, doe) = (z / 146_097, z % 146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + u64::from(m <= 2);
        let (h, mi, s, milli) = (rest / 3_600_000, rest / 60_000 % 60, rest / 1000 % 60, rest % 1000);
        format!("{y:04}-{m:02}-{d:02}T{h:02}-{mi:02}-{s:02}-{milli:03}Z")
    }

    #[test]
    fn file_names_carry_the_time() {
        let name = format!("{}_x.jsonl", chrono_name(1_794_028_781_297));
        assert_eq!(name_parts(&name).map(|(ms, _)| ms), Some(1_794_028_781_297));
    }
}
