//! `zt <query>` searches through the ZTHING service, which needs to run.

use std::io::{self, BufWriter, Write};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use zthing_client::{Client, Field, Search, Sort};

const USAGE: &str = "usage: zt [--json] [--count] [--limit N] [--sort name|path|size|dm] <query>";
/// Results fetched per request. The service caps a page at the same size.
const PAGE: u64 = 10_000;

struct Options {
    query: String,
    json: bool,
    count: bool,
    limit: Option<u64>,
    sort: Sort,
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
    let mut client = Client::connect()?;
    if options.count {
        let total = client.count(&options.query)?;
        println!("{total}");
        return Ok(total > 0);
    }

    let wanted = options.limit.unwrap_or(u64::MAX);
    let fields = if options.json {
        vec![Field::Path, Field::Size, Field::Modified, Field::Folder]
    } else {
        vec![Field::Path]
    };
    let mut out = BufWriter::new(io::stdout().lock());
    if options.json {
        writeln!(out, "[")?;
    }
    let mut offset = 0;
    let total = loop {
        let page = client.search(Search {
            query: options.query.clone(),
            sort: options.sort,
            offset,
            limit: (wanted - offset).min(PAGE),
            fields: fields.clone(),
            ..Search::default()
        })?;
        for hit in &page.hits {
            if options.json {
                // Written entry by entry, so a large result never exists as one JSON value.
                let separator = if offset == 0 { "" } else { ",\n" };
                write!(out, "{separator}  {}", serde_json::to_string(hit)?)?;
            } else {
                writeln!(out, "{}", hit.path.as_deref().unwrap_or_default())?;
            }
            offset += 1;
        }
        if page.hits.is_empty() || offset >= page.total.min(wanted) {
            break page.total;
        }
    };
    if options.json {
        writeln!(out, "\n]")?;
    }
    out.flush()?;
    Ok(total > 0)
}

/// Returns `None` for `--help`.
fn parse_args() -> Result<Option<Options>> {
    let mut options = Options {
        query: String::new(),
        json: false,
        count: false,
        limit: None,
        sort: Sort::Name,
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
                    Some("name") => Sort::Name,
                    Some("path") => Sort::Path,
                    Some("size") => Sort::Size,
                    Some("dm") => Sort::Modified,
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
