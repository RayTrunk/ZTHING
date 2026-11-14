//! Measures indexing and searching one volume:
//! `cargo run --release -p zthing-ntfs --example index_volume -- C report`

use std::error::Error;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use zthing_core::{Index, Now, Query};
use zthing_ntfs::NtfsVolume;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let drive = args.next().and_then(|a| a.chars().next()).unwrap_or('C');
    let query = args.next().unwrap_or_else(|| "report".to_owned());

    let start = Instant::now();
    let volume = NtfsVolume::open(drive)?;
    let mut index = Index::new();
    let records = volume.read(&mut index)?;
    index.shrink_to_fit();
    let indexed = start.elapsed();

    let now = Now {
        unix: 0,
        utc_offset: 0,
    };
    let cancel = AtomicBool::new(false);
    for input in [
        query.as_str(),
        "*.dll",
        "path:windows\\system32",
        "ext:jpg;png;gif",
    ] {
        let query = Query::parse(input, now)?;
        // The first path: search also builds the folder path cache.
        let first = Instant::now();
        let hits = index
            .search(&query, None, &cancel)
            .map_or(0, |hits| hits.len());
        let first = first.elapsed();
        let again = Instant::now();
        index.search(&query, None, &cancel);
        println!(
            "{input:?}: {hits} hits, {:.1} ms, repeated {:.1} ms",
            first.as_secs_f64() * 1e3,
            again.elapsed().as_secs_f64() * 1e3
        );
    }

    let mib = |bytes: usize| bytes as f64 / f64::from(1 << 20);
    println!(
        "{drive}: {} entries, indexed in {:.2} s, index (after path cache) {:.1} MiB, record map {:.1} MiB",
        index.len(),
        indexed.as_secs_f64(),
        mib(index.heap_size()),
        mib(records.heap_size()),
    );
    Ok(())
}
