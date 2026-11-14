#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use zthing_core::{Now, Query};
use zthing_ntfs::Indexer;

const DEADLINE: Duration = Duration::from_secs(1);

fn scratch(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{test}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("files/sub")).unwrap();
    dir
}

fn drive(dir: &Path) -> char {
    dir.to_str()
        .unwrap()
        .chars()
        .next()
        .unwrap()
        .to_ascii_uppercase()
}

/// Path and size of the live entry with exactly this name.
fn find(indexer: &Indexer, name: &str) -> Option<(String, u64)> {
    let now = Now {
        unix: 0,
        utc_offset: 0,
    };
    let query = Query::parse(&format!("regex:\"^{}$\"", name.replace('.', r"\.")), now).unwrap();
    let index = indexer.index();
    let hits = index.search(&query, None, &AtomicBool::new(false)).unwrap();
    hits.first().map(|&id| (index.path(id), index.size(id)))
}

/// Polls the journal until `check` holds and returns how long that took.
fn wait_for(step: &str, indexer: &mut Indexer, check: impl Fn(&Indexer) -> bool) -> Duration {
    let start = Instant::now();
    loop {
        indexer.update().unwrap();
        if check(indexer) {
            return start.elapsed();
        }
        assert!(
            start.elapsed() < DEADLINE,
            "{step} not visible after {DEADLINE:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "needs administrator rights"]
fn changes_appear_within_a_second() {
    let dir = scratch("journal-live");
    let files = dir.join("files");
    let (mut indexer, _) = Indexer::open(&dir.join("index.bin"), &[drive(&dir)]).unwrap();
    let a = format!("zthing-live-{}-a.txt", std::process::id());
    let b = format!("zthing-live-{}-b.txt", std::process::id());

    fs::write(files.join(&a), b"0123456789").unwrap();
    let created = wait_for("create", &mut indexer, |i| {
        find(i, &a).is_some_and(|(_, size)| size == 10)
    });

    fs::rename(files.join(&a), files.join(&b)).unwrap();
    let renamed = wait_for("rename", &mut indexer, |i| {
        find(i, &a).is_none() && find(i, &b).is_some()
    });

    fs::rename(files.join(&b), files.join("sub").join(&b)).unwrap();
    let moved = wait_for("move", &mut indexer, |i| {
        find(i, &b).is_some_and(|(path, _)| path.ends_with(&format!(r"\files\sub\{b}")))
    });

    fs::write(files.join("sub").join(&b), vec![1u8; 5000]).unwrap();
    let resized = wait_for("resize", &mut indexer, |i| {
        find(i, &b).is_some_and(|(_, size)| size == 5000)
    });

    fs::remove_file(files.join("sub").join(&b)).unwrap();
    let deleted = wait_for("delete", &mut indexer, |i| find(i, &b).is_none());

    eprintln!(
        "latency: create {created:?}, rename {renamed:?}, move {moved:?}, resize {resized:?}, delete {deleted:?}"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[ignore = "needs administrator rights"]
fn saved_index_is_replayed_or_rebuilt() {
    let dir = scratch("journal-saved");
    let path = dir.join("index.bin");
    let drives = [drive(&dir)];
    let name = format!("zthing-saved-{}.txt", std::process::id());

    let (mut indexer, report) = Indexer::open(&path, &drives).unwrap();
    assert!(report.rebuilt.is_some());
    assert!(indexer.needs_save());
    indexer.save(&path).unwrap();
    drop(indexer);

    // Created while no indexer runs, so only the journal can tell.
    fs::write(dir.join("files").join(&name), b"x").unwrap();
    let start = Instant::now();
    let (mut indexer, report) = Indexer::open(&path, &drives).unwrap();
    let reopened = start.elapsed();
    assert_eq!(report.rebuilt, None);
    assert!(find(&indexer, &name).is_some());
    indexer.save(&path).unwrap();
    eprintln!("open of a saved index with replay: {reopened:?}");

    let mut file = fs::read(&path).unwrap();
    // The journal id follows magic (8), version (4), volume count (4), drive (4),
    // serial (8) and the journal flag (1).
    file[29] ^= 0xFF;
    fs::write(&path, &file).unwrap();
    let (_, report) = Indexer::open(&path, &drives).unwrap();
    assert!(report.rebuilt.unwrap().contains("recreated"));

    file[8] = file[8].wrapping_add(1);
    fs::write(&path, &file).unwrap();
    let (_, report) = Indexer::open(&path, &drives).unwrap();
    assert!(report.rebuilt.unwrap().contains("version"));

    fs::remove_dir_all(&dir).unwrap();
}

/// Paths of all live entries named `name`, sorted.
fn paths(indexer: &Indexer, name: &str) -> Vec<String> {
    let now = Now {
        unix: 0,
        utc_offset: 0,
    };
    let query = Query::parse(&format!("regex:\"^{}$\"", name.replace('.', r"\.")), now).unwrap();
    let index = indexer.index();
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
#[ignore = "needs administrator rights"]
fn every_hard_link_is_an_entry() {
    let dir = scratch("journal-links");
    let files = dir.join("files");
    let a = format!("zthing-link-{}-a.txt", std::process::id());
    let b = format!("zthing-link-{}-b.txt", std::process::id());
    let at = |name: &str| files.join(name).to_string_lossy().into_owned();
    let in_sub = |name: &str| files.join("sub").join(name).to_string_lossy().into_owned();
    // `paths` sorts, and "sub" sorts before the test file names.
    let both = |name: &str| vec![in_sub(name), at(name)];

    // Linked before the volume is read, so the $MFT holds both names.
    fs::write(files.join(&a), b"0123").unwrap();
    fs::hard_link(files.join(&a), files.join("sub").join(&a)).unwrap();
    let (mut indexer, _) = Indexer::open(&dir.join("index.bin"), &[drive(&dir)]).unwrap();
    assert_eq!(paths(&indexer, &a), both(&a));
    assert!(find(&indexer, &a).is_some_and(|(_, size)| size == 4));

    fs::hard_link(files.join(&a), files.join(&b)).unwrap();
    let added = wait_for("add link", &mut indexer, |i| {
        paths(i, &b) == [at(&b)] && paths(i, &a).len() == 2
    });

    fs::rename(files.join("sub").join(&a), files.join("sub").join(&b)).unwrap();
    let renamed = wait_for("rename link", &mut indexer, |i| {
        paths(i, &a) == [at(&a)] && paths(i, &b) == both(&b)
    });

    fs::write(files.join(&a), vec![7u8; 3000]).unwrap();
    let resized = wait_for("resize", &mut indexer, |i| {
        paths(i, &b).len() == 2
            && find(i, &b).is_some_and(|(_, size)| size == 3000)
            && find(i, &a).is_some_and(|(_, size)| size == 3000)
    });

    fs::remove_file(files.join(&a)).unwrap();
    let removed = wait_for("remove link", &mut indexer, |i| {
        paths(i, &a).is_empty() && paths(i, &b).len() == 2
    });

    fs::remove_file(files.join(&b)).unwrap();
    fs::remove_file(files.join("sub").join(&b)).unwrap();
    let deleted = wait_for("delete", &mut indexer, |i| paths(i, &b).is_empty());

    eprintln!(
        "latency: add {added:?}, rename {renamed:?}, resize {resized:?}, remove {removed:?}, delete {deleted:?}"
    );
    fs::remove_dir_all(&dir).unwrap();
}
