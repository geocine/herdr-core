//! What runs in a terminal right now: its foreground process and that
//! process's folder. Raw facts, read about twice a second; clients name and
//! draw programs themselves.

use crate::runtime::Core;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

#[derive(Serialize, Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct Foreground {
    /// The foreground process group's leader.
    pub pid: i32,
    /// Its name as the OS has it (`node`, `vim`, `sh`).
    pub name: String,
    pub argv: Vec<String>,
    /// Its working directory now (resolved by the OS).
    pub cwd: String,
}

/// The facts about `pid`, if it is still there to read.
#[cfg(target_os = "linux")]
pub fn about(pid: i32) -> Option<Foreground> {
    let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?.trim_end().to_string();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv =
        cmdline.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?.display().to_string();
    Some(Foreground { pid, name, argv, cwd })
}

/// A pseudo console has no foreground group: this is the terminal's first
/// process, with the folder and command line Windows keeps in its memory.
/// None for a 32-bit process, whose own copy of them lives elsewhere.
#[cfg(windows)]
pub fn about(pid: i32) -> Option<Foreground> {
    use std::ffi::c_void;
    use std::mem::{MaybeUninit, size_of};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
    use windows_sys::Win32::Foundation::{HANDLE, LocalFree, UNICODE_STRING};
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid as u32) };
    if raw.is_null() {
        return None;
    }
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let h = process.as_raw_handle() as HANDLE;
    let mut wow64 = 0;
    if unsafe { IsWow64Process(h, &mut wow64) } == 0 || wow64 != 0 {
        return None;
    }
    let read = |at: *const c_void, into: *mut c_void, len: usize| {
        let mut n = 0;
        unsafe { ReadProcessMemory(h, at, into, len, &mut n) != 0 && n == len }
    };
    fn value<T>(read: impl Fn(*const c_void, *mut c_void, usize) -> bool, at: *const c_void) -> Option<T> {
        let mut v = MaybeUninit::<T>::uninit();
        read(at, v.as_mut_ptr().cast(), size_of::<T>()).then(|| unsafe { v.assume_init() })
    }
    let text = |s: UNICODE_STRING| -> Option<Vec<u16>> {
        let mut buf = vec![0u16; s.Length as usize / 2];
        read(s.Buffer.cast(), buf.as_mut_ptr().cast(), buf.len() * 2).then_some(buf)
    };

    let mut basic: PROCESS_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let size = size_of::<PROCESS_BASIC_INFORMATION>() as u32;
    let status =
        unsafe { NtQueryInformationProcess(h, ProcessBasicInformation, (&raw mut basic).cast(), size, &mut 0) };
    if status != 0 || basic.PebBaseAddress.is_null() {
        return None;
    }
    let peb: PEB = value(read, basic.PebBaseAddress.cast())?;
    let params: RTL_USER_PROCESS_PARAMETERS = value(read, peb.ProcessParameters.cast())?;
    // CurrentDirectory.DosPath: right after the standard handles, in what the SDK calls Reserved2.
    let dir: UNICODE_STRING = value(read, peb.ProcessParameters.cast::<u8>().wrapping_add(0x38).cast())?;
    let mut cwd = String::from_utf16(&text(dir)?).ok()?;
    if cwd.len() > 3 && cwd.ends_with('\\') {
        cwd.pop(); // Windows keeps C:\dir\; only a drive's root has one
    }
    let image = String::from_utf16(&text(params.ImagePathName)?).ok()?;
    let name = image.rsplit('\\').next().unwrap_or(&image).to_string();

    let mut line = text(params.CommandLine)?;
    line.push(0);
    let mut argc = 0;
    let list = unsafe { CommandLineToArgvW(line.as_ptr(), &mut argc) };
    if list.is_null() {
        return None;
    }
    let argv = (0..argc as usize)
        .map(|i| unsafe {
            let arg = *list.add(i);
            String::from_utf16_lossy(std::slice::from_raw_parts(arg, (0..).take_while(|&n| *arg.add(n) != 0).count()))
        })
        .collect();
    unsafe { LocalFree(list.cast()) };
    Some(Foreground { pid, name, argv, cwd })
}

#[cfg(target_os = "macos")]
pub fn about(pid: i32) -> Option<Foreground> {
    let mut name = [0u8; 256];
    let n = unsafe { libc::proc_name(pid, name.as_mut_ptr().cast(), name.len() as u32) };
    let name = String::from_utf8_lossy(&name[..n.max(0) as usize]).into_owned();
    let mut vnode: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    if unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, (&raw mut vnode).cast(), size) } != size {
        return None;
    }
    let path = unsafe { std::ffi::CStr::from_ptr(vnode.pvi_cdir.vip_path.as_ptr().cast()) };
    let (argv, _) = procargs(pid)?;
    Some(Foreground { pid, name, argv, cwd: path.to_string_lossy().into_owned() })
}

