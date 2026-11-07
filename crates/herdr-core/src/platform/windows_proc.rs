//! Windows: processes. The program under a terminal's shell, when a process
//! started and where its program is, the files it has open, and the system's
//! SQLite for reading other programs' data.

use std::ffi::{CStr, c_void};
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr::null_mut;
use std::sync::{Arc, OnceLock};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Threading::*;

/// When process `pid` started, in ms since 1970; None if it is gone.
pub fn process_started_ms(pid: i32) -> Option<u64> {
    unsafe {
        let process = open(pid)?;
        let (mut created, mut exited, mut kernel, mut user) =
            (zeroed::<FILETIME>(), zeroed::<FILETIME>(), zeroed::<FILETIME>(), zeroed::<FILETIME>());
        let ok = GetProcessTimes(process.as_raw_handle() as HANDLE, &mut created, &mut exited, &mut kernel, &mut user);
        // A process that exited stays openable while anything holds a handle to
        // it; its exit time says it is gone.
        if ok == 0 || exited.dwHighDateTime != 0 || exited.dwLowDateTime != 0 {
            return None;
        }
        // 100 ns steps since 1601.
        let ticks = (created.dwHighDateTime as u64) << 32 | created.dwLowDateTime as u64;
        Some((ticks / 10_000).saturating_sub(11_644_473_600_000))
    }
}

/// The full path of process `pid`'s program; None if it is gone.
pub fn process_path(pid: i32) -> Option<String> {
    unsafe {
        let process = open(pid)?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process.as_raw_handle() as HANDLE, 0, buf.as_mut_ptr(), &mut len);
        (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

unsafe fn open(pid: i32) -> Option<OwnedHandle> {
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) };
    // SAFETY: a handle just opened, owned from here on.
    (!raw.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) })
}

/// A function of the system's SQLite (winsqlite3.dll, from System32 only),
/// loaded the first time and kept loaded.
pub fn sqlite_symbol(name: &CStr) -> Option<*const c_void> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
    static MODULE: OnceLock<usize> = OnceLock::new();
    let module = *MODULE.get_or_init(|| {
        let name: Vec<u16> = "winsqlite3.dll".encode_utf16().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated name; the module is never freed.
        unsafe { LoadLibraryExW(name.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) as usize }
    });
    if module == 0 {
        return None;
    }
    // SAFETY: the module is loaded; `name` is NUL-terminated.
    unsafe { GetProcAddress(module as HMODULE, name.as_ptr().cast()) }.map(|f| f as *const c_void)
}

/// Windows doesn't tell `C:\X` from `c:\x`.
pub const PATHS_IGNORE_CASE: bool = true;

/// The process list as of the last two seconds. Listing every process takes
/// milliseconds, so a thread refreshes it in the background and asking (from
/// under the holder's lock, between output) only clones what it has.
type Processes = Arc<Vec<(u32, u32, String)>>;

pub(super) fn recent_processes() -> Processes {
    static LIST: OnceLock<Arc<std::sync::Mutex<Processes>>> = OnceLock::new();
    let list = LIST.get_or_init(|| {
        let list = Arc::new(std::sync::Mutex::new(Arc::new(processes())));
        let shared = list.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let now = Arc::new(processes());
                *shared.lock().unwrap_or_else(|p| p.into_inner()) = now;
            }
        });
        list
    });
    list.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Every process now: (pid, parent pid, file name), in the system's order.
fn processes() -> Vec<(u32, u32, String)> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let snap = OwnedHandle::from_raw_handle(snap as RawHandle);
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap.as_raw_handle() as HANDLE, &mut entry);
        while ok != 0 {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            out.push((
                entry.th32ProcessID,
                entry.th32ParentProcessID,
                String::from_utf16_lossy(&entry.szExeFile[..len]),
            ));
            ok = Process32NextW(snap.as_raw_handle() as HANDLE, &mut entry);
        }
    }
    out
}

/// Process `pid`'s children now (as of the last two seconds): pid and file name.
pub fn child_processes(pid: i32) -> Vec<(i32, String)> {
    recent_processes()
        .iter()
        .filter(|(child, parent, _)| *parent == pid as u32 && *child != pid as u32)
        .map(|(child, _, name)| (*child as i32, name.clone()))
        .collect()
}

/// The console host of process `pid` (the conhost of the pseudo console it is
/// attached to), which every process of one terminal shares, even one that
/// left the terminal's job; None for one without a console.
pub(super) fn console_host(pid: u32) -> Option<u32> {
    use windows_sys::Wdk::System::Threading::NtQueryInformationProcess;
    /// ProcessConsoleHostProcess: the host's pid, its low two bits flags.
    const CONSOLE_HOST: i32 = 49;
    unsafe {
        let process = open(pid as i32)?;
        let mut host: usize = 0;
        let size = size_of::<usize>() as u32;
        let h = process.as_raw_handle() as HANDLE;
        let status = NtQueryInformationProcess(h, CONSOLE_HOST, (&raw mut host).cast(), size, null_mut());
        let host = host & !3;
        (status == 0 && host != 0).then_some(host as u32)
    }
}

