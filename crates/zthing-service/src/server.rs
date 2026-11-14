//! Answers protocol requests from an index. Free of Windows calls, so the
//! tests run on small in-memory indexes.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::AtomicBool;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use zthing_client::{
    Command, Field, Hit, PROTOCOL_VERSION, Request, Response, Search, Sort, Status,
};
use zthing_core::{Index, Now, Query, SortKey};

/// Upper bound for one page, which keeps a single answer at a few megabytes.
pub const MAX_LIMIT: u64 = 10_000;
const NOT_READY: &str = "the service is still reading the drives";

/// Checks folder access as the client. The service implements it with impersonation.
pub trait Access {
    /// Runs `check` with the client's access rights.
    fn as_client(&self, check: &mut dyn FnMut()) -> io::Result<()>;
    fn can_list(&self, folder: &str) -> bool;
}

pub struct Context<'a> {
    /// `None` while the drives are read for the first time.
    pub index: Option<&'a Index>,
    /// Changes with every change to the index.
    pub version: u64,
    /// Changes when entry ids change, on compaction and rebuild.
    pub epoch: u64,
    pub status: Status,
}

/// State of one client connection.
#[derive(Default)]
pub struct Session {
    previous: Option<Previous>,
    /// Folder id and whether the client may list it, valid for `access_epoch`.
    access: HashMap<u32, bool>,
    access_epoch: u64,
}

/// The last search, kept so paging does not search again and so a longer
/// input searches only within these hits.
struct Previous {
    text: String,
    sort: Sort,
    descending: bool,
    version: u64,
    query: Query,
    ids: Vec<u32>,
}

/// An `Err` means impersonation failed and the connection must end.
pub fn handle(
    line: &str,
    context: &Context<'_>,
    session: &mut Session,
    access: Option<&dyn Access>,
) -> io::Result<Response> {
    let request: Request = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(e) => return Ok(failure(0, format!("invalid request: {e}"))),
    };
    let id = request.id;
    if request.v != PROTOCOL_VERSION {
        return Ok(failure(
            id,
            format!(
                "protocol version {} is not supported, this service speaks version {PROTOCOL_VERSION}",
                request.v
            ),
        ));
    }
    let mut response = Response {
        v: PROTOCOL_VERSION,
        id,
        ok: true,
        ..Response::default()
    };
    match request.command {
        Command::Version => {
            response.version = Some(env!("CARGO_PKG_VERSION").to_owned());
            response.protocol = Some(PROTOCOL_VERSION);
        }
        Command::Status => response.status = Some(context.status.clone()),
        Command::Count { query } => {
            let Some(index) = context.index else {
                return Ok(failure(id, NOT_READY.to_owned()));
            };
            let query = match Query::parse(&query, now()) {
                Ok(query) => query,
                Err(e) => return Ok(failure(id, e.to_string())),
            };
            let mut ids = index
                .search(&query, None, &AtomicBool::new(false))
                .unwrap_or_default();
            if let Some(access) = access {
                filter(index, &mut ids, session, context.epoch, access)?;
            }
            response.total = Some(ids.len() as u64);
        }
        Command::Search(search) => {
            let Some(index) = context.index else {
                return Ok(failure(id, NOT_READY.to_owned()));
            };
            let start = Instant::now();
            if let Err(e) = run_search(index, &search, context, session, access) {
                return match e {
                    SearchError::Query(message) => Ok(failure(id, message)),
                    SearchError::Access(e) => Err(e),
                };
            }
            let ids = session
                .previous
                .as_ref()
                .map_or(&[][..], |p| p.ids.as_slice());
            let total = ids.len() as u64;
            let offset = search.offset.min(total) as usize;
            let end =
                (search.offset.saturating_add(search.limit.min(MAX_LIMIT))).min(total) as usize;
            response.total = Some(total);
            response.millis = Some(start.elapsed().as_secs_f64() * 1e3);
            response.results = Some(
                ids[offset..end]
                    .iter()
                    .map(|&id| hit(index, id, &search.fields))
                    .collect(),
            );
        }
    }
    Ok(response)
}

