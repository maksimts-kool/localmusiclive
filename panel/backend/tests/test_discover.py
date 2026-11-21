from app.discover import Source, rank, song_key
from app.youtube import split_title


def e(vid, title, channel="Someone", duration=220):
    return {"id": vid.ljust(11, "x"), "title": title, "channel": channel, "duration": duration}


def test_song_key_ignores_case_punctuation_and_features():
    assert song_key("Daft Punk", "Get Lucky (feat. Pharrell)") == song_key("daft punk", "Get Lucky!")


def test_ranks_new_songs_by_how_strongly_favourites_point_to_them():
    sources = [
        Source("mix", "Bohemian Rhapsody", 12, [
            e("seed1", "Queen - Bohemian Rhapsody (Official Video)"),
            e("a", "Queen - Don't Stop Me Now (Official Video)"),
            e("b", "Muse - Uprising"),
            e("c", "Queen - Somebody To Love (Lyrics)"),
            e("d", "Queen - Live Aid 1985 (Live)"),
            e("e", "Queen - Greatest Hits", duration=3600),
            e("f", "David Bowie - Starman"),
        ]),
        Source("mix", "Levitating", 2, [e("g", "Dua Lipa - Physical"), e("f2", "David Bowie - Starman (2012 Remaster)")]),
    ]
    ranked = rank(
        sources,
        have_ids={"seed1".ljust(11, "x")},
        have_keys={song_key("Muse", "Uprising")},
        artist_songs={"queen": 3},
        split=split_title,
    )
    titles = [r["title"] for r in ranked]
    # Owned, lyric, live and too-long uploads are left out.
    assert titles == [
        "Queen - Don't Stop Me Now (Official Video)",
        "David Bowie - Starman",
        "Dua Lipa - Physical",
    ]
    assert ranked[0]["reasons"] == ["Like Bohemian Rhapsody", "More Queen"]
    assert ranked[1]["reasons"] == ["Like Bohemian Rhapsody +1"]


def test_remixes_are_suggested_next_to_the_original():
    ranked = rank(
        [Source("mix", "Cheri Cheri Lady", 6, [e("a", "Modern Talking - Cheri Cheri Lady (Remix 2024)")])],
        set(), {song_key("Modern Talking", "Cheri Cheri Lady")}, {}, split_title,
    )
    assert [r["title"] for r in ranked] == ["Modern Talking - Cheri Cheri Lady (Remix 2024)"]


def test_artist_source_reason():
    ranked = rank(
        [Source("artist", "Queen", 30, [e("a", "Queen - Radio Ga Ga")])],
        set(), set(), {"queen": 3}, split_title,
    )
    assert ranked[0]["reasons"] == ["More Queen"]


def test_fresh_picks_skip_recently_shown_and_fill_up_with_them():
    import random

    from app.discover import fresh_picks

    ranked = [{"id": str(i), "score": 10.0 - i} for i in range(10)]
    picks = fresh_picks(ranked, {"0", "1", "2"}, 5, random.Random(1))
    assert len(picks) == 5 and not {"0", "1", "2"} & {p["id"] for p in picks}
    short = fresh_picks(ranked[:4], {"0", "1", "2"}, 4, random.Random(1))
    assert short[0]["id"] == "3" and {p["id"] for p in short} == {"0", "1", "2", "3"}


def test_weighted_sample_picks_distinct_items():
    import random

    from app.discover import weighted_sample

    for seed in range(20):
        picked = weighted_sample(list("abcdef"), [50, 1, 1, 1, 1, 1], 3, random.Random(seed))
        assert len(set(picked)) == 3
    assert weighted_sample(["a"], [0], 3) == ["a"]
