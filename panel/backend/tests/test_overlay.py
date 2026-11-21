from pathlib import Path

from app.engine import OVERLAY_DEFAULTS, card_phase, clock, line_opacity, queue_opacity
from app.overlay import (
    BLANK, SEGMENT_COLORS, Card, Line, QueueCard, QueueItem, compose, line_edges, render, render_queue,
)

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


def test_line_between_the_cards():
    # 200 s song, next one follows: hidden under both cards, crossfades at the edges.
    assert line_opacity(5, 195, 200, S) == 0
    assert line_opacity(14.75, 185.25, 200, S) == 0.5
    assert line_opacity(100, 100, 200, S) == 1
    assert line_opacity(200 - 19.75, 19.75, 200, S) == 0.5
    assert line_opacity(190, 10, 200, S) == 0


def test_line_without_up_next_runs_to_the_end():
    assert line_opacity(190, None, 200, S) == 1
    assert line_opacity(190, 10, 200, {**S, "show_next": False}) == 1
    assert line_opacity(199.75, None, 200, S) == 0.5


def test_line_without_card_and_off():
    assert line_opacity(3, 197, 200, {**S, "enabled": False}) == 1
    assert line_opacity(100, 100, 200, {**S, "mode": "always"}) == 0
    assert line_opacity(100, 100, 200, {**S, "progress_line": False}) == 0


def test_compose_draws_full_width_line_at_the_bottom():
    image = compose(None, Line((200.0,), 50.0, 1.0), S, (1280, 720))
    box = image.getchannel("A").getbbox()
    assert box == (0, 716, 1280, 720)
    assert image.getpixel((100, 718))[:3] == (0xFF, 0x5C, 0x7A)  # filled: accent
    assert image.getpixel((1000, 718))[3] < 140  # rest: faint track
    assert compose(None, None, S, (1280, 720)) is BLANK


def test_line_has_a_segment_per_queued_song():
    # On air 200 s (half played), then queued songs of 100 s and 100 s.
    image = compose(None, Line((200.0, 100.0, 100.0), 100.0, 1.0), S, (1280, 720))
    edges = line_edges((200.0, 100.0, 100.0), 1280)
    assert [x0 for x0, _ in edges] == [0, 640, 960] and edges[-1][1] == 1280
    assert edges[0][1] < 640  # a gap between segments
    assert image.getpixel((100, 718)) == (0xFF, 0x5C, 0x7A, 255)  # played part of the song on air
    assert image.getpixel((500, 718))[:3] == (0xFF, 0x5C, 0x7A)  # rest of it: faint accent
    assert image.getpixel((500, 718))[3] < 140
    second = image.getpixel((800, 718))
    third = image.getpixel((1100, 718))
    assert "#%02x%02x%02x" % second[:3] == SEGMENT_COLORS[0] and second[3] < 140
    assert "#%02x%02x%02x" % third[:3] == SEGMENT_COLORS[1]
    assert image.getpixel((edges[0][1], 718))[3] == 0  # gap


def test_one_song_is_one_unbroken_line():
    assert line_edges((180.0,), 1280) == [(0, 1280)]


def test_queue_card_brightens_then_dims():
    q = {**S, "queue_bright_seconds": 8, "queue_dim_opacity": 0.4}
    assert queue_opacity(0, True, q) == 0  # new card fades in from nothing
    assert queue_opacity(0, False, q) == 0.4  # existing card brightens from dimmed
    assert queue_opacity(0.25, True, q) == 0.5
    assert queue_opacity(4, True, q) == 1
    assert 0.4 < queue_opacity(8.5, True, q) < 1
    assert queue_opacity(30, True, q) == 0.4


def test_queue_card_lists_countdowns():
    items = [QueueItem("Песня", "Исполнитель", "2:15", SEGMENT_COLORS[0]),
             QueueItem("Ещё", "", "5:20", SEGMENT_COLORS[1])]
    image = render_queue(QueueCard(items, 0, 0.4), S, (1280, 720))
    assert image.getchannel("A").getbbox() is not None


def test_queue_card_sits_opposite_the_song_card():
    queue = QueueCard([QueueItem("Song", "Artist", "3:00", SEGMENT_COLORS[0])], 2)
    box = render_queue(queue, S, (1280, 720)).getchannel("A").getbbox()  # song card bottom-left
    assert box[2] > 1180 and box[1] < 100  # queue top-right
    box = render_queue(queue, {**S, "position": "top-right"}, (1280, 720)).getchannel("A").getbbox()
    assert box[0] < 100 and box[3] > 600  # queue bottom-left
    dim = render_queue(QueueCard(queue.items, 0, 0.4), S, (1280, 720)).getchannel("A")
    assert dim.getextrema()[1] <= 103


def test_concurrent_writes_do_not_collide(tmp_path):
    from concurrent.futures import ThreadPoolExecutor

    from PIL import Image

    from app.overlay import write

    target = tmp_path / "card.png"
    image = Image.new("RGBA", (64, 36), (255, 0, 0, 128))
    with ThreadPoolExecutor(8) as pool:
        list(pool.map(lambda _: write(image, target), range(200)))
    assert Image.open(target).size == (64, 36)
    assert [p.name for p in tmp_path.iterdir()] == ["card.png"]