enum SearchError {
    Query(String),
    Access(io::Error),
}

/// Leaves the sorted, filtered hits in `session.previous`.
fn run_search(
    index: &Index,
    search: &Search,
    context: &Context<'_>,
    session: &mut Session,
    access: Option<&dyn Access>,
) -> Result<(), SearchError> {
    let current = |p: &Previous| p.version == context.version;
    if session.previous.as_ref().is_some_and(|p| {
        current(p)
            && p.text == search.query
            && p.sort == search.sort
            && p.descending == search.descending
    }) {
        return Ok(());
    }
    let query =
        Query::parse(&search.query, now()).map_err(|e| SearchError::Query(e.to_string()))?;
    let within = session
        .previous
        .as_ref()
        .filter(|p| current(p) && query.narrows(&p.query))
        .map(|p| p.ids.as_slice());
    let mut ids = index
        .search(&query, within, &AtomicBool::new(false))
        .unwrap_or_default();
    if let Some(access) = access {
        filter(index, &mut ids, session, context.epoch, access).map_err(SearchError::Access)?;
    }
    index.sort(
        &mut ids,
        match search.sort {
            Sort::Name => SortKey::Name,
            Sort::Path => SortKey::Path,
            Sort::Size => SortKey::Size,
            Sort::Modified => SortKey::Modified,
        },
    );
    if search.descending {
        ids.reverse();
    }
    session.previous = Some(Previous {
        text: search.query.clone(),
        sort: search.sort,
        descending: search.descending,
        version: context.version,
        query,
        ids,
    });
    Ok(())
}

/// Keeps entries whose folder the client may list. Listing a folder reveals
/// the names in it, so the folder of a hit decides, not the hit itself.
fn filter(
    index: &Index,
    ids: &mut Vec<u32>,
    session: &mut Session,
    epoch: u64,
    access: &dyn Access,
) -> io::Result<()> {
    if session.access_epoch != epoch {
        session.access.clear();
        session.access_epoch = epoch;
    }
    let cache = &mut session.access;
    access.as_client(&mut || {
        ids.retain(|&id| match index.parent(id) {
            None => true,
            Some(folder) => *cache
                .entry(folder)
                .or_insert_with(|| access.can_list(&index.path(folder))),
        });
    })
}

fn hit(index: &Index, id: u32, fields: &[Field]) -> Hit {
    let mut hit = Hit::default();
    for field in fields {
        match field {
            Field::Name => hit.name = Some(index.name(id).to_owned()),
            Field::Path => hit.path = Some(index.path(id)),
            Field::Size => hit.size = Some(index.size(id)),
            Field::Modified => hit.modified = Some(index.modified(id)),
            Field::Folder => hit.folder = Some(index.is_dir(id)),
        }
    }
    hit
}

fn failure(id: u64, error: String) -> Response {
    Response {
        v: PROTOCOL_VERSION,
        id,
        ok: false,
        error: Some(error),
        ..Response::default()
    }
}

