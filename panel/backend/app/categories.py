"""Song categories: free-form names stored per track as a JSON list."""

import json
import re
from typing import Any, Iterable

MAX_LENGTH = 32
MAX_PER_TRACK = 8


def clean_categories(values: Iterable[str], existing: Iterable[str] = ()) -> list[str]:
    """Trimmed, de-duplicated names, reusing the casing of existing categories."""
    known = {name.casefold(): name for name in existing}
    out: list[str] = []
    seen: set[str] = set()
    for value in values:
        name = re.sub(r"\s+", " ", str(value)).strip()[:MAX_LENGTH].strip()
        key = name.casefold()
        if not name or key in seen:
            continue
        seen.add(key)
        out.append(known.get(key, name))
    return out[:MAX_PER_TRACK]


def categories_of(row: Any) -> list[str]:
    try:
        value = json.loads(row["categories"] or "[]")
    except (ValueError, TypeError, IndexError, KeyError):
        return []
    return [v for v in value if isinstance(v, str)] if isinstance(value, list) else []


def split_genres(text: str) -> list[str]:
    """A genre tag like "Pop; Dance/Electronic" as starting categories."""
    return clean_categories(re.split(r"[;,/|]", text))[:3] if text else []