/// The files process `pid` has open on disk, by full path (`\\?\C:\...`);
/// None when its handles can't be read. Asking a file's name can wait on a
/// read another thread of it is blocked in, so the asking is done aside and
/// given up after a moment; a process that held it up once isn't asked again.
pub fn open_files(pid: i32) -> Option<Vec<String>> {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    /// Asks still stuck: past this many, none are made.
    static STUCK: AtomicUsize = AtomicUsize::new(0);
    static SLOW: Mutex<Vec<(i32, Option<u64>)>> = Mutex::new(Vec::new());
    if pid <= 0 {
        return None;
    }
    let start = process_started_ms(pid)?;
    if STUCK.load(Ordering::Relaxed) >= 4
        || SLOW.lock().unwrap_or_else(|p| p.into_inner()).contains(&(pid, Some(start)))
    {
        return None;
    }
    let file_type = file_type_index()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("open-files".into())
        .spawn(move || {
            // SAFETY: only reads the other process's handle table and names.
            let _ = tx.send(unsafe { disk_files(pid as u32, file_type) });
        })
        .ok()?;
    match rx.recv_timeout(Duration::from_millis(500)) {
        Ok(files) => files,
        Err(_) => {
            STUCK.fetch_add(1, Ordering::Relaxed);
            let mut slow = SLOW.lock().unwrap_or_else(|p| p.into_inner());
            if slow.len() >= 64 {
                slow.remove(0);
            }
            slow.push((pid, Some(start)));
            None
        }
    }
}

/// One entry of a process's handle table (PROCESS_HANDLE_TABLE_ENTRY_INFO).
#[repr(C)]
#[derive(Clone, Copy)]
struct HandleEntry {
    value: usize,
    count: usize,
    pointers: usize,
    access: u32,
    type_index: u32,
    attributes: u32,
    reserved: u32,
}

/// The handles `process` has open (ProcessHandleInformation).
unsafe fn handle_table(process: HANDLE) -> Option<Vec<HandleEntry>> {
    use windows_sys::Wdk::System::Threading::{NtQueryInformationProcess, ProcessHandleInformation};
    const TOO_SMALL: i32 = 0xC000_0004_u32 as i32;
    const HEADER: usize = 2 * size_of::<usize>();
    let mut size = 64 * 1024;
    for _ in 0..4 {
        let mut buf = vec![0usize; size / size_of::<usize>()];
        let mut needed = 0u32;
        let len = (buf.len() * size_of::<usize>()) as u32;
        let status = unsafe {
            NtQueryInformationProcess(process, ProcessHandleInformation, buf.as_mut_ptr().cast(), len, &mut needed)
        };
        if status == TOO_SMALL {
            size = needed as usize + 16 * 1024;
            continue;
        }
        if status != 0 {
            return None;
        }
        let room = (len as usize - HEADER) / size_of::<HandleEntry>();
        // SAFETY: the entries follow the header, `buf[0]` of them, within the buffer.
        let at = unsafe { buf.as_ptr().cast::<u8>().add(HEADER).cast::<HandleEntry>() };
        return Some((0..buf[0].min(room)).map(|i| unsafe { at.add(i).read_unaligned() }).collect());
    }
    None
}

/// The system's type number for files: that of a file this process opens.
fn file_type_index() -> Option<u32> {
    static INDEX: OnceLock<Option<u32>> = OnceLock::new();
    *INDEX.get_or_init(|| {
        let file = std::fs::File::open(std::env::current_exe().ok()?).ok()?;
        let raw = file.as_raw_handle() as usize;
        // SAFETY: our own process, through its pseudo handle.
        let table = unsafe { handle_table(GetCurrentProcess())? };
        table.iter().find(|e| e.value == raw).map(|e| e.type_index)
    })
}

