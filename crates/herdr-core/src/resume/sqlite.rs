//! Just enough SQLite to read a few rows of another program's database, through
//! the system's own library, loaded the first time it is needed
//! (`platform::sqlite_symbol`): Windows has winsqlite3.dll, macOS and most
//! Linux systems have libsqlite3. Where there is none, or the build can't load
//! one (the static Linux release), nothing is read; herdr doesn't build or
//! ship SQLite for this.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;

type Open = unsafe extern "system" fn(*const c_char, *mut *mut c_void, c_int, *const c_char) -> c_int;
type Prepare =
    unsafe extern "system" fn(*mut c_void, *const c_char, c_int, *mut *mut c_void, *mut *const c_char) -> c_int;
type WithInt = unsafe extern "system" fn(*mut c_void, c_int) -> c_int;
type Text = unsafe extern "system" fn(*mut c_void, c_int) -> *const u8;
type One = unsafe extern "system" fn(*mut c_void) -> c_int;

struct Api {
    open: Open,
    busy_timeout: WithInt,
    prepare: Prepare,
    step: One,
    text: Text,
    finalize: One,
    close: One,
}

const READ_ONLY: c_int = 0x1;
const ROW: c_int = 100;
const DONE: c_int = 101;

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load).as_ref()
}

fn load() -> Option<Api> {
    symbols(crate::platform::sqlite_symbol)
}

fn symbols(get: impl Fn(&CStr) -> Option<*const c_void>) -> Option<Api> {
    // SAFETY: these exports have exactly these signatures (sqlite3.h), and the
    // library stays loaded.
    unsafe {
        Some(Api {
            open: std::mem::transmute::<*const c_void, Open>(get(c"sqlite3_open_v2")?),
            busy_timeout: std::mem::transmute::<*const c_void, WithInt>(get(c"sqlite3_busy_timeout")?),
            prepare: std::mem::transmute::<*const c_void, Prepare>(get(c"sqlite3_prepare_v2")?),
            step: std::mem::transmute::<*const c_void, One>(get(c"sqlite3_step")?),
            text: std::mem::transmute::<*const c_void, Text>(get(c"sqlite3_column_text")?),
            finalize: std::mem::transmute::<*const c_void, One>(get(c"sqlite3_finalize")?),
            close: std::mem::transmute::<*const c_void, One>(get(c"sqlite3_close")?),
        })
    }
}

/// Every row `sql` gives from the database at `path`, opened read-only, its
/// first `columns` columns as text (None for NULL); None if it can't be read.
pub fn rows(path: &Path, sql: &CStr, columns: usize) -> Option<Vec<Vec<Option<String>>>> {
    let api = api()?;
    let path = CString::new(path.to_str()?).ok()?;
    let mut db = null_mut();
    // SAFETY: a NUL-terminated path; the handle is closed even when opening
    // fails (closing none does nothing).
    unsafe {
        let opened = (api.open)(path.as_ptr(), &mut db, READ_ONLY, null()) == 0;
        let out = if opened { query(api, db, sql, columns) } else { None };
        (api.close)(db);
        out
    }
}

fn query(api: &Api, db: *mut c_void, sql: &CStr, columns: usize) -> Option<Vec<Vec<Option<String>>>> {
    let mut stmt = null_mut();
    // SAFETY: `db` is open; `stmt` is finalized before returning; each text is
    // copied before the next step.
    unsafe {
        // A writer has it for a moment at most: never wait long on one.
        (api.busy_timeout)(db, 20);
        if (api.prepare)(db, sql.as_ptr(), -1, &mut stmt, null_mut()) != 0 {
            (api.finalize)(stmt);
            return None;
        }
        let mut out = Vec::new();
        let done = loop {
            match (api.step)(stmt) {
                ROW => out.push(
                    (0..columns as c_int)
                        .map(|i| {
                            let text = (api.text)(stmt, i);
                            (!text.is_null()).then(|| CStr::from_ptr(text.cast()).to_string_lossy().into_owned())
                        })
                        .collect(),
                ),
                DONE => break true,
                _ => break false,
            }
        };
        (api.finalize)(stmt);
        done.then_some(out)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_missing_database_is_none() {
        let path = std::env::temp_dir().join(format!("herdr-no-such-{}.db", std::process::id()));
        assert_eq!(super::rows(&path, c"SELECT 1", 1), None);
    }
}
