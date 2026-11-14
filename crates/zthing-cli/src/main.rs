//! `zt <query>` searches all local NTFS volumes. Until the service of milestone 5
//! holds the index, zt reads the volumes itself and needs administrator rights.

use std::io::{self, BufWriter, Write};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use zthing_core::{Index, Now, Query, SortKey};
use zthing_ntfs::NtfsVolume;

const USAGE: &str = "usage: zt [--json] [--count] [--limit N] [--sort name|path|size|dm] <query>";

struct Options {
    query: String,
    json: bool,
    count: bool,
    limit: Option<usize>,
    sort: SortKey,
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("zt: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Returns whether anything matched.
fn run() -> Result<bool> {
    let Some(options) = parse_args()? else {
        println!("{USAGE}");
        return Ok(true);
    };
    let now = Now {
        unix: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
        utc_offset: i64::from(chrono::Local::now().offset().local_minus_utc()),
    };
    let query = Query::parse(&options.query, now)?;

    let mut index = Index::new();
    let mut indexed = 0;
    for drive in zthing_ntfs::fixed_drives() {
        let result = NtfsVolume::open(drive).and_then(|volume| volume.read(&mut index));
        match result {
            Ok(_) => indexed += 1,
            Err(e) => eprintln!("zt: skipped {e}"),
        }
    }
    if indexed == 0 {
        bail!("no volume could be indexed, run zt from a terminal with administrator rights");
    }
    index.shrink_to_fit();

    // `None` only signals cancellation, which this flag never requests.
    let mut hits = index
        .search(&query, None, &AtomicBool::new(false))
        .unwrap_or_default();
    if options.count {
        println!("{}", hits.len());
        return Ok(!hits.is_empty());
    }
    index.sort(&mut hits, options.sort);
    hits.truncate(options.limit.unwrap_or(hits.len()));

    let mut out = BufWriter::new(io::stdout().lock());
    if options.json {
        // Written entry by entry, so a large result never exists as one JSON value.
        writeln!(out, "[")?;
        for (i, &id) in hits.iter().enumerate() {
            let entry = serde_json::json!({
                "path": index.path(id),
                "folder": index.is_dir(id),
                "size": index.size(id),
                "modified": index.modified(id),
            });
            let comma = if i + 1 < hits.len() { "," } else { "" };
            writeln!(out, "  {entry}{comma}")?;
        }
        writeln!(out, "]")?;
    } else {
        for &id in &hits {
            writeln!(out, "{}", index.path(id))?;
        }
    }
    out.flush()?;
    Ok(!hits.is_empty())
}

/// Returns `None` for `--help`.
fn parse_args() -> Result<Option<Options>> {
    let mut options = Options {
        query: String::new(),
        json: false,
        count: false,
        limit: None,
        sort: SortKey::Name,
    };
    let mut terms = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--json" => options.json = true,
            "--count" => options.count = true,
            "--limit" => {
                let n = args.next().context("--limit needs a number")?;
                let n = n
                    .parse()
                    .with_context(|| format!("--limit needs a number, got {n:?}"))?;
                options.limit = Some(n);
            }
            "--sort" => {
                options.sort = match args.next().as_deref() {
                    Some("name") => SortKey::Name,
                    Some("path") => SortKey::Path,
                    Some("size") => SortKey::Size,
                    Some("dm") => SortKey::Modified,
                    other => bail!("--sort expects name, path, size or dm, got {other:?}"),
                }
            }
            _ => match arg.find(char::is_whitespace) {
                // The shell already removed the quotes of `zt "my docs"` or `zt path:"my docs"`,
                // so the value after the last modifier is quoted again to stay one phrase.
                Some(space) if !arg.contains('"') => {
                    let value_start = arg[..space]
                        .rfind(':')
                        .map_or(usize::from(arg.starts_with('!')), |colon| colon + 1);
                    terms.push(format!(
                        "{}\"{}\"",
                        &arg[..value_start],
                        &arg[value_start..]
                    ));
                }
                _ => terms.push(arg),
            },
        }
    }
    if terms.is_empty() {
        bail!("{USAGE}");
    }
    options.query = terms.join(" ");
    Ok(Some(options))
}
