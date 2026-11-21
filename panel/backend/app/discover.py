"""Ranks new songs to download, from what you play most.

The YouTube side (youtube.py) collects candidates for the most-played songs in
the library: YouTube's related "Mix" for each of them, and other songs by the
most-played artists. This module throws out what's already in the library
and what isn't the song's music video, and ranks the rest by how strongly
your favourites point to it.
"""

from __future__ import annotations

import math
import random
import re
from dataclasses import dataclass, field
from typing import Iterable

MIN_SECONDS = 60
MAX_SECONDS = 12 * 60
# Uploads that are not the song's own clip. Remixes are welcome.
UNWANTED = re.compile(
    r"\blyrics?\b|\blyric\s+video\b|\bletra\b|\bтекст\b|\b(official\s+)?audio\b|\bvisuali[sz]er\b"
    r"|\blive\b|\bconcert\b|\bcover\b|\bkaraoke\b|\binstrumental\b|\bsped\s*up\b"
    r"|\bslowed\b|\bnightcore\b|\b8d\b|\breaction\b|\btutorial\b|\b\d+\s*hours?\b|\bloop\b"
    r"|\bfull\s+album\b|\bplaylist\b|\bcompilation\b|\bbest\s+of\b|\bmix\s+20\d\d\b",
    re.IGNORECASE,
)
FEAT = re.compile(r"\s*[(\[]?\b(feat|ft|featuring|with)\b\.?.*$", re.IGNORECASE)


def song_key(artist: str, title: str) -> str:
    """The same song under slightly different names maps to one key."""
    def norm(text: str) -> str:
        text = FEAT.sub("", text.casefold())
        return re.sub(r"[\W_]+", " ", text).strip()

    return f"{norm(artist)}|{norm(title)}"


@dataclass
class Source:
    """Candidates found from one favourite song (kind "mix") or artist ("artist")."""

    kind: str
    name: str  # the song's title, or the artist
    plays: int
    entries: list[dict] = field(default_factory=list)


def rank(
    sources: Iterable[Source],
    have_ids: set[str],
    have_keys: set[str],
    artist_songs: dict[str, int],
    split,
    limit: int = 20,
) -> list[dict]:
    """New songs best first, each with the reasons it was picked.

    entries are YouTube results ({"id", "title", "channel", "duration", ...});
    split(title, channel) gives (artist, song title); artist_songs counts
    library songs per casefolded artist.
    """
    found: dict[str, dict] = {}
    for source in sources:
        weight = 1.0 + math.log1p(source.plays)
        if source.kind == "artist":
            weight *= 0.7
        count = len(source.entries)
        for position, entry in enumerate(source.entries):
            vid = entry.get("id")
            title = entry.get("title") or ""
            length = entry.get("duration")
            if not vid or vid in have_ids or UNWANTED.search(title):
                continue
            if entry.get("live_status") in ("is_live", "is_upcoming", "post_live"):
                continue
            if length is not None and not MIN_SECONDS <= length <= MAX_SECONDS:
                continue
            artist, song = split(title, entry.get("channel") or "")
            key = song_key(artist, song)
            if key in have_keys:
                continue
            item = found.get(key)
            if item is None:
                item = found[key] = {
                    **entry, "artist": artist, "song": song,
                    "score": 0.0, "related": [], "by": None,
                }
            # Another upload of the same song still counts for it; the first one found is kept.
            # Earlier in a Mix means more closely related.
            item["score"] += weight * (1.0 - position / (count + 1))
            if source.kind == "mix":
                item["related"].append((source.name, source.plays))
            else:
                item["by"] = (source.name, source.plays)

    for item in found.values():
        songs = artist_songs.get(item["artist"].casefold(), 0)
        if songs:
            item["score"] += 0.4 * math.log1p(songs)
        if len(item["related"]) > 1:
            item["score"] *= 1.0 + 0.15 * (len(item["related"]) - 1)
        item["reasons"] = _reasons(item, songs)

    ranked = sorted(found.values(), key=lambda i: -i["score"])[:limit]
    for item in ranked:
        item["score"] = round(item["score"], 3)
        del item["related"], item["by"]
    return ranked


def _reasons(item: dict, songs: int) -> list[str]:
    """Short reasons, e.g. ["Like Cheri Cheri Lady +2", "More Bad Boys Blue"]."""
    reasons = []
    related = sorted(item["related"], key=lambda r: -r[1])
    if related:
        others = len({name for name, _ in related}) - 1
        reasons.append(f"Like {related[0][0]}" + (f" +{others}" if others else ""))
    if item["by"] or songs:
        reasons.append(f"More {item['by'][0] if item['by'] else item['artist']}")
    return reasons


def weighted_sample(items: list, weights: list[float], k: int, rng: random.Random | None = None) -> list:
    """k different items, likelier the heavier they are."""
    rng = rng or random
    pool = list(zip(items, weights))
    picked = []
    while pool and len(picked) < k:
        index = rng.choices(range(len(pool)), weights=[max(w, 0.01) for _, w in pool])[0]
        picked.append(pool.pop(index)[0])
    return picked


def fresh_picks(
    ranked: list[dict], recently_shown: set[str], limit: int, rng: random.Random | None = None
) -> list[dict]:
    """Up to `limit` suggestions, leaving out the ones shown in recent lists.

    Close scores are shuffled a little so the same list doesn't come back in
    the same order; recently shown songs only fill up a short list.
    """
    rng = rng or random
    jittered = sorted(ranked, key=lambda item: -item["score"] * rng.uniform(0.75, 1.25))
    new = [item for item in jittered if item["id"] not in recently_shown]
    seen = [item for item in jittered if item["id"] in recently_shown]
    return (new + seen)[:limit]
