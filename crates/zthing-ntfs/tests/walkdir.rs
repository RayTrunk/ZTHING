#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use walkdir::WalkDir;
use zthing_core::Index;
use zthing_ntfs::NtfsVolume;

/// (path relative to the test folder, is folder, size, modified). Folders carry no size or date.
type Entry = (String, bool, u64, i64);

fn walk(base: &Path) -> Vec<Entry> {
    let mut entries: Vec<Entry> = WalkDir::new(base)
        .min_depth(1)
        .into_iter()
        .map(|entry| {
            let entry = entry.unwrap();
            let meta = entry.metadata().unwrap();
            let relative = entry.path().strip_prefix(base).unwrap();
            let modified = meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap();
            let (size, modified) = if meta.is_dir() {
                (0, 0)
            } else {
                (meta.len(), modified.as_secs() as i64)
            };
            (
                relative.display().to_string(),
                meta.is_dir(),
                size,
                modified,
            )
        })
        .collect();
    entries.sort();
    entries
}

#[test]
#[ignore = "needs administrator rights"]
fn mft_matches_walkdir() {
    let base =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("walkdir-{}", std::process::id()));
    for dir in ["a/b/Ärger", "leer", "İstanbul"] {
        fs::create_dir_all(base.join(dir)).unwrap();
    }
    for (file, len) in [
        ("x.txt", 0),
        ("a/y.bin", 5000),
        ("a/b/z.pdf", 123),
        ("a/b/Ärger/ü ö.txt", 1),
        ("İstanbul/big.dat", 3 << 20),
    ] {
        fs::write(base.join(file), vec![7u8; len]).unwrap();
    }
    // walkdir lists every hard link, so the index must hold both names.
    fs::hard_link(base.join("a/y.bin"), base.join("leer/y-link.bin")).unwrap();
    let expected = walk(&base);

    let drive = base
        .to_str()
        .unwrap()
        .chars()
        .next()
        .unwrap()
        .to_ascii_uppercase();
    let volume = NtfsVolume::open(drive).unwrap();
    let mut index = Index::new();
    volume.read(&mut index, 0).unwrap();

    let prefix = format!("{}\\", base.display()).to_lowercase();
    let mut actual: Vec<Entry> = (0..index.len() as u32)
        .filter(|&id| !index.is_removed(id))
        .filter_map(|id| {
            let path = index.path(id);
            // The prefix is ASCII, so its byte length is the same in both cases.
            let relative = path
                .to_lowercase()
                .starts_with(&prefix)
                .then(|| &path[prefix.len()..])?;
            let (size, modified) = if index.is_dir(id) {
                (0, 0)
            } else {
                (index.size(id), index.modified(id))
            };
            Some((relative.to_owned(), index.is_dir(id), size, modified))
        })
        .collect();
    actual.sort();
    fs::remove_dir_all(&base).unwrap();

    assert_eq!(actual, expected);
}
