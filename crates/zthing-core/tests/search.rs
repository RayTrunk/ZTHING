#![allow(clippy::unwrap_used)]

use std::sync::atomic::AtomicBool;

use zthing_core::{ATTRIBUTE_DIRECTORY, Index, LoadError, Now, Query, QueryError, SortKey};

const DAY: i64 = 86_400;
/// 2026-03-15 12:00 UTC, 13:00 local time in CET.
const NOW: Now = Now {
    unix: 1_773_576_000,
    utc_offset: 3600,
};
const MB: u64 = 1 << 20;

fn sample() -> Index {
    let mut index = Index::new();
    let mut add = |name: &str, parent: Option<u32>, dir: bool, size: u64, modified: i64| {
        let attributes = if dir { ATTRIBUTE_DIRECTORY } else { 0 };
        index
            .push(name, parent, attributes, size, modified)
            .unwrap()
    };
    let root = add("C:", None, true, 0, 0);
    let users = add("Users", Some(root), true, 0, 0);
    let anna = add("anna", Some(users), true, 0, 0);
    // 2026-03-14 23:30 UTC is already 2026-03-15 in local time.
    add(
        "Report 2026.PDF",
        Some(anna),
        false,
        12 * MB,
        NOW.unix - DAY / 2 - 1800,
    );
    add("report-draft.docx", Some(anna), false, 500, NOW.unix - DAY);
    add("Ärger.txt", Some(anna), false, 0, NOW.unix - 33 * DAY);
    let docs = add("My Docs", Some(anna), true, 0, 0);
    add(
        "budget.xlsx",
        Some(docs),
        false,
        2 * MB,
        NOW.unix - 74 * DAY,
    );
    let projects = add("Projects", Some(root), true, 0, 0);
    let report = add("report", Some(projects), true, 0, 0);
    add("main.rs", Some(report), false, 300, NOW.unix - 2 * DAY);
    add(
        "İstanbul.jpg",
        Some(projects),
        false,
        3 * MB,
        NOW.unix - 300 * DAY,
    );
    add(
        "zebra.png",
        Some(projects),
        false,
        900 << 10,
        NOW.unix - 300 * DAY,
    );
    index
}

fn search(query: &str) -> Vec<String> {
    let index = sample();
    let query = Query::parse(query, NOW).unwrap();
    let mut paths: Vec<String> = index
        .search(&query, None, &AtomicBool::new(false))
        .unwrap()
        .into_iter()
        .map(|id| index.path(id))
        .collect();
    paths.sort();
    paths
}

#[test]
fn text_matches_anywhere_in_name_ignoring_case() {
    assert_eq!(
        search("report"),
        [
            r"C:\Projects\report",
            r"C:\Users\anna\Report 2026.PDF",
            r"C:\Users\anna\report-draft.docx",
        ]
    );
    assert_eq!(search("case:Report"), [r"C:\Users\anna\Report 2026.PDF"]);
}

#[test]
fn case_folding_covers_non_ascii_names() {
    assert_eq!(search("ärger"), [r"C:\Users\anna\Ärger.txt"]);
    assert_eq!(search("İst"), [r"C:\Projects\İstanbul.jpg"]);
    // 'İ' lowercases to two chars. Later entries must still be found at the right offsets.
    assert_eq!(search("zebra"), [r"C:\Projects\zebra.png"]);
}

#[test]
fn space_is_and_bar_is_or_and_bang_is_not() {
    assert_eq!(search("report draft"), [r"C:\Users\anna\report-draft.docx"]);
    assert_eq!(
        search("budget|zebra"),
        [
            r"C:\Projects\zebra.png",
            r"C:\Users\anna\My Docs\budget.xlsx"
        ]
    );
    assert_eq!(
        search("report !draft"),
        [r"C:\Projects\report", r"C:\Users\anna\Report 2026.PDF"]
    );
}