/// The disk files process `pid` has open, by name.
unsafe fn disk_files(pid: u32, file_type: u32) -> Option<Vec<String>> {
    use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_DISK, GetFileType, GetFinalPathNameByHandleW};
    let raw = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE, 0, pid) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: a handle just opened, owned from here on.
    let process = unsafe { OwnedHandle::from_raw_handle(raw as RawHandle) };
    let h = process.as_raw_handle() as HANDLE;
    let me = unsafe { GetCurrentProcess() };
    let mut out = Vec::new();
    for e in unsafe { handle_table(h)? }.into_iter().filter(|e| e.type_index == file_type) {
        let mut dup: HANDLE = null_mut();
        if unsafe { DuplicateHandle(h, e.value as HANDLE, me, &mut dup, 0, 0, DUPLICATE_SAME_ACCESS) } == 0 {
            continue;
        }
        // SAFETY: a handle just made in this process, ours to close.
        let dup = unsafe { OwnedHandle::from_raw_handle(dup as RawHandle) };
        let dup = dup.as_raw_handle() as HANDLE;
        if unsafe { GetFileType(dup) } != FILE_TYPE_DISK {
            continue;
        }
        let mut name = [0u16; 1024];
        let n = unsafe { GetFinalPathNameByHandleW(dup, name.as_mut_ptr(), name.len() as u32, 0) } as usize;
        if n > 0 && n < name.len() {
            out.push(String::from_utf16_lossy(&name[..n]));
        }
    }
    Some(out)
}

/// Every process in `job` now (a terminal's job holds all it started).
pub(super) fn job_processes(job: &OwnedHandle) -> Vec<u32> {
    #[repr(C)]
    struct List {
        assigned: u32,
        listed: u32,
        ids: [usize; 512],
    }
    unsafe {
        let mut list: Box<List> = Box::new(zeroed());
        // More than fit fails with the first ones listed, enough to look among.
        QueryInformationJobObject(
            job.as_raw_handle() as HANDLE,
            JobObjectBasicProcessIdList,
            (&raw mut *list).cast(),
            size_of::<List>() as u32,
            null_mut(),
        );
        list.ids[..(list.listed as usize).min(512)].iter().map(|&id| id as u32).collect()
    }
}

/// The program a person runs under `root`: the nearest descendant that isn't a
/// shell, else `root`. Git Bash keeps a `bash.exe` between a shell and each
/// program it runs, so shells are walked through, not stopped at.
///
/// A shell script Git Bash runs (an npm shim like `pi`, `codex` or `opencode`)
/// has no parent left: the bash forked for it ends once the script's `sh.exe`
/// starts. Such a shell is still the terminal's: in its `job`, or (Git Bash
/// starts it out of the job) on its pseudo console (`console`: a process's
/// console host). A job run in the background (`npm run dev &`) looks just the
/// same, so when nothing else is found, the program under one counts as the
/// terminal's only if it is an agent's app (`agent`), which nobody runs in the
/// background.
pub(crate) fn foreground_of(
    root: u32,
    procs: &[(u32, u32, String)],
    job: &[u32],
    agent: impl Fn(u32) -> bool,
    console: impl Fn(u32) -> Option<u32>,
) -> u32 {
    let found = nearest_program(root, procs);
    if found != root {
        return found;
    }
    let in_job = |pid: &u32, parent: &u32| job.contains(pid) && !job.contains(parent);
    let orphan = |parent: &u32| !procs.iter().any(|(pid, _, _)| pid == parent);
    let host = std::cell::OnceCell::new();
    let on_console = |pid: u32| host.get_or_init(|| console(root)).is_some_and(|h| console(pid) == Some(h));
    procs
        .iter()
        .filter(|(pid, _, name)| *pid != root && is_shell(name))
        .filter(|(pid, parent, _)| in_job(pid, parent) || (orphan(parent) && on_console(*pid)))
        .map(|(orphan, _, _)| (*orphan, nearest_program(*orphan, procs)))
        .find(|(orphan, program)| program != orphan && agent(*program))
        .map_or(root, |(_, program)| program)
}

/// The nearest program under `root` that isn't a shell, else `root`.
fn nearest_program(root: u32, procs: &[(u32, u32, String)]) -> u32 {
    let mut level = vec![root];
    let mut seen = std::collections::HashSet::from([root]);
    for _ in 0..8 {
        let children: Vec<&(u32, u32, String)> =
            procs.iter().filter(|(pid, parent, _)| level.contains(parent) && seen.insert(*pid)).collect();
        if let Some((pid, _, _)) = children.iter().find(|(_, _, name)| !is_shell(name)) {
            return *pid;
        }
        if children.is_empty() {
            break;
        }
        level = children.iter().map(|(pid, _, _)| *pid).collect();
    }
    root
}