/// KERN_PROCARGS2: argc, the executable path, the arguments, then the
/// environment, each NUL-terminated. Returns (arguments, environment).
#[cfg(target_os = "macos")]
pub fn procargs(pid: i32) -> Option<(Vec<String>, Vec<u8>)> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    let call = |buf: *mut libc::c_void, size: &mut libc::size_t, mib: &mut [i32; 3]| unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, buf, size, std::ptr::null_mut(), 0)
    };
    if call(std::ptr::null_mut(), &mut size, &mut mib) != 0 || size < 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    if call(buf.as_mut_ptr().cast(), &mut size, &mut mib) != 0 {
        return None;
    }
    buf.truncate(size);
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let mut rest = &buf[4..];
    let one = |r: &[u8]| -> Option<usize> { r.iter().position(|b| *b == 0) };
    rest = &rest[one(rest)? + 1..]; // the executable path
    while rest.first() == Some(&0) {
        rest = &rest[1..]; // its padding
    }
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        let end = one(rest)?;
        argv.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        rest = &rest[end + 1..];
    }
    Some((argv, rest.to_vec()))
}

impl Core {
    /// Keeps every live terminal's `foreground` current, with an event on each change.
    pub async fn watch_foreground(self: Arc<Self>) {
        let ms = std::env::var("HC_FOREGROUND_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(500);
        loop {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            let live: Vec<u32> = self.lock().reg.terminals.values().filter(|t| t.is_live()).map(|t| t.id()).collect();
            for id in live {
                let pgid = self.foreground(id).await;
                let mut now = (pgid > 0).then(|| about(pgid)).flatten();
                // Read before the lock: it may read an agent's session file.
                let resume = now.as_ref().and_then(crate::resume::command);
                let mut st = self.lock();
                #[cfg(windows)]
                if let (Some(f), Some(t)) = (now.as_mut(), st.reg.terminals.get(&id))
                    && f.name.eq_ignore_ascii_case("bash.exe")
                    && let Some(cwd) = &t.last_cwd
                {
                    f.cwd = cwd.clone();
                }
                // The agent to resume if this terminal is lost, kept current as its session
                // changes. A gone agent is forgotten only after a while: a shutdown ends agents
                // before the core, and Windows doesn't always say it is shutting down by then.
                if let Some(t) = st.reg.terminals.get_mut(&id).filter(|t| t.is_live()) {
                    let now_ms = crate::runtime::now_ms();
                    if track_resume(
                        &mut t.resume,
                        &mut t.resume_gone_ms,
                        resume,
                        now_ms,
                        crate::platform::host_shutdown_in_progress(),
                    ) {
                        self.save_soon();
                    }
                }
                let Some(t) = st.reg.terminals.get_mut(&id).filter(|t| t.is_live() && t.foreground != now) else {
                    continue;
                };
                if let Some(f) = &now {
                    t.last_cwd = Some(f.cwd.clone());
                }
                t.foreground = now.clone();
                self.save_soon();
                self.emit(&mut st, "foreground", id, serde_json::to_value(&now).unwrap_or_default());
            }
        }
    }
}

/// How long an agent that ended is still resumed if its terminal is lost.
const RESUME_GRACE_MS: u64 = 60_000;

/// Keeps `kept` (the agent to resume) current with `now` (the agent in front,
/// if any). An agent that is gone stays for `RESUME_GRACE_MS` (for good while
/// the host shuts down), so one a shutdown or reboot ends before the core
/// still comes back. Whether `kept` changed (to save).
fn track_resume(
    kept: &mut Option<Vec<String>>,
    gone_since: &mut Option<u64>,
    now: Option<Vec<String>>,
    now_ms: u64,
    shutting_down: bool,
) -> bool {
    if now.is_some() {
        *gone_since = None;
        if *kept == now {
            return false;
        }
        *kept = now;
        return true;
    }
    if kept.is_none() || shutting_down {
        return false;
    }
    let since = *gone_since.get_or_insert(now_ms);
    if now_ms.saturating_sub(since) < RESUME_GRACE_MS {
        return false;
    }
    *kept = None;
    *gone_since = None;
    true
}

#[cfg(test)]
mod resume_tests {
    use super::*;

    fn claude() -> Option<Vec<String>> {
        Some(vec!["claude".into(), "--resume".into(), "abc".into()])
    }

    #[test]
    fn an_agent_that_ends_is_still_resumed_for_a_while() {
        let (mut kept, mut since) = (None, None);
        assert!(track_resume(&mut kept, &mut since, claude(), 0, false));
        assert!(!track_resume(&mut kept, &mut since, claude(), 500, false));
        // Ended (a reboot ending it before the core, unannounced): still kept.
        assert!(!track_resume(&mut kept, &mut since, None, 1_000, false));
        assert!(!track_resume(&mut kept, &mut since, None, 1_000 + RESUME_GRACE_MS - 1, false));
        assert_eq!(kept, claude());
        // Gone for good after the grace.
        assert!(track_resume(&mut kept, &mut since, None, 1_000 + RESUME_GRACE_MS, false));
        assert_eq!(kept, None);
    }

    #[test]
    fn a_shutdown_never_forgets_and_a_new_agent_replaces_at_once() {
        let (mut kept, mut since) = (claude(), None);
        assert!(!track_resume(&mut kept, &mut since, None, 0, true));
        assert!(!track_resume(&mut kept, &mut since, None, 10 * RESUME_GRACE_MS, true));
        assert_eq!(kept, claude());
        let codex = Some(vec!["codex".into(), "resume".into(), "--last".into()]);
        assert!(track_resume(&mut kept, &mut since, codex.clone(), 10 * RESUME_GRACE_MS, false));
        assert_eq!((kept, since), (codex, None));
    }
}
