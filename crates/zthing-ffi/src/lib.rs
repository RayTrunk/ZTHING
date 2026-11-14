//! zthing.dll: a C ABI over `zthing-client`, declared in `include/zthing.h`.
//!
//! The contract shared by every function, which is why the functions carry no
//! `# Safety` section of their own:
//! - `handle` comes from `zthing_connect` and has not been freed. Null is accepted
//!   and reported as `ZTHING_ERROR_ARGUMENT` or an empty result.
//! - String arguments are null-terminated, UTF-8 for `char` and UTF-16 for `uint16_t`.
//! - Returned strings belong to the handle and stay valid until the next
//!   `zthing_execute` or `zthing_free` on it.
//! - A handle is used by one thread at a time.
#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString, c_char};
use std::ptr;

use zthing_client::{Client, ClientError, Field, Search, Sort};

const ZTHING_OK: i32 = 0;
/// The service is unreachable or the connection broke. The next call reconnects.
const ZTHING_ERROR_SERVICE: i32 = 1;
/// The service rejected the query, for example a bad regex.
const ZTHING_ERROR_QUERY: i32 = 2;
const ZTHING_ERROR_ARGUMENT: i32 = 3;
/// Results returned when the caller sets no range. `zthing_set_range` lifts it.
const DEFAULT_LIMIT: u64 = 1000;
/// Results per pipe request. The service caps a request at the same size.
const PAGE: u64 = 10_000;
/// Returned by the error getters for a null handle.
const EMPTY_UTF16: &[u16] = &[0];

/// Each string in both encodings, so the getters hand out pointers without converting.
struct Text {
    utf8: CString,
    utf16: Vec<u16>,
}

impl Text {
    fn new(text: &str) -> Self {
        Self {
            // NTFS names and the service's messages contain no NUL.
            utf8: CString::new(text).unwrap_or_default(),
            utf16: text.encode_utf16().chain([0]).collect(),
        }
    }
}

struct Row {
    name: Text,
    path: Text,
    size: u64,
    modified: i64,
    folder: bool,
}

pub struct Zthing {
    client: Option<Client>,
    search: Search,
    rows: Vec<Row>,
    total: u64,
    error: Text,
}

impl Zthing {
    fn fail(&mut self, code: i32, message: &str) -> i32 {
        self.error = Text::new(message);
        code
    }

    fn execute(&mut self) -> i32 {
        self.rows.clear();
        self.total = 0;
        let client = match &mut self.client {
            Some(client) => client,
            None => match Client::connect() {
                Ok(client) => self.client.insert(client),
                Err(e) => return self.fail(ZTHING_ERROR_SERVICE, &e.to_string()),
            },
        };
        let end = self.search.offset.saturating_add(self.search.limit);
        let mut offset = self.search.offset;
        loop {
            let page = client.search(Search {
                offset,
                limit: (end - offset).min(PAGE),
                ..self.search.clone()
            });
            let page = match page {
                Ok(page) => page,
                Err(ClientError::Service(message)) => {
                    return self.fail(ZTHING_ERROR_QUERY, &message);
                }
                Err(e) => {
                    self.client = None;
                    return self.fail(ZTHING_ERROR_SERVICE, &e.to_string());
                }
            };
            self.total = page.total;
            offset += page.hits.len() as u64;
            let done = page.hits.is_empty() || offset >= end.min(page.total);
            self.rows.extend(page.hits.into_iter().map(|hit| Row {
                name: Text::new(hit.name.as_deref().unwrap_or_default()),
                path: Text::new(hit.path.as_deref().unwrap_or_default()),
                size: hit.size.unwrap_or(0),
                modified: hit.modified.unwrap_or(0),
                folder: hit.folder.unwrap_or(false),
            }));
            if done {
                break;
            }
        }
        self.error = Text::new("");
        ZTHING_OK
    }
}

unsafe fn handle<'a>(handle: *mut Zthing) -> Option<&'a mut Zthing> {
    // SAFETY: by the module contract the pointer is null or a live handle.
    unsafe { handle.as_mut() }
}

unsafe fn row<'a>(handle: *mut Zthing, index: u64) -> Option<&'a Row> {
    // SAFETY: by the module contract the pointer is null or a live handle.
    let zthing = unsafe { self::handle(handle) }?;
    zthing.rows.get(usize::try_from(index).ok()?)
}

