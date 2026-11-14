"""Search file names through the ZTHING service on Windows.

(C) 2026 R.Trunk, licensed under the GNU General Public License version 3.

Talks JSON Lines to the service pipe ``\\\\.\\pipe\\zthing`` directly, so it needs
neither zthing.dll nor anything beyond the standard library. docs/API.md in the
ZTHING repository describes the protocol.

    import zthing

    with zthing.Client() as zt:
        for hit in zt.search("*.iso", sort="size", descending=True, limit=5).hits:
            print(hit["size"], hit["path"])
"""

import json
import time

__all__ = ["Client", "Error", "Page", "PIPE_NAME", "PROTOCOL_VERSION"]

PIPE_NAME = r"\\.\pipe\zthing"
PROTOCOL_VERSION = 1
# The service caps one request at this many results.
MAX_LIMIT = 10_000
FIELDS = ("name", "path", "size", "modified", "folder")
# ERROR_PIPE_BUSY: every pipe instance is taken for a moment.
_PIPE_BUSY = 231
_CONNECT_TIMEOUT = 2.0


class Error(Exception):
    """The service is unreachable, rejected a request or answered unexpectedly."""


class Page:
    """One page of a search.

    ``total`` counts all matches, ``hits`` holds dicts with the requested fields:
    ``name``, ``path``, ``size`` (bytes), ``modified`` (Unix seconds, UTC) and
    ``folder`` (bool).
    """

    def __init__(self, total, millis, hits):
        self.total = total
        self.millis = millis
        self.hits = hits

    def __repr__(self):
        return f"Page(total={self.total}, hits={len(self.hits)}, millis={self.millis:.1f})"


class Client:
    """One connection to the service. Not safe to share between threads."""

    def __init__(self):
        deadline = time.monotonic() + _CONNECT_TIMEOUT
        while True:
            try:
                # Unbuffered, because buffered reads on a pipe wait for more data than one answer.
                self._pipe = open(PIPE_NAME, "r+b", buffering=0)
                break
            except OSError as e:
                if getattr(e, "winerror", None) == _PIPE_BUSY and time.monotonic() < deadline:
                    time.sleep(0.02)
                    continue
                raise Error(f"the ZTHING service is not running ({PIPE_NAME}: {e})") from e
        self._next_id = 1
        self._pending = b""

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def close(self):
        self._pipe.close()

    def search(self, query, sort="name", descending=False, offset=0, limit=100, fields=FIELDS):
        """One page of matches. ``sort`` is ``name``, ``path``, ``size`` or ``modified``."""
        answer = self._call(
            "search",
            query=query,
            sort=sort,
            descending=descending,
            offset=offset,
            limit=limit,
            fields=list(fields),
        )
        return Page(answer.get("total", 0), answer.get("millis", 0.0), answer.get("results", []))

    def search_all(self, query, sort="name", descending=False, fields=FIELDS):
        """Yields every match, fetching pages of the largest size the service allows."""
        offset = 0
        while True:
            page = self.search(query, sort, descending, offset, MAX_LIMIT, fields)
            yield from page.hits
            offset += len(page.hits)
            if not page.hits or offset >= page.total:
                return

    def count(self, query):
        return self._call("count", query=query).get("total", 0)

    def status(self):
        """Dict with ``ready``, ``index_version``, ``entries``, ``volumes`` and ``notes``."""
        return self._call("status")["status"]

    def version(self):
        """The service version and the protocol version it speaks."""
        answer = self._call("version")
        return answer["version"], answer["protocol"]

    def _call(self, cmd, **arguments):
        request_id = self._next_id
        self._next_id += 1
        request = {"v": PROTOCOL_VERSION, "id": request_id, "cmd": cmd, **arguments}
        try:
            self._pipe.write(json.dumps(request).encode() + b"\n")
            line = self._read_line()
        except OSError as e:
            raise Error(f"talking to the ZTHING service: {e}") from e
        answer = json.loads(line)
        if answer.get("id") != request_id:
            raise Error(f"answer to request {answer.get('id')} instead of {request_id}")
        if not answer.get("ok"):
            raise Error(answer.get("error", "request failed"))
        return answer

    def _read_line(self):
        while b"\n" not in self._pending:
            chunk = self._pipe.read(65536)
            if not chunk:
                raise Error("the service closed the connection")
            self._pending += chunk
        line, _, self._pending = self._pending.partition(b"\n")
        return line
