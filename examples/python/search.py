"""Prints the largest matches of a query, then how many matches there are per drive.

    pip install ./python        (or set PYTHONPATH=python)
    python examples/python/search.py "*.iso"
"""

import collections
import sys

import zthing

query = sys.argv[1] if len(sys.argv) > 1 else "*.iso"
try:
    with zthing.Client() as zt:
        page = zt.search(query, sort="size", descending=True, limit=10)
        print(f"{page.total} matches for {query} in {page.millis:.1f} ms, the largest:")
        for hit in page.hits:
            print(f"{hit['size']:>14,}  {hit['path']}")

        per_drive = collections.Counter(hit["path"][:2] for hit in zt.search_all(query, fields=["path"]))
        print("per drive:", dict(sorted(per_drive.items())))
except zthing.Error as e:
    sys.exit(f"zthing: {e}")