fn now() -> Now {
    Now {
        unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        utc_offset: i64::from(chrono::Local::now().offset().local_minus_utc()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zthing_core::ATTRIBUTE_DIRECTORY;

    fn index() -> Index {
        let mut index = Index::new();
        let root = index.push("C:", None, ATTRIBUTE_DIRECTORY, 0, 0).unwrap();
        let public = index
            .push("Public", Some(root), ATTRIBUTE_DIRECTORY, 0, 0)
            .unwrap();
        let secret = index
            .push("Secret", Some(root), ATTRIBUTE_DIRECTORY, 0, 0)
            .unwrap();
        for (name, parent, size) in [
            ("a.txt", public, 30),
            ("b.txt", public, 10),
            ("c.txt", public, 20),
            ("hidden.txt", secret, 5),
        ] {
            index.push(name, Some(parent), 0, size, 0).unwrap();
        }
        index
    }

    fn context(index: &Index) -> Context<'_> {
        Context {
            index: Some(index),
            version: 1,
            epoch: 1,
            status: Status::default(),
        }
    }

    fn ask(
        line: &str,
        context: &Context<'_>,
        session: &mut Session,
        access: Option<&dyn Access>,
    ) -> Response {
        handle(line, context, session, access).unwrap()
    }

    fn paths(response: &Response) -> Vec<String> {
        response
            .results
            .iter()
            .flatten()
            .map(|hit| hit.path.clone().unwrap_or_default())
            .collect()
    }

    struct DenySecret;

    impl Access for DenySecret {
        fn as_client(&self, check: &mut dyn FnMut()) -> io::Result<()> {
            check();
            Ok(())
        }

        fn can_list(&self, folder: &str) -> bool {
            folder != r"C:\Secret"
        }
    }

    #[test]
    fn search_pages_through_sorted_hits() {
        let index = index();
        let context = context(&index);
        let mut session = Session::default();
        let first = ask(
            r#"{"v":1,"id":1,"cmd":"search","query":".txt","sort":"size","descending":true,"limit":2,"fields":["path","size"]}"#,
            &context,
            &mut session,
            None,
        );
        assert!(first.ok);
        assert_eq!(first.total, Some(4));
        assert_eq!(paths(&first), [r"C:\Public\a.txt", r"C:\Public\c.txt"]);
        let hits = first.results.unwrap();
        assert_eq!((hits[0].size, hits[0].name.as_ref()), (Some(30), None));

        let second = ask(
            r#"{"v":1,"id":2,"cmd":"search","query":".txt","sort":"size","descending":true,"offset":2,"limit":2,"fields":["path"]}"#,
            &context,
            &mut session,
            None,
        );
        assert_eq!(
            paths(&second),
            [r"C:\Public\b.txt", r"C:\Secret\hidden.txt"]
        );
    }

    #[test]
    fn hits_in_folders_the_client_cannot_list_are_hidden() {
        let index = index();
        let context = context(&index);
        let mut session = Session::default();
        let search = ask(
            r#"{"v":1,"id":1,"cmd":"search","query":".txt","fields":["path"]}"#,
            &context,
            &mut session,
            Some(&DenySecret),
        );
        assert_eq!(search.total, Some(3));
        assert!(!paths(&search).iter().any(|path| path.contains("hidden")));
        let count = ask(
            r#"{"v":1,"id":2,"cmd":"count","query":"hidden"}"#,
            &context,
            &mut session,
            Some(&DenySecret),
        );
        assert_eq!(count.total, Some(0));
        // The folder itself sits in the listable root and stays visible.
        let folder = ask(
            r#"{"v":1,"id":3,"cmd":"count","query":"secret"}"#,
            &context,
            &mut session,
            Some(&DenySecret),
        );
        assert_eq!(folder.total, Some(1));
    }

    #[test]
    fn errors_are_answers_not_disconnects() {
        let index = index();
        let mut session = Session::default();
        let ready = context(&index);
        let wrong_version = ask(
            r#"{"v":99,"id":4,"cmd":"status"}"#,
            &ready,
            &mut session,
            None,
        );
        assert_eq!((wrong_version.ok, wrong_version.id), (false, 4));
        assert!(!ask("not json", &ready, &mut session, None).ok);
        let bad_query = ask(
            r#"{"v":1,"id":5,"cmd":"search","query":"size:>1xb"}"#,
            &ready,
            &mut session,
            None,
        );
        assert!(bad_query.error.unwrap().contains("size"));

        let loading = Context {
            index: None,
            ..context(&index)
        };
        let not_ready = ask(
            r#"{"v":1,"id":6,"cmd":"count","query":"a"}"#,
            &loading,
            &mut session,
            None,
        );
        assert_eq!(not_ready.error.as_deref(), Some(NOT_READY));
        let version = ask(
            r#"{"v":1,"id":7,"cmd":"version"}"#,
            &loading,
            &mut session,
            None,
        );
        assert_eq!(version.protocol, Some(PROTOCOL_VERSION));
    }
}