/// Shells and console helpers: what's under a program, never the program itself.
fn is_shell(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    matches!(
        name,
        "bash"
            | "sh"
            | "dash"
            | "zsh"
            | "fish"
            | "pwsh"
            | "powershell"
            | "cmd"
            | "nu"
            | "conhost"
            | "openconsole"
            | "winpty-agent"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent: u32, name: &str) -> (u32, u32, String) {
        (pid, parent, name.to_owned())
    }

    #[test]
    fn the_program_under_git_bash_stubs_is_the_foreground() {
        // Git's bash.exe launcher, the real bash, the stub bash it forks, then the agent.
        let tree = [
            p(10, 1, "bash.exe"),
            p(11, 10, "bash.exe"),
            p(12, 11, "bash.exe"),
            p(13, 12, "node.exe"),
            p(14, 13, "git.exe"),
        ];
        let any = |_| true;
        let none = |_| None;
        assert_eq!(foreground_of(10, &tree, &[], any, none), 13);
        assert_eq!(foreground_of(10, &tree[..3], &[], any, none), 10, "only shells: the shell itself");
        let pwsh = [p(10, 1, "pwsh.exe"), p(20, 10, "conhost.exe"), p(21, 10, "claude.exe")];
        assert_eq!(foreground_of(10, &pwsh, &[], any, none), 21);
        let other = [p(10, 1, "bash.exe"), p(30, 99, "codex.exe")];
        assert_eq!(foreground_of(10, &other, &[], any, none), 10, "another tree's program");
    }

    #[test]
    fn a_shim_whose_bash_ended_is_still_the_terminals_if_it_runs_an_agent() {
        // `pi` in Git Bash: the bash forked for it (40) is gone, its sh.exe and node stay.
        // `npm run dev &` before it left one the same way (60).
        let tree = [
            p(10, 1, "bash.exe"),
            p(11, 10, "bash.exe"),
            p(61, 60, "sh.exe"),
            p(62, 61, "node.exe"),
            p(41, 40, "sh.exe"),
            p(42, 41, "node.exe"),
            p(43, 42, "cmd.exe"),
            p(44, 43, "node.exe"),
            p(51, 50, "sh.exe"),
            p(52, 51, "vim.exe"),
        ];
        let job = [10, 11, 61, 62, 41, 42, 43, 44];
        let agent = |pid| pid == 42;
        let none = |_| None;
        assert_eq!(foreground_of(10, &tree, &job, agent, none), 42);
        assert_eq!(foreground_of(10, &tree, &[], agent, none), 10, "not in this terminal's job: another's");
        // At the prompt, with only the background job left: the shell.
        assert_eq!(foreground_of(10, &tree, &job, |_| false, none), 10);
        // A program found the usual way comes first.
        let mut more = tree.to_vec();
        more.push(p(12, 11, "claude.exe"));
        assert_eq!(foreground_of(10, &more, &job, agent, none), 12);
    }

    #[test]
    fn a_shim_out_of_the_job_is_the_terminals_by_its_console() {
        // Git Bash starts the shim's sh.exe out of the terminal's job; it shares its console.
        let tree = [p(10, 1, "bash.exe"), p(11, 10, "bash.exe"), p(41, 40, "sh.exe"), p(42, 41, "node.exe")];
        let job = [10, 11];
        let agent = |pid| pid == 42;
        let console = |pid| matches!(pid, 10 | 11 | 41 | 42).then_some(500);
        assert_eq!(foreground_of(10, &tree, &job, agent, console), 42);
        let elsewhere = |pid| if pid == 41 { Some(600) } else { Some(500) };
        assert_eq!(foreground_of(10, &tree, &job, agent, elsewhere), 10, "another terminal's console");
        assert_eq!(foreground_of(10, &tree, &job, agent, |_| None), 10, "no console known");
        // Its parent still running: not an orphan, so another tree's.
        let mut parented = tree.to_vec();
        parented.push(p(40, 1, "bash.exe"));
        assert_eq!(foreground_of(10, &parented, &job, agent, console), 10);
    }

    #[test]
    fn reads_its_own_open_files_and_console() {
        let path = std::env::temp_dir().join(format!("herdr-open-files-{}.txt", std::process::id()));
        let held = std::fs::File::create(&path).unwrap();
        let files = open_files(std::process::id() as i32).expect("its own handles");
        let name = path.file_name().unwrap().to_string_lossy().to_ascii_lowercase();
        assert!(files.iter().any(|f| f.to_ascii_lowercase().ends_with(&name)), "{files:?}");
        drop(held);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(open_files(-1), None);
        // Which console a process is on (how a test is run decides ours).
        let _ = console_host(std::process::id());
        assert_eq!(console_host(u32::MAX), None, "no such process");
    }

    #[test]
    fn reads_processes_and_the_system_sqlite() {
        let me = std::process::id() as i32;
        let exe = std::env::current_exe().unwrap();
        assert_eq!(process_path(me).map(std::path::PathBuf::from), Some(exe));
        assert!(process_started_ms(me).is_some_and(|ms| ms <= crate::runtime::now_ms()));
        assert_eq!(process_path(-1), None);
        assert!(sqlite_symbol(c"sqlite3_open_v2").is_some(), "every Windows has winsqlite3.dll");
    }

    #[test]
    fn an_exited_process_is_gone_even_while_a_handle_holds_it() {
        // `child` keeps a handle to the process after it exits.
        let mut child = std::process::Command::new("cmd").args(["/c", "exit 0"]).spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert_eq!(process_started_ms(pid), None);
    }
}
