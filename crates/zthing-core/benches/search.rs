//! Search timings on 1M synthetic entries: `cargo bench -p zthing-core`.
#![allow(clippy::unwrap_used)]

use std::hint::black_box;
use std::sync::atomic::AtomicBool;

use criterion::{Criterion, criterion_group, criterion_main};
use zthing_core::{ATTRIBUTE_DIRECTORY, Index, Now, Query};

const ENTRIES: usize = 1_000_000;
const FOLDERS: usize = 50_000;
const NOW: Now = Now {
    unix: 1_773_576_000,
    utc_offset: 3600,
};
const WORDS: &[&str] = &[
    "report",
    "invoice",
    "photo",
    "backup",
    "project",
    "draft",
    "final",
    "notes",
    "budget",
    "meeting",
    "Übersicht",
    "Rechnung",
    "config",
    "setup",
    "readme",
    "archive",
    "summer",
    "winter",
    "client",
    "server",
    "data",
    "image",
    "video",
    "music",
    "source",
    "build",
    "test",
    "release",
    "old",
    "new",
];
const EXTS: &[&str] = &[
    "pdf", "docx", "xlsx", "jpg", "png", "txt", "rs", "toml", "mp4", "mp3", "zip", "exe", "dll",
    "json",
];

fn synthetic_index() -> Index {
    // xorshift64 keeps the data identical between runs without a rand dependency.
    let mut state = 0x9E37_79B9_7F4A_7C15_u64;
    let mut next = move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };
    let mut index = Index::new();
    let mut folders = vec![index.push("C:", None, ATTRIBUTE_DIRECTORY, 0, 0).unwrap()];
    for i in 1..ENTRIES {
        let parent = Some(folders[next(folders.len())]);
        let (a, b) = (WORDS[next(WORDS.len())], WORDS[next(WORDS.len())]);
        if i < FOLDERS {
            let id = index.push(&format!("{a} {b}"), parent, ATTRIBUTE_DIRECTORY, 0, 0);
            folders.push(id.unwrap());
        } else {
            let name = format!("{a}_{b}_{}.{}", next(10_000), EXTS[next(EXTS.len())]);
            let modified = NOW.unix - next(3 * 365) as i64 * 86_400;
            index
                .push(&name, parent, 0, next(100 << 20) as u64, modified)
                .unwrap();
        }
    }
    index
}

fn bench(c: &mut Criterion) {
    let index = synthetic_index();
    let cancel = AtomicBool::new(false);
    eprintln!(
        "{} entries, index heap {:.1} MiB",
        index.len(),
        index.heap_size() as f64 / f64::from(1 << 20)
    );

    let mut group = c.benchmark_group("search_1m");
    for input in [
        "r",
        "report",
        "Übersicht",
        "report final 42",
        "invoice|budget",
        "!report",
        "*.pdf",
        "ext:pdf;docx",
        r"regex:^inv.*_9\d{3}\.",
        "path:summer",
        "path:summer report",
        "case:Rechnung",
        "size:>50mb dm:2026-01-01..2026-03-31",
    ] {
        let query = Query::parse(input, NOW).unwrap();
        let hits = index.search(&query, None, &cancel).unwrap().len();
        eprintln!("{input:?}: {hits} hits");
        group.bench_function(input, |b| {
            b.iter(|| index.search(black_box(&query), None, &cancel))
        });
    }
    let previous = index
        .search(&Query::parse("rep", NOW).unwrap(), None, &cancel)
        .unwrap();
    let narrower = Query::parse("repo", NOW).unwrap();
    group.bench_function("repo within rep", |b| {
        b.iter(|| index.search(black_box(&narrower), Some(&previous), &cancel))
    });
    group.finish();

    let mut group = c.benchmark_group("build");
    group.sample_size(10);
    group.bench_function("synthetic_1m", |b| b.iter(synthetic_index));
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