/// Never returns null. If the service is unreachable, `zthing_last_error` says
/// why and `zthing_execute` tries again.
#[unsafe(no_mangle)]
pub extern "C" fn zthing_connect() -> *mut Zthing {
    let (client, error) = match Client::connect() {
        Ok(client) => (Some(client), String::new()),
        Err(e) => (None, e.to_string()),
    };
    Box::into_raw(Box::new(Zthing {
        client,
        search: Search {
            limit: DEFAULT_LIMIT,
            fields: vec![
                Field::Name,
                Field::Path,
                Field::Size,
                Field::Modified,
                Field::Folder,
            ],
            ..Search::default()
        },
        rows: Vec::new(),
        total: 0,
        error: Text::new(&error),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_free(handle: *mut Zthing) {
    if !handle.is_null() {
        // SAFETY: the pointer came from Box::into_raw in zthing_connect and is freed once.
        drop(unsafe { Box::from_raw(handle) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_set_query(handle: *mut Zthing, query: *const c_char) -> i32 {
    // SAFETY: by the module contract.
    let Some(zthing) = (unsafe { self::handle(handle) }) else {
        return ZTHING_ERROR_ARGUMENT;
    };
    if query.is_null() {
        return zthing.fail(ZTHING_ERROR_ARGUMENT, "query is null");
    }
    // SAFETY: a non-null query is a null-terminated string by the module contract.
    match unsafe { CStr::from_ptr(query) }.to_str() {
        Ok(query) => {
            zthing.search.query = query.to_owned();
            ZTHING_OK
        }
        Err(_) => zthing.fail(ZTHING_ERROR_ARGUMENT, "query is not UTF-8"),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_set_query_w(handle: *mut Zthing, query: *const u16) -> i32 {
    // SAFETY: by the module contract.
    let Some(zthing) = (unsafe { self::handle(handle) }) else {
        return ZTHING_ERROR_ARGUMENT;
    };
    if query.is_null() {
        return zthing.fail(ZTHING_ERROR_ARGUMENT, "query is null");
    }
    let mut len = 0;
    // SAFETY: a non-null query is null-terminated, so every read up to the terminator is in bounds.
    while unsafe { *query.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: the `len` units before the terminator were just read.
    let units = unsafe { std::slice::from_raw_parts(query, len) };
    match String::from_utf16(units) {
        Ok(query) => {
            zthing.search.query = query;
            ZTHING_OK
        }
        Err(_) => zthing.fail(ZTHING_ERROR_ARGUMENT, "query is not valid UTF-16"),
    }
}

/// `sort`: 0 name, 1 path, 2 size, 3 date modified. `descending`: 0 or 1.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_set_sort(handle: *mut Zthing, sort: i32, descending: i32) -> i32 {
    // SAFETY: by the module contract.
    let Some(zthing) = (unsafe { self::handle(handle) }) else {
        return ZTHING_ERROR_ARGUMENT;
    };
    zthing.search.sort = match sort {
        0 => Sort::Name,
        1 => Sort::Path,
        2 => Sort::Size,
        3 => Sort::Modified,
        _ => return zthing.fail(ZTHING_ERROR_ARGUMENT, "sort must be 0 to 3"),
    };
    zthing.search.descending = descending != 0;
    ZTHING_OK
}

/// Skips `offset` matches and returns at most `limit`. `UINT64_MAX` returns all.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_set_range(handle: *mut Zthing, offset: u64, limit: u64) -> i32 {
    // SAFETY: by the module contract.
    let Some(zthing) = (unsafe { self::handle(handle) }) else {
        return ZTHING_ERROR_ARGUMENT;
    };
    zthing.search.offset = offset;
    zthing.search.limit = limit;
    ZTHING_OK
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_execute(handle: *mut Zthing) -> i32 {
    // SAFETY: by the module contract.
    match unsafe { self::handle(handle) } {
        Some(zthing) => zthing.execute(),
        None => ZTHING_ERROR_ARGUMENT,
    }
}

/// All matches of the last query, also those outside the range.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_total(handle: *mut Zthing) -> u64 {
    // SAFETY: by the module contract.
    unsafe { self::handle(handle) }.map_or(0, |zthing| zthing.total)
}

/// Results held by the handle, indexed from 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_result_count(handle: *mut Zthing) -> u64 {
    // SAFETY: by the module contract.
    unsafe { self::handle(handle) }.map_or(0, |zthing| zthing.rows.len() as u64)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_result_name(handle: *mut Zthing, index: u64) -> *const c_char {
    // SAFETY: by the module contract.
    unsafe { row(handle, index) }.map_or(ptr::null(), |row| row.name.utf8.as_ptr())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_result_name_w(handle: *mut Zthing, index: u64) -> *const u16 {
    // SAFETY: by the module contract.
    unsafe { row(handle, index) }.map_or(ptr::null(), |row| row.name.utf16.as_ptr())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_result_path(handle: *mut Zthing, index: u64) -> *const c_char {
    // SAFETY: by the module contract.
    unsafe { row(handle, index) }.map_or(ptr::null(), |row| row.path.utf8.as_ptr())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_result_path_w(handle: *mut Zthing, index: u64) -> *const u16 {
    // SAFETY: by the module contract.
    unsafe { row(handle, index) }.map_or(ptr::null(), |row| row.path.utf16.as_ptr())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_result_size(handle: *mut Zthing, index: u64) -> u64 {
    // SAFETY: by the module contract.
    unsafe { row(handle, index) }.map_or(0, |row| row.size)
}

/// Unix seconds (UTC).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_result_modified(handle: *mut Zthing, index: u64) -> i64 {
    // SAFETY: by the module contract.
    unsafe { row(handle, index) }.map_or(0, |row| row.modified)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_result_is_folder(handle: *mut Zthing, index: u64) -> i32 {
    // SAFETY: by the module contract.
    unsafe { row(handle, index) }.map_or(0, |row| i32::from(row.folder))
}

/// Empty after a successful call and for a null handle. Never null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_last_error(handle: *mut Zthing) -> *const c_char {
    // SAFETY: by the module contract.
    unsafe { self::handle(handle) }.map_or(c"".as_ptr(), |zthing| zthing.error.utf8.as_ptr())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn zthing_last_error_w(handle: *mut Zthing) -> *const u16 {
    // SAFETY: by the module contract.
    unsafe { self::handle(handle) }
        .map_or(EMPTY_UTF16.as_ptr(), |zthing| zthing.error.utf16.as_ptr())
}