#[test]
fn bar_binds_tighter_than_space() {
    assert_eq!(
        search("path:anna report | budget"),
        [
            r"C:\Users\anna\My Docs\budget.xlsx",
            r"C:\Users\anna\Report 2026.PDF",
            r"C:\Users\anna\report-draft.docx",
        ]
    );
}

#[test]
fn quotes_keep_a_phrase_together() {
    assert_eq!(search(r#""my docs""#), [r"C:\Users\anna\My Docs"]);
    assert_eq!(search("docs my"), [r"C:\Users\anna\My Docs"]);
    assert!(search(r#""docs my""#).is_empty());
}

#[test]
fn wildcards_match_the_whole_name() {
    assert_eq!(search("*.pdf"), [r"C:\Users\anna\Report 2026.PDF"]);
    assert!(search("*.p").is_empty());
    assert_eq!(search("?stanbul.*"), [r"C:\Projects\İstanbul.jpg"]);
}

#[test]
fn regex_terms() {
    assert_eq!(
        search("regex:^report-"),
        [r"C:\Users\anna\report-draft.docx"]
    );
    assert_eq!(
        search(r"regex:\d{4}\.pdf$"),
        [r"C:\Users\anna\Report 2026.PDF"]
    );
    assert!(search(r"case:regex:\d{4}\.pdf$").is_empty());
}

#[test]
fn ext_accepts_a_list() {
    assert_eq!(
        search("ext:pdf;xlsx"),
        [
            r"C:\Users\anna\My Docs\budget.xlsx",
            r"C:\Users\anna\Report 2026.PDF",
        ]
    );
}

#[test]
fn path_matches_against_the_full_path() {
    assert_eq!(
        search(r"path:projects\report"),
        [r"C:\Projects\report", r"C:\Projects\report\main.rs"]
    );
}

#[test]
fn path_search_respects_case_regex_and_later_entries() {
    assert_eq!(
        search(r"case:path:Projects\report\main"),
        [r"C:\Projects\report\main.rs"]
    );
    assert!(search(r"case:path:projects\report\main").is_empty());
    assert_eq!(search(r"path:regex:users\\anna$"), [r"C:\Users\anna"]);

    let mut index = sample();
    let cancel = AtomicBool::new(false);
    let query = Query::parse(r"path:anna\new", NOW).unwrap();
    assert_eq!(index.search(&query, None, &cancel), Some(vec![]));
    let anna = 2;
    let new = index
        .push("New", Some(anna), ATTRIBUTE_DIRECTORY, 0, 0)
        .unwrap();
    assert_eq!(index.search(&query, None, &cancel), Some(vec![new]));
}

#[test]
fn file_and_folder_restrict_the_type() {
    assert_eq!(search("folder:report"), [r"C:\Projects\report"]);
    assert_eq!(search("file:report").len(), 2);
    assert_eq!(search("folder:").len(), 6);
}

#[test]
fn size_filters_files_only() {
    assert_eq!(search("size:>10mb"), [r"C:\Users\anna\Report 2026.PDF"]);
    assert_eq!(
        search("size:<1kb"),
        [
            r"C:\Projects\report\main.rs",
            r"C:\Users\anna\report-draft.docx",
            r"C:\Users\anna\Ärger.txt",
        ]
    );
    assert_eq!(search("size:500"), [r"C:\Users\anna\report-draft.docx"]);
}

#[test]
fn date_modified_uses_local_days() {
    assert_eq!(search("dm:today"), [r"C:\Users\anna\Report 2026.PDF"]);
    assert_eq!(search("dm:yesterday"), [r"C:\Users\anna\report-draft.docx"]);
    assert_eq!(
        search("dm:2026-01-01..2026-03-14"),
        [
            r"C:\Projects\report\main.rs",
            r"C:\Users\anna\report-draft.docx",
            r"C:\Users\anna\Ärger.txt",
        ]
    );
}

#[test]
fn invalid_values_are_reported() {
    assert!(matches!(
        Query::parse("regex:(", NOW),
        Err(QueryError::Regex { .. })
    ));
    assert!(matches!(
        Query::parse("size:>10xb", NOW),
        Err(QueryError::Size(_))
    ));
    assert!(matches!(
        Query::parse("dm:2026-02-29", NOW),
        Err(QueryError::Date(_))
    ));
}

#[test]
fn narrows_only_when_results_can_only_shrink() {
    let narrows = |new: &str, old: &str| {
        Query::parse(new, NOW)
            .unwrap()
            .narrows(&Query::parse(old, NOW).unwrap())
    };
    assert!(narrows("repo", "rep"));
    assert!(narrows("rep x", "rep"));
    assert!(narrows("file:a", "file:"));
    assert!(!narrows("rep|x", "rep"));
    assert!(!narrows("!repo", "!rep"));
    assert!(!narrows("*.pd", "*.p"));
    assert!(!narrows("rep", "rep x"));
    assert!(!narrows("case:repo", "rep"));
}

#[test]
fn searching_within_previous_results_gives_the_same_hits() {
    let index = sample();
    let cancel = AtomicBool::new(false);
    let previous = index
        .search(&Query::parse("rep", NOW).unwrap(), None, &cancel)
        .unwrap();
    let query = Query::parse("report d", NOW).unwrap();
    assert_eq!(
        index.search(&query, Some(&previous), &cancel),
        index.search(&query, None, &cancel)
    );
}

#[test]
fn cancelled_search_returns_none() {
    let index = sample();
    let query = Query::parse("report", NOW).unwrap();
    assert_eq!(index.search(&query, None, &AtomicBool::new(true)), None);
}

#[test]
fn sort_orders_by_each_key() {
    let index = sample();
    let mut ids = index
        .search(
            &Query::parse("file:", NOW).unwrap(),
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
    let names = |ids: &[u32]| ids.iter().map(|&id| index.name(id)).collect::<Vec<_>>();

    index.sort(&mut ids, SortKey::Name);
    assert_eq!(
        names(&ids),
        [
            "budget.xlsx",
            "main.rs",
            "Report 2026.PDF",
            "report-draft.docx",
            "zebra.png",
            "Ärger.txt",
            "İstanbul.jpg",
        ]
    );
    index.sort(&mut ids, SortKey::Size);
    assert_eq!(names(&ids)[..2], ["Ärger.txt", "main.rs"]);
    index.sort(&mut ids, SortKey::Modified);
    assert_eq!(names(&ids).last(), Some(&"Report 2026.PDF"));
    index.sort(&mut ids, SortKey::Path);
    assert_eq!(names(&ids)[0], "main.rs");
}

#[test]
fn parents_can_be_set_after_children() {
    let mut index = Index::new();
    let file = index.push("file.txt", None, 0, 0, 0).unwrap();
    let root = index.push("D:", None, ATTRIBUTE_DIRECTORY, 0, 0).unwrap();
    index.set_parent(file, Some(root));
    assert_eq!(index.path(file), r"D:\file.txt");
}

#[test]
fn removed_entries_disappear_from_searches() {
    let mut index = sample();
    let cancel = AtomicBool::new(false);
    let query = Query::parse(r#"path:"my docs""#, NOW).unwrap();
    let hits = index.search(&query, None, &cancel).unwrap();
    assert_eq!(hits.len(), 2);
    index.remove(hits[0]);
    assert!(index.is_removed(hits[0]));
    assert_eq!(index.search(&query, None, &cancel), Some(vec![hits[1]]));
}

#[test]
fn renamed_entries_are_found_by_their_new_name() {
    let mut index = sample();
    let cancel = AtomicBool::new(false);
    let zebra = index
        .search(&Query::parse("zebra", NOW).unwrap(), None, &cancel)
        .unwrap()[0];
    index.set_name(zebra, "Giraffe.png").unwrap();
    assert_eq!(index.path(zebra), r"C:\Projects\Giraffe.png");
    assert_eq!(
        index.search(&Query::parse("zebra", NOW).unwrap(), None, &cancel),
        Some(vec![])
    );
    assert_eq!(
        index.search(&Query::parse("giraffe", NOW).unwrap(), None, &cancel),
        Some(vec![zebra])
    );
}

#[test]
fn saved_index_loads_with_the_same_content() {
    let mut index = sample();
    index.remove(1);
    let mut file = Vec::new();
    index.save(&mut file).unwrap();
    let loaded = Index::load(&mut file.as_slice()).unwrap();

    assert_eq!(loaded.len(), index.len());
    for id in 0..index.len() as u32 {
        assert_eq!(loaded.is_removed(id), index.is_removed(id));
        if !index.is_removed(id) {
            assert_eq!(
                (
                    loaded.path(id),
                    loaded.size(id),
                    loaded.modified(id),
                    loaded.is_dir(id)
                ),
                (
                    index.path(id),
                    index.size(id),
                    index.modified(id),
                    index.is_dir(id)
                )
            );
        }
    }
    let query = Query::parse("ärger", NOW).unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        loaded.search(&query, None, &cancel),
        index.search(&query, None, &cancel)
    );
}

#[test]
fn foreign_or_damaged_files_are_rejected() {
    let mut file = Vec::new();
    sample().save(&mut file).unwrap();

    let mut other_version = file.clone();
    other_version[8] = other_version[8].wrapping_add(1);
    assert!(matches!(
        Index::load(&mut other_version.as_slice()),
        Err(LoadError::Version { .. })
    ));

    let truncated = &file[..file.len() - 1];
    assert!(matches!(
        Index::load(&mut &truncated[..]),
        Err(LoadError::Damaged(_))
    ));

    // A name offset pointing into the middle of 'Ä' must not be accepted.
    let mut bad_offset = file.clone();
    let names_len = u64::from_le_bytes(file[20..28].try_into().unwrap()) as usize;
    let first_entry = 28 + names_len;
    let entry_with_umlaut = first_entry + 5 * 32;
    let offset = u32::from_le_bytes(
        bad_offset[entry_with_umlaut..entry_with_umlaut + 4]
            .try_into()
            .unwrap(),
    );
    bad_offset[entry_with_umlaut..entry_with_umlaut + 4]
        .copy_from_slice(&(offset + 1).to_le_bytes());
    assert!(matches!(
        Index::load(&mut bad_offset.as_slice()),
        Err(LoadError::Damaged(_))
    ));

    assert!(matches!(
        Index::load(&mut &b"not an index at all"[..]),
        Err(LoadError::Damaged(_) | LoadError::Io(_))
    ));
}

#[test]
fn compaction_keeps_live_entries_and_drops_dead_ones() {
    let mut index = sample();
    let cancel = AtomicBool::new(false);
    let find = |index: &Index, query: &str| {
        let hits = index
            .search(&Query::parse(query, NOW).unwrap(), None, &cancel)
            .unwrap();
        let mut paths: Vec<String> = hits.iter().map(|&id| index.path(id)).collect();
        paths.sort();
        paths
    };
    let docs = index
        .search(
            &Query::parse("folder:\"my docs\"", NOW).unwrap(),
            None,
            &cancel,
        )
        .unwrap()[0];
    let zebra = index
        .search(&Query::parse("zebra", NOW).unwrap(), None, &cancel)
        .unwrap()[0];
    index.remove(docs);
    index.set_name(zebra, "Giraffe.png").unwrap();
    let before = find(&index, "");

    let remap = index.compact();

    // "My Docs" is removed and budget.xlsx below it goes with it. Before the
    // compaction the orphan was still found, with a path lacking the drive.
    assert_eq!(index.len(), 13 - 2);
    assert_eq!(remap[docs as usize], None);
    assert!(before.contains(&r"My Docs\budget.xlsx".to_owned()));
    let expected: Vec<String> = before
        .into_iter()
        .filter(|path| path.starts_with("C:"))
        .collect();
    assert_eq!(find(&index, ""), expected);
    let new_zebra = remap[zebra as usize].unwrap();
    assert_eq!(index.path(new_zebra), r"C:\Projects\Giraffe.png");
    assert_eq!(index.removed_count(), 0);
}
