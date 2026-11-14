# zthing for Python

Searches file names through the ZTHING service on Windows. Pure Python, standard
library only: the package talks to the service pipe `\.\pipe\zthing` directly and
needs no DLL. The service must be installed and running.

```
pip install ./python
```

## Example

```python
import zthing

with zthing.Client() as zt:
    page = zt.search("*.iso", sort="size", descending=True, limit=5)
    print(page.total, "matches in", page.millis, "ms")
    for hit in page.hits:
        print(hit["size"], hit["path"])

    # Every match, fetched page by page.
    paths = [hit["path"] for hit in zt.search_all("ext:pdf", fields=["path"])]

    print(zt.count("report"))
    print(zt.status()["entries"])
```

## API

| Call | Returns |
|---|---|
| `Client()` | A connection. Raises `zthing.Error` if the service is not running. |
| `search(query, sort="name", descending=False, offset=0, limit=100, fields=FIELDS)` | `Page` with `total`, `millis` and `hits` |
| `search_all(query, sort="name", descending=False, fields=FIELDS)` | Generator over all hits |
| `count(query)` | Number of matches |
| `status()` | Dict with `ready`, `index_version`, `entries`, `volumes`, `notes` |
| `version()` | `(service version, protocol version)` |

`sort` is `"name"`, `"path"`, `"size"` or `"modified"`. A hit is a dict with the
requested fields: `name`, `path`, `size` (bytes), `modified` (Unix seconds, UTC)
and `folder` (bool). One request returns at most 10 000 hits.

Errors from the service, such as an invalid regex, raise `zthing.Error` with the
service's message. A client is not thread-safe; use one per thread.

The query syntax and the protocol are described in `docs/API.md`.
