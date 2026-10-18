"""Draws the on-screen song card and hands it to ffplayout.

The card is a transparent PNG the size of the video frame. ffplayout (with
live-logo.patch) shows it as its logo and reloads it whenever the file changes,
so the panel can fade it in and out and tick the countdown every second.
"""

import os
from dataclasses import dataclass
from functools import lru_cache
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

FONT_DIR = Path(os.environ.get("CARD_FONT_DIR", "/usr/share/fonts/opentype/inter"))
FONTS = {
    "heading": "Inter-SemiBold.otf",
    "title": "InterDisplay-Bold.otf",
    "body": "Inter-Regular.otf",
    "status": "Inter-Medium.otf",
}
# Sizes at 720p and 100 % scale.
BASE = {
    "padding": 18,
    "gap": 16,
    "heading": 13,
    "title": 28,
    "body": 19,
    "status": 15,
    "thumb_height": 100,
    "bar": 4,
    "min_width": 380,
}
SLIDE = 14  # px the card rises while it fades in


@dataclass
class Card:
    heading: str  # "NOW PLAYING" / "UP NEXT"
    title: str
    artist: str
    status: str  # e.g. "Next song in {time}"; {time} is drawn with even-width digits
    time: str  # "3:42"
    progress: float | None  # 0..1, or None for no bar
    thumb: Path | None
    opacity: float = 1.0


@lru_cache(maxsize=32)
def font(kind: str, size: int) -> ImageFont.FreeTypeFont:
    try:
        return ImageFont.truetype(str(FONT_DIR / FONTS[kind]), size)
    except OSError:
        return ImageFont.load_default(size)


def rgba(hex_color: str, alpha: float) -> tuple[int, int, int, int]:
    n = int(hex_color.lstrip("#"), 16)
    return (n >> 16, (n >> 8) & 255, n & 255, round(max(0.0, min(alpha, 1.0)) * 255))


def fit(draw: ImageDraw.ImageDraw, text: str, face: ImageFont.FreeTypeFont, width: int) -> str:
    if draw.textlength(text, font=face) <= width:
        return text
    while text and draw.textlength(text + "…", font=face) > width:
        text = text[:-1]
    return text.rstrip() + "…"


def spaced(text: str) -> str:
    # Wide letter-spacing for the small uppercase heading.
    return " ".join(text)


@lru_cache(maxsize=64)
def thumbnail(path: Path, height: int, radius: int, mtime: float) -> Image.Image | None:
    try:
        image = Image.open(path).convert("RGBA")
    except OSError:
        return None
    width = round(height * 16 / 9)
    image = image.resize((width, height), Image.LANCZOS)
    mask = Image.new("L", (width, height), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, width - 1, height - 1), radius, fill=255)
    image.putalpha(mask)
    return image


def draw_time(draw, xy, text: str, face, fill) -> None:
    """Digits in equal-width cells so the countdown doesn't wobble."""
    x, y = xy
    cell = max(draw.textlength(d, font=face) for d in "0123456789")
    for ch in text:
        w = cell if ch.isdigit() else draw.textlength(ch, font=face)
        draw.text((x + (w - draw.textlength(ch, font=face)) / 2, y), ch, font=face, fill=fill)
        x += w


def time_width(draw, text: str, face) -> float:
    cell = max(draw.textlength(d, font=face) for d in "0123456789")
    return sum(cell if ch.isdigit() else draw.textlength(ch, font=face) for ch in text)


