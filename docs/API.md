# ZTHING API

Every way into ZTHING asks the same running index, kept by the ZTHING service.
None of them builds an index of its own.

| Way | For | Needs |
|---|---|---|
| [Named pipe](#named-pipe-protocol) `\\.\pipe\zthing` | any language that can open a file | the service |
| [`zthing-client`](#rust-zthing-client) | Rust | the service |
| [`zthing.dll`](#c-abi-zthingdll) | C, C++, C# (P/Invoke), Python (ctypes) | the service |
| [Python package `zthing`](#python-package) | Python, no DLL | the service |
| [`zt --json`](#command-line-zt) | scripts | the service |
| [HTTP API](#http-api) | anything that speaks HTTP | the service and the ZTHING window, opt-in |
| [URL protocol](#url-protocol-zthing) `zthing:<query>` | links, other programs | the ZTHING window |

Examples that build and run: [`examples/c`](../examples/c/search.c),
[`examples/csharp`](../examples/csharp/Program.cs),
[`examples/python`](../examples/python/search.py).

## Query syntax

The same syntax everywhere, also in the search window.

| Input | Matches |
|---|---|
| `foo bar` | names containing `foo` and `bar` (space = AND) |
| `foo\|bar` | names containing `foo` or `bar`. `\|` binds tighter than a space: `a b\|c` is `a AND (b OR c)` |
| `!foo` | names not containing `foo` |
| `"my file"` | the phrase including the space |
| `*.iso`, `report??.pdf` | wildcards, matched against the whole name |
| `regex:^IMG_\d+\.jpe?g$` | a regular expression ([`regex` crate syntax](https://docs.rs/regex/latest/regex/#syntax)) |
| `ext:pdf;docx` | file extensions |
| `path:windows\system32` | the term is matched against the full path instead of the name |
| `file:` / `folder:` | only files / only folders, e.g. `folder:backup` |
| `size:>10mb`, `size:<=4kb`, `size:1gb` | size with `>`, `>=`, `<`, `<=` or `=`; units b, kb, mb, gb, tb (1024-based) |
| `dm:today`, `dm:yesterday`, `dm:2026-01-01`, `dm:2026-01-01..2026-03-31` | date modified, local calendar days |
| `case:Readme` | case-sensitive |

Matching ignores case unless `case:` is given. Modifiers combine: `!path:temp\`,
`case:path:Src`.

## Named pipe protocol

The service listens on `\\.\pipe\zthing`. Each request and each response is one
JSON object on one line, UTF-8, terminated by `\n` (JSON Lines). A connection
takes any number of requests in turn; the service answers each before reading
the next.

Only interactively logged-on local users may connect, network clients are
rejected. With `filter_by_user_access` (default on, see [Security](#security))
every answer only contains what the connecting user may list.

### Requests

```json
{"v": 1, "id": 7, "cmd": "search", "query": "*.iso", "sort": "size", "descending": true, "offset": 0, "limit": 100, "fields": ["path", "size"]}
```

| Field | Meaning |
|---|---|
| `v` | protocol version, currently `1` |
| `id` | any number chosen by the client, repeated in the response |
| `cmd` | `search`, `count`, `status` or `version` |

**`search`**: one page of matches.

| Field | Default | Meaning |
|---|---|---|
| `query` | `""` | query in the syntax above; empty matches everything |
| `sort` | `"name"` | `name`, `path`, `size` or `modified` (name and path ignore case) |
| `descending` | `false` | |
| `offset` | `0` | matches to skip |
| `limit` | `100` | matches to return, at most 10 000 per request |
| `fields` | all | any of `name`, `path`, `size`, `modified`, `folder` |

Paging: request `offset` 0, 10 000, 20 000 … with the same query and sort. The
service keeps the sorted result of the last search per connection, so further
pages of the same search cost no new search.

**`count`**: `{"v":1,"id":2,"cmd":"count","query":"ext:pdf"}`, returns `total` only.

**`status`** and **`version`** take no further fields.

### Responses

One flat object for every command. Fields a command does not produce are absent.

```json
{"v":1,"id":7,"ok":true,"total":78,"millis":4.2,"results":[{"path":"C:\\Users\\me\\Downloads\\big.iso","size":8489097216}]}
```

| Field | Present for | Meaning |
|---|---|---|
| `v`, `id`, `ok` | all | `ok` is `false` if the request failed |
| `error` | failures | message for people, e.g. `invalid regex "(": …` |
| `total` | `search`, `count` | all matches, before paging |
| `millis` | `search` | time the service spent on this request |
| `results` | `search` | the page, each hit with the requested fields |
| `status` | `status` | see below |
| `version`, `protocol` | `version` | service version such as `"0.88.0"` and protocol version |

A hit holds the requested fields: `name`, `path` (full path including the name),
`size` (bytes, 0 for folders), `modified` (Unix seconds, UTC) and `folder` (bool).
Hard links of one file are separate hits with the same size and date.

`status`:

```json
{"ready":true,"index_version":1215,"entries":3845331,"volumes":[{"drive":"C","live":true}],"notes":[]}
```

| Field | Meaning |
|---|---|
| `ready` | `false` while the service reads the drives for the first time; `search` and `count` fail until then |
| `index_version` | changes whenever the index changes; poll it to know when to search again |
| `entries` | entries in the index, including hard links |
| `volumes` | indexed drives; `live` is true if the USN journal keeps the drive current |
| `notes` | skipped drives and other problems, as text |

### Versions

The protocol version rises whenever a command or field changes incompatibly.
New optional fields do not raise it, so clients should ignore fields they do not
know. A request with another version fails with an `error` naming the version
the service speaks.

## Rust: `zthing-client`

The crate used by the window and by `zt`, so the public interface is the internal one.

```rust
use zthing_client::{Client, Search, Sort};

let mut client = Client::connect()?;
let page = client.search(Search {
    query: "*.iso".into(),
    sort: Sort::Size,
    descending: true,
    limit: 10,
    ..Search::default()
})?;
println!("{} matches", page.total);
for hit in page.hits {
    println!("{:?}", hit.path);
}
let pdfs = client.count("ext:pdf")?;
let status = client.status()?;
```

`ClientError::Service` carries an error message from the service, such as a bad
query. Other variants mean the connection failed; connect again.

## C ABI: `zthing.dll`

Built with `cargo build --release -p zthing-ffi` into `target/release/zthing.dll`
with the import library `zthing.dll.lib`. The header is
[`crates/zthing-ffi/include/zthing.h`](../crates/zthing-ffi/include/zthing.h).

```c
Zthing *zt = zthing_connect();
zthing_set_query(zt, "*.iso");
zthing_set_sort(zt, ZTHING_SORT_SIZE, 1);
zthing_set_range(zt, 0, 10);
if (zthing_execute(zt) == ZTHING_OK) {
    for (uint64_t i = 0; i < zthing_result_count(zt); i++)
        printf("%llu %s\n", zthing_result_size(zt, i), zthing_result_path(zt, i));
} else {
    fprintf(stderr, "%s\n", zthing_last_error(zt));
}
zthing_free(zt);
```

| Function | Purpose |
|---|---|
| `Zthing *zthing_connect(void)` | Creates a handle and connects. Never NULL; if the service is unreachable, `zthing_last_error` says why and `zthing_execute` retries. |
| `void zthing_free(Zthing *)` | Closes the connection and frees all strings of the handle. |
| `int32_t zthing_set_query(Zthing *, const char *utf8)` | Sets the query. |
| `int32_t zthing_set_query_w(Zthing *, const uint16_t *utf16)` | The same for UTF-16 (`wchar_t`). |
| `int32_t zthing_set_sort(Zthing *, int32_t sort, int32_t descending)` | `ZTHING_SORT_NAME`, `_PATH`, `_SIZE`, `_MODIFIED`. Default: name, ascending. |
| `int32_t zthing_set_range(Zthing *, uint64_t offset, uint64_t limit)` | Default: 0 and 1000. `UINT64_MAX` returns every match; the DLL fetches the pages. |
| `int32_t zthing_execute(Zthing *)` | Runs the query and keeps the results in the handle. |
| `uint64_t zthing_total(Zthing *)` | All matches of the last query, also those outside the range. |
| `uint64_t zthing_result_count(Zthing *)` | Results held by the handle. |
| `zthing_result_name` / `_name_w`, `zthing_result_path` / `_path_w` | Name and full path as UTF-8 or UTF-16, NULL for an index out of range. |
| `uint64_t zthing_result_size(Zthing *, uint64_t)` | Bytes. |
| `int64_t zthing_result_modified(Zthing *, uint64_t)` | Unix seconds (UTC). |
| `int32_t zthing_result_is_folder(Zthing *, uint64_t)` | 1 for folders. |
| `zthing_last_error` / `zthing_last_error_w` | Why the last call failed, empty after success. Never NULL. |

Return codes: `ZTHING_OK` (0), `ZTHING_ERROR_SERVICE` (1, service unreachable or
connection broken), `ZTHING_ERROR_QUERY` (2, the service rejected the query),
`ZTHING_ERROR_ARGUMENT` (3).

Strings returned by the DLL belong to the handle and stay valid until the next
`zthing_execute` or `zthing_free` on it; do not free them. A handle must not be
used by two threads at once; use one handle per thread.

C# declares the functions with `[DllImport("zthing")]` and reads the UTF-16
getters with `Marshal.PtrToStringUni` (see `examples/csharp`). Python can load
the DLL with `ctypes`:

```python
import ctypes

dll = ctypes.CDLL("zthing.dll")
dll.zthing_connect.restype = ctypes.c_void_p
dll.zthing_set_query_w.argtypes = [ctypes.c_void_p, ctypes.c_wchar_p]
dll.zthing_execute.argtypes = [ctypes.c_void_p]
dll.zthing_result_count.argtypes = [ctypes.c_void_p]
dll.zthing_result_count.restype = ctypes.c_uint64
dll.zthing_result_path_w.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
dll.zthing_result_path_w.restype = ctypes.c_wchar_p
dll.zthing_free.argtypes = [ctypes.c_void_p]

zt = dll.zthing_connect()
dll.zthing_set_query_w(zt, "*.iso")
if dll.zthing_execute(zt) == 0:
    print([dll.zthing_result_path_w(zt, i) for i in range(dll.zthing_result_count(zt))])
dll.zthing_free(zt)
```

The pure Python package below is simpler and needs no DLL.

## Python package

[`python/`](../python/README.md), standard library only, talks to the pipe directly.

```
pip install ./python
```

```python
import zthing

with zthing.Client() as zt:
    page = zt.search("*.iso", sort="size", descending=True, limit=5)
    for hit in page.hits:
        print(hit["size"], hit["path"])
    every_pdf = [hit["path"] for hit in zt.search_all("ext:pdf", fields=["path"])]
```

## Command line: `zt`

```
zt [--json] [--count] [--limit N] [--sort name|path|size|dm] <query>
```

Prints one path per line, or with `--json` an array of hits with `path`, `size`,
`modified` and `folder`. Exit code 0 with matches, 1 without, 2 on errors.

```powershell
zt --json --sort size --limit 5 "*.iso" | ConvertFrom-Json
```

## HTTP API

Off by default. It runs inside the ZTHING window, not in the service: an HTTP
request carries no Windows identity, but passed on through the pipe by the window
it gets the rights of the logged-on user, so the access filter still applies. It
is served while the window runs, also while it is hidden in the tray.

Enable it in `%APPDATA%\ZTHING\config.toml` and restart the window:

```toml
[http]
enabled = true
port = 8732
token = "a long random string"
```

It binds to `127.0.0.1` only and refuses to start with an empty token. Send the
token as `Authorization: Bearer <token>` or as the parameter `token=<token>`.
Responses are JSON in the shape of the pipe responses.

**`GET /search`**, parameters as in the pipe request: `q` (required), `sort`,
`descending` (`1` or `true`), `offset`, `limit`, `fields` (comma-separated).
Values are percent-encoded.

```
curl -H "Authorization: Bearer TOKEN" "http://127.0.0.1:8732/search?q=ext%3Apdf+report&sort=modified&descending=1&limit=10&fields=path,modified"
```

```json
{"v":1,"id":0,"ok":true,"total":7,"millis":12.4,"results":[{"path":"C:\\…\\report.pdf","modified":1605006364}]}
```

**`GET /status`** returns `{"v":1,"id":0,"ok":true,"status":{…}}` as described above.

| Status | Meaning |
|---|---|
| 200 | success |
| 400 | bad parameter or query, `error` says which |
| 401 | missing or wrong token |
| 404 | unknown path |
| 405 | method other than GET |
| 503 | the service is unreachable |

## URL protocol `zthing:`

`zthing:<query>` opens the search window with this query. A running window comes
to the front and takes over the query; otherwise the window starts. The query may
be percent-encoded, as browsers do: `zthing:ext%3Apdf%20invoice`.

The installer registers the protocol for the current user:

```
HKEY_CURRENT_USER\Software\Classes\zthing
    (Default)       = "URL:ZTHING search"
    URL Protocol    = ""
    shell\open\command
        (Default)   = "C:\Program Files\ZTHING\zthing-gui.exe" "%1"
```

`zthing-gui.exe <query>` without the scheme works the same, for example from a script.

## Security

The service runs as LocalSystem and knows the names of every user's files.

- The pipe admits SYSTEM, administrators and interactively logged-on local users.
  Network logons are denied and remote pipe clients are rejected.
- With `filter_by_user_access = true` in the service settings (default) a hit is
  only returned if the connecting user may list the folder containing it. The
  service checks this while impersonating the pipe client.
- The HTTP API listens on 127.0.0.1 only, needs a token and runs with the rights
  of the user whose window serves it.
