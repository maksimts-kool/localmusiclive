from app.timeline import DAY, Slot, continuation, gap, layout


def track(length: float, track_id: int = 1) -> Slot:
    return Slot(
        kind="auto",
        path=f"/media/{track_id}.mp4",
        seek=0.0,
        out=length,
        duration=length,
        track_id=track_id,
    )


def total(slots: list[Slot]) -> float:
    return round(sum(s.length for s in slots), 3)


def test_prefix_is_kept_unchanged():
    prefix = [gap(1000), track(200, 7)]
    today, _ = layout(prefix, iter([track(100, 8)]))
    assert today[:2] == prefix
    assert today[2].track_id == 8


def test_fills_today_exactly_then_tomorrow():
    today, tomorrow = layout([gap(DAY - 400)], (track(100, i) for i in range(2000)))
    assert total(today) == DAY
    assert len(today) == 5
    assert total(tomorrow) >= DAY


def test_clip_crossing_midnight_is_split():
    today, tomorrow = layout([gap(DAY - 50)], iter([track(100, 3), track(100, 4)]))
    head, tail = today[-1], tomorrow[0]
    assert (head.seek, head.out, head.full_out, head.part) == (0.0, 50.0, 100.0, 0)
    assert (tail.seek, tail.out, tail.full_out, tail.part) == (50.0, 100.0, None, 1)
    assert total(today) == DAY
    assert tomorrow[1].track_id == 4


def test_less_than_a_second_before_midnight_starts_clip_tomorrow():
    today, tomorrow = layout([gap(DAY - 0.6)], iter([track(100, 3)]))
    assert len(today) == 1
    assert (tomorrow[0].seek, tomorrow[0].out, tomorrow[0].part) == (0.0, 100.0, 0)


def test_small_overrun_is_not_split():
    today, tomorrow = layout([gap(DAY - 100)], iter([track(100.4, 3), track(50, 4)]))
    assert today[-1].out == 100.4 and today[-1].full_out is None
    assert tomorrow[0].track_id == 4


def test_tiny_tail_is_dropped():
    today, tomorrow = layout([gap(DAY - 100)], iter([track(100.8, 3), track(50, 4)]))
    assert today[-1].out == 100.0
    assert tomorrow[0].track_id == 4


def test_split_clip_on_air_continues_tomorrow():
    head = Slot(kind="queue", path="/media/x.mp4", seek=0.0, out=50.0, duration=100.0,
                track_id=9, queue_id=1, full_out=100.0)
    _, tomorrow = layout([gap(DAY - 50), head], iter([track(30, 2)]))
    assert tomorrow[0].part == 1 and tomorrow[0].seek == 50.0 and tomorrow[0].track_id == 9
    assert tomorrow[1].track_id == 2


def test_trimmed_clip_has_no_continuation():
    skipped = Slot(kind="queue", path="/media/x.mp4", seek=0.0, out=12.0, duration=100.0)
    assert continuation(skipped) is None


def test_tomorrow_stops_after_a_full_day():
    consumed = []

    def source():
        for i in range(10_000):
            consumed.append(i)
            yield track(600, i)

    _, tomorrow = layout([gap(DAY - 10)], source())
    assert total(tomorrow) == DAY
    assert tomorrow[-1].full_out == 600 and tomorrow[-1].out < 600
    assert len(consumed) < 200


def test_tomorrow_unchanged_when_it_becomes_today():
    tracks = [track(70 + i % 13, i) for i in range(3000)]
    _, tomorrow = layout([gap(DAY - 30)], iter(tracks))
    # After midnight the tail of track 0 is on air; nothing else changed.
    assert tomorrow[0].part == 1 and tomorrow[0].track_id == 0
    new_today, _ = layout(tomorrow[:1], iter(tracks[1:]))
    assert new_today == tomorrow


def test_empty_upcoming_leaves_tomorrow_empty():
    today, tomorrow = layout([gap(100), track(30)], iter([]))
    assert total(today) == 130 and tomorrow == []
