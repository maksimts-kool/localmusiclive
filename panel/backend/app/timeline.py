"""Turns the queue into ffplayout's daily playlists.

ffplayout plays one JSON playlist per day and finds "now" by wall clock: a
clip's start time is the day start plus the lengths of all clips before it.
So the panel never edits what already played. It keeps every slot up to and
including the current one exactly as written and rewrites only what follows.

The clip that crosses the day boundary is split in two: the first part closes
today's playlist and the second part opens tomorrow's, so playback continues
through midnight without a cut.
"""

from __future__ import annotations

from dataclasses import asdict, dataclass, replace
from typing import Iterable

DAY = 86400.0
# ffplayout skips clips shorter than one second.
MIN_PART = 1.0
# A clip may overrun the day end by this much instead of being split.
OVERRUN_TOLERANCE = 0.5


def _r(value: float) -> float:
    return round(value, 3)


@dataclass
class Slot:
    kind: str  # "gap" | "queue" | "auto"
    path: str  # what ffplayout plays; "" makes ffplayout generate black/silence
    seek: float  # ffplayout "in"
    out: float  # ffplayout "out"
    duration: float  # full media length
    track_id: int | None = None
    queue_id: int | None = None
    auto_id: int | None = None
    title: str = ""
    full_out: float | None = None  # set on the first part of a split clip
    part: int = 0  # 1 on the second part of a split clip

    @property
    def length(self) -> float:
        return self.out - self.seek

    def to_media(self) -> dict:
        media = {
            "in": _r(self.seek),
            "out": _r(self.out),
            "duration": _r(self.duration),
            "source": self.path,
        }
        if self.title:
            media["title"] = self.title
        return media

    def to_dict(self) -> dict:
        return asdict(self)

    @classmethod
    def from_dict(cls, data: dict) -> Slot:
        return cls(**data)


def gap(length: float) -> Slot:
    return Slot(kind="gap", path="", seek=0.0, out=_r(length), duration=_r(length))


def continuation(slot: Slot) -> Slot | None:
    """The part of a split clip that belongs to the next day."""
    if slot.full_out is None or slot.full_out - slot.out < MIN_PART:
        return None
    return replace(slot, seek=slot.out, out=slot.full_out, full_out=None, part=1)


def layout(prefix: list[Slot], upcoming: Iterable[Slot]) -> tuple[list[Slot], list[Slot]]:
    """Fill today after `prefix`, then all of tomorrow, from `upcoming`.

    `prefix` is today's playlist up to and including the clip on air. It is
    returned unchanged as the start of today's list. `upcoming` is consumed
    lazily and only as far as needed to fill tomorrow.
    """
    today = list(prefix)
    tomorrow: list[Slot] = []
    pos = sum(slot.length for slot in today)
    tomorrow_pos = 0.0

    def items():
        if today and (tail := continuation(today[-1])):
            yield tail
        yield from upcoming

    for item in items():
        if item.length < MIN_PART:
            continue

        if pos < DAY:
            room = DAY - pos
            if item.length <= room + OVERRUN_TOLERANCE:
                today.append(item)
                pos += item.length
                continue

            pos = DAY
            if room >= MIN_PART:
                head = replace(item, out=_r(item.seek + room), full_out=item.out)
                today.append(head)
                item = continuation(head)
                if item is None:
                    continue
            # Otherwise less than a second is left: ffplayout moves on to the
            # next day by itself, so the clip starts tomorrow in full.

        room = DAY - tomorrow_pos
        if item.length > room + OVERRUN_TOLERANCE:
            # Split tomorrow's last clip the same way, so the playlist doesn't
            # change (and make ffplayout reload) once tomorrow becomes today.
            if room >= MIN_PART:
                tomorrow.append(replace(item, out=_r(item.seek + room), full_out=item.out))
            break
        tomorrow.append(item)
        tomorrow_pos += item.length
        if tomorrow_pos >= DAY:
            break

    return today, tomorrow
