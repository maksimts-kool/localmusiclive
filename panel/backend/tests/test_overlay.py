from pathlib import Path

from app.engine import OVERLAY_DEFAULTS, card_phase, clock
from app.overlay import Card, render

S = OVERLAY_DEFAULTS  # start 15 s, end 20 s


def phase(*args, **kw):
    result = card_phase(*args, **kw)
    return result and result[0]


def test_now_playing_only_at_the_start():
    assert phase(0.6, 200, S) == "now"
    assert phase(14.9, 200, S) == "now"
    assert phase(15.1, 200, S) is None
    assert phase(120, 100, S) is None


def test_up_next_before_the_end():
    assert phase(200, 20, S) == "next"
    assert phase(200, 0.5, S) == "next"
    assert phase(200, 0.1, S) is None


def test_fades():
    assert card_phase(0.25, 200, S) == ("now", 0.5)
    assert card_phase(14.75, 200, S) == ("now", 0.5)
    assert card_phase(7, 200, S) == ("now", 1.0)
    assert card_phase(200, 19.75, S)[1] == 0.5
    assert card_phase(200, 0.45, S)[1] == 0.5


def test_nothing_next_means_no_up_next():
    assert phase(200, None, S) is None
    assert phase(3, None, S) == "now"
    assert phase(200, 10, {**S, "show_next": False}) is None


def test_short_song_prefers_up_next():
    assert phase(5, 12, S) == "next"


def test_always_and_disabled():
    assert card_phase(100, 100, {**S, "mode": "always"}) == ("now", 1.0)
    assert phase(1, 100, {**S, "enabled": False}) is None


def test_clock():
    assert clock(0) == "0:00" and clock(59.2) == "1:00" and clock(222) == "3:42"


def test_render_draws_card_in_corner():
    card = Card("Now playing", "Get Lucky", "Daft Punk", "Next song in {time}", "3:42", 0.4, None)
    image = render(card, S, (1280, 720))
    assert image.size == (1280, 720)
    box = image.getchannel("A").getbbox()
    assert box[0] < 100 and box[3] > 600  # bottom-left
    top_right = render(card, {**S, "position": "top-right"}, (1280, 720)).getchannel("A").getbbox()
    assert top_right[2] > 1180 and top_right[1] < 100


def test_render_fades():
    card = Card("Up next", "Song", "", "Starts in {time}", "0:10", None, Path("/nope.jpg"), 0.5)
    alpha = render(card, S, (1280, 720)).getchannel("A")
    assert 0 < alpha.getextrema()[1] <= 128