def render(card: Card, settings: dict, size: tuple[int, int]) -> Image.Image:
    frame_w, frame_h = size
    k = frame_h / 720 * settings["scale"] / 100
    px = {name: max(1, round(value * k)) for name, value in BASE.items()}
    f_heading = font("heading", px["heading"])
    f_title = font("title", px["title"])
    f_body = font("body", px["body"])
    f_status = font("status", px["status"])

    probe = ImageDraw.Draw(Image.new("RGBA", (1, 1)))
    pad, gap = px["padding"], px["gap"]
    thumb = None
    if settings["show_thumbnail"] and card.thumb and card.thumb.exists():
        thumb = thumbnail(card.thumb, px["thumb_height"], round(10 * k), card.thumb.stat().st_mtime)
    thumb_w = thumb.width + gap if thumb else 0

    # Text block.
    max_text = round(frame_w * 0.62) - 2 * pad - thumb_w
    heading = spaced(card.heading.upper())
    title = fit(probe, card.title, f_title, max_text)
    artist = fit(probe, card.artist, f_body, max_text) if card.artist else ""
    before, _, after = card.status.partition("{time}")
    status_w = probe.textlength(before, font=f_status) + time_width(probe, card.time, f_status) + probe.textlength(after, font=f_status)
    text_w = max(
        probe.textlength(heading, font=f_heading),
        probe.textlength(title, font=f_title),
        probe.textlength(artist, font=f_body) if artist else 0,
        status_w,
    )

    line = {
        "heading": round(px["heading"] * 1.5),
        "title": round(px["title"] * 1.25),
        "body": round(px["body"] * 1.35),
        "status": round(px["status"] * 1.5),
    }
    text_h = line["heading"] + line["title"] + (line["body"] if artist else 0) + round(6 * k) + line["status"]
    bar_h = px["bar"] + round(12 * k) if settings["show_progress"] and card.progress is not None else 0
    inner_h = max(text_h, thumb.height if thumb else 0)
    card_w = max(px["min_width"], round(2 * pad + thumb_w + text_w))
    card_h = 2 * pad + inner_h + bar_h

    # Position, with the fade-in slide.
    margin = round(settings["margin"] * frame_h / 720)
    vertical, horizontal = settings["position"].split("-")
    x = {"left": margin, "center": (frame_w - card_w) // 2, "right": frame_w - margin - card_w}[horizontal]
    y = margin if vertical == "top" else frame_h - margin - card_h
    rise = round(SLIDE * k * (1 - card.opacity))
    y += -rise if vertical == "top" else rise

    layer = Image.new("RGBA", size, (0, 0, 0, 0))
    radius = round(settings["radius"] * k)
    box = (x, y, x + card_w, y + card_h)

    if settings["shadow"]:
        shadow = Image.new("RGBA", size, (0, 0, 0, 0))
        offset = round(8 * k)
        ImageDraw.Draw(shadow).rounded_rectangle(
            (box[0], box[1] + offset, box[2], box[3] + offset), radius, fill=(0, 0, 0, 110)
        )
        layer = Image.alpha_composite(layer, shadow.filter(ImageFilter.GaussianBlur(round(18 * k))))

    draw = ImageDraw.Draw(layer)
    stroke = max(1, round(settings["stroke_width"] * k)) if settings["stroke"] else 0
    draw.rounded_rectangle(
        box, radius,
        fill=rgba(settings["background_color"], settings["background_opacity"]),
        outline=rgba(settings["stroke_color"], settings["stroke_opacity"]) if stroke else None,
        width=stroke,
    )

    if thumb:
        ty = y + pad + (inner_h - thumb.height) // 2
        layer.alpha_composite(thumb, (x + pad, ty))

    tx = x + pad + thumb_w
    ty = y + pad + (inner_h - text_h) // 2
    accent = rgba(settings["accent_color"], 1)
    white = (255, 255, 255, 255)
    soft = (255, 255, 255, 190)
    faint = (255, 255, 255, 150)

    draw.text((tx, ty), heading, font=f_heading, fill=accent)
    ty += line["heading"]
    draw.text((tx, ty), title, font=f_title, fill=white)
    ty += line["title"]
    if artist:
        draw.text((tx, ty), artist, font=f_body, fill=soft)
        ty += line["body"]
    ty += round(6 * k)
    sx = tx
    draw.text((sx, ty), before, font=f_status, fill=faint)
    sx += probe.textlength(before, font=f_status)
    draw_time(draw, (sx, ty), card.time, f_status, white)
    sx += time_width(probe, card.time, f_status)
    draw.text((sx, ty), after, font=f_status, fill=faint)

    if bar_h:
        bx0, bx1 = x + pad, x + card_w - pad
        by = y + card_h - pad - px["bar"]
        r = px["bar"] // 2
        draw.rounded_rectangle((bx0, by, bx1, by + px["bar"]), r, fill=(255, 255, 255, 46))
        fill_to = bx0 + round((bx1 - bx0) * max(0.0, min(card.progress, 1.0)))
        if fill_to > bx0 + px["bar"]:
            draw.rounded_rectangle((bx0, by, fill_to, by + px["bar"]), r, fill=accent)

    if card.opacity < 1:
        alpha = layer.getchannel("A").point(lambda a: round(a * max(card.opacity, 0.0)))
        layer.putalpha(alpha)
    return layer


BLANK = Image.new("RGBA", (2, 2), (0, 0, 0, 0))


def write(image: Image.Image, target: Path) -> None:
    """Replace the card atomically, so ffplayout never reads half a file."""
    target.parent.mkdir(parents=True, exist_ok=True)
    tmp = target.with_name(f".{target.name}.tmp")
    image.save(tmp, format="PNG", compress_level=1)
    os.replace(tmp, target)
