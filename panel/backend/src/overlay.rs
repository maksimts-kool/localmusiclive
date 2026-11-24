//! Draws the on-screen song card and hands it to ffplayout.
//!
//! The card is a transparent PNG the size of the video frame. ffplayout (with
//! live-logo.patch) shows it as its logo and reloads it whenever the file changes,
//! so the panel can fade it in and out and tick the countdown every second.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::raster::{self, Canvas, Face, Rgba};

pub const POSITIONS: [&str; 6] = ["bottom-left", "bottom-center", "bottom-right", "top-left", "top-center", "top-right"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OverlaySettings {
    pub enabled: bool,
    /// "start_end": "Now playing" as a song starts, "Up next" before it ends.
    /// "always": the "Now playing" card the whole time.
    pub mode: String,
    pub start_seconds: i64,
    pub end_seconds: i64,
    pub show_next: bool,
    pub position: String,
    pub margin: i64,
    pub scale: i64,
    pub accent_color: String,
    pub background_color: String,
    pub background_opacity: f64,
    pub stroke: bool,
    pub stroke_color: String,
    pub stroke_opacity: f64,
    pub stroke_width: f64,
    pub radius: i64,
    pub shadow: bool,
    pub show_thumbnail: bool,
    pub show_progress: bool,
    /// Full-width song progress line along the bottom edge while no card shows.
    pub progress_line: bool,
    pub line_height: i64,
    /// Small card listing the queued songs, in the colours of their segments on
    /// the progress line. Bright for a while after a new song starts or the
    /// queue changes, then dimmed.
    pub queue_card: bool,
    pub queue_rows: i64,
    pub queue_bright_seconds: i64,
    pub queue_dim_opacity: f64,
}

impl Default for OverlaySettings {
    fn default() -> Self {
        OverlaySettings {
            enabled: true,
            mode: "start_end".into(),
            start_seconds: 15,
            end_seconds: 20,
            show_next: true,
            position: "bottom-left".into(),
            margin: 48,
            scale: 100,
            accent_color: "#ff5c7a".into(),
            background_color: "#0e1118".into(),
            background_opacity: 0.9,
            stroke: true,
            stroke_color: "#ffffff".into(),
            stroke_opacity: 0.16,
            stroke_width: 1.5,
            radius: 18,
            shadow: true,
            show_thumbnail: true,
            show_progress: true,
            progress_line: true,
            line_height: 4,
            queue_card: true,
            queue_rows: 3,
            queue_bright_seconds: 8,
            queue_dim_opacity: 0.4,
        }
    }
}

impl OverlaySettings {
    fn corner(&self) -> (&str, &str) {
        self.position.split_once('-').unwrap_or(("bottom", "left"))
    }
}

/// Sizes at 720p and 100 % scale.
mod base {
    pub const PADDING: f32 = 18.0;
    pub const GAP: f32 = 16.0;
    pub const HEADING: f32 = 13.0;
    pub const TITLE: f32 = 28.0;
    pub const BODY: f32 = 19.0;
    pub const STATUS: f32 = 15.0;
    pub const THUMB_HEIGHT: f32 = 100.0;
    pub const BAR: f32 = 4.0;
    pub const MIN_WIDTH: f32 = 380.0;
}
/// px the card rises while it fades in
const SLIDE: f32 = 14.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    /// "Сейчас играет" / "Далее"
    pub heading: String,
    pub title: String,
    pub artist: String,
    /// e.g. "Следующая песня через {time}"; {time} is drawn with even-width digits
    pub status: String,
    /// "3:42"
    pub time: String,
    /// 0..1, or None for no bar
    pub progress: Option<f64>,
    pub thumb: Option<PathBuf>,
    pub opacity: f64,
}

pub fn rgba(hex: &str, alpha: f64) -> Rgba {
    let n = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0xffffff);
    [(n >> 16) as u8, (n >> 8) as u8, n as u8, (alpha.clamp(0.0, 1.0) * 255.0).round() as u8]
}

fn round(v: f32) -> f32 {
    v.round()
}

/// The text, shortened with "…" to fit `width`.
fn fit(text: &str, face: &Face, width: f32) -> String {
    if face.width(text) <= width {
        return text.to_string();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() && face.width(&format!("{}…", chars.iter().collect::<String>())) > width {
        chars.pop();
    }
    format!("{}…", chars.iter().collect::<String>().trim_end())
}

/// Wide letter-spacing for the small uppercase heading (thin spaces).
fn spaced(text: &str) -> String {
    text.chars().map(String::from).collect::<Vec<_>>().join("\u{2009}")
}

fn digit_cell(face: &Face) -> f32 {
    "0123456789".chars().map(|d| face.width(&d.to_string())).fold(0.0, f32::max)
}

/// Digits in equal-width cells so the countdown doesn't wobble.
fn draw_time(canvas: &mut Canvas, x: f32, y: f32, text: &str, face: &Face, ink: Rgba) {
    let cell = digit_cell(face);
    let mut x = x;
    for ch in text.chars() {
        let s = ch.to_string();
        let own = face.width(&s);
        let w = if ch.is_ascii_digit() { cell } else { own };
        face.draw(canvas, x + (w - own) / 2.0, y, &s, ink);
        x += w;
    }
}

fn time_width(text: &str, face: &Face) -> f32 {
    let cell = digit_cell(face);
    text.chars().map(|ch| if ch.is_ascii_digit() { cell } else { face.width(&ch.to_string()) }).sum()
}

pub fn render(card: &Card, s: &OverlaySettings, size: (u32, u32)) -> Canvas {
    let (frame_w, frame_h) = (size.0 as f32, size.1 as f32);
    let k = frame_h / 720.0 * s.scale as f32 / 100.0;
    let px = |v: f32| round(v * k).max(1.0);
    let (pad, gap) = (px(base::PADDING), px(base::GAP));
    let (heading_px, title_px, body_px, status_px) =
        (px(base::HEADING), px(base::TITLE), px(base::BODY), px(base::STATUS));
    let f_heading = Face::new("heading", heading_px);
    let f_title = Face::new("title", title_px);
    let f_body = Face::new("body", body_px);
    let f_status = Face::new("status", status_px);

    let thumb = match (&card.thumb, s.show_thumbnail) {
        (Some(path), true) => std::fs::metadata(path).ok().and_then(|meta| {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            raster::thumbnail(path, px(base::THUMB_HEIGHT) as u32, round(10.0 * k), mtime)
        }),
        _ => None,
    };
    let thumb_w = thumb.as_ref().map(|t| t.w as f32 + gap).unwrap_or(0.0);
    let thumb_h = thumb.as_ref().map(|t| t.h as f32).unwrap_or(0.0);

    // Text block.
    let max_text = round(frame_w * 0.62) - 2.0 * pad - thumb_w;
    let heading = spaced(&card.heading.to_uppercase());
    let title = fit(&card.title, &f_title, max_text);
    let artist = if card.artist.is_empty() { String::new() } else { fit(&card.artist, &f_body, max_text) };
    let (before, after) = card.status.split_once("{time}").unwrap_or((&card.status, ""));
    let status_w = f_status.width(before) + time_width(&card.time, &f_status) + f_status.width(after);
    let text_w = f_heading
        .width(&heading)
        .max(f_title.width(&title))
        .max(if artist.is_empty() { 0.0 } else { f_body.width(&artist) })
        .max(status_w);

    let line_heading = round(heading_px * 1.5);
    let line_title = round(title_px * 1.25);
    let line_body = round(body_px * 1.35);
    let line_status = round(status_px * 1.5);
    let status_gap = round(6.0 * k);
    let text_h = line_heading
        + line_title
        + if artist.is_empty() { 0.0 } else { line_body }
        + status_gap
        + line_status;
    let bar = px(base::BAR);
    let bar_h = if s.show_progress && card.progress.is_some() { bar + round(12.0 * k) } else { 0.0 };
    let inner_h = text_h.max(thumb_h);
    let card_w = px(base::MIN_WIDTH).max(round(2.0 * pad + thumb_w + text_w));
    let card_h = 2.0 * pad + inner_h + bar_h;

    // Position, with the fade-in slide.
    let margin = round(s.margin as f32 * frame_h / 720.0);
    let (vertical, horizontal) = s.corner();
    let x = match horizontal {
        "center" => ((frame_w - card_w) / 2.0).floor(),
        "right" => frame_w - margin - card_w,
        _ => margin,
    };
    let mut y = if vertical == "top" { margin } else { frame_h - margin - card_h };
    // Ease out: quick at first, settling gently into place.
    let rise = round(SLIDE * k * (1.0 - card.opacity as f32).clamp(0.0, 1.0).powi(3));
    y += if vertical == "top" { -rise } else { rise };

    let radius = round(s.radius as f32 * k);
    let (x1, y1) = (x + card_w, y + card_h);
    // Just the card and room for its shadow, not the whole frame.
    let offset = round(8.0 * k);
    let sigma = round(18.0 * k);
    let spread = if s.shadow { offset + (3.0 * sigma).ceil() + 3.0 } else { 2.0 };
    let mut layer = Canvas::new_at(
        (x - spread).floor() as i32,
        (y - spread).floor() as i32,
        (card_w + 2.0 * spread + 2.0).ceil() as u32,
        (card_h + 2.0 * spread + 2.0).ceil() as u32,
    );

    if s.shadow {
        layer.rounded_rect(x, y + offset, x1, y1 + offset, radius, Some([0, 0, 0, 110]), None, 0.0);
        layer.blur_alpha(sigma);
    }

    let stroke = if s.stroke { round(s.stroke_width as f32 * k).max(1.0) } else { 0.0 };
    layer.rounded_rect(
        x,
        y,
        x1,
        y1,
        radius,
        Some(rgba(&s.background_color, s.background_opacity)),
        (stroke > 0.0).then(|| rgba(&s.stroke_color, s.stroke_opacity)),
        stroke,
    );

    if let Some(thumb) = &thumb {
        let ty = y + pad + ((inner_h - thumb_h) / 2.0).floor();
        layer.composite(thumb, (x + pad) as i32, ty as i32);
    }

    let tx = x + pad + thumb_w;
    let mut ty = y + pad + ((inner_h - text_h) / 2.0).floor();
    let accent = rgba(&s.accent_color, 1.0);
    let white = [255, 255, 255, 255];
    let soft = [255, 255, 255, 190];
    let faint = [255, 255, 255, 150];

    f_heading.draw(&mut layer, tx, ty, &heading, accent);
    ty += line_heading;
    f_title.draw(&mut layer, tx, ty, &title, white);
    ty += line_title;
    if !artist.is_empty() {
        f_body.draw(&mut layer, tx, ty, &artist, soft);
        ty += line_body;
    }
    ty += status_gap;
    let mut sx = tx;
    f_status.draw(&mut layer, sx, ty, before, faint);
    sx += f_status.width(before);
    draw_time(&mut layer, sx, ty, &card.time, &f_status, white);
    sx += time_width(&card.time, &f_status);
    f_status.draw(&mut layer, sx, ty, after, faint);

    if bar_h > 0.0 {
        let (bx0, bx1) = (x + pad, x + card_w - pad);
        let by = y + card_h - pad - bar;
        let r = (bar / 2.0).floor();
        layer.rounded_rect(bx0, by, bx1, by + bar, r, Some([255, 255, 255, 46]), None, 0.0);
        let progress = card.progress.unwrap_or(0.0).clamp(0.0, 1.0) as f32;
        let fill_to = bx0 + round((bx1 - bx0) * progress);
        if fill_to > bx0 + bar {
            layer.rounded_rect(bx0, by, fill_to, by + bar, r, Some(accent), None, 0.0);
        }
    }

    if card.opacity < 1.0 {
        layer.multiply_alpha(card.opacity as f32);
    }
    layer
}

/// Colours of the queued songs on the progress line and the queue card. The song
/// on air is always the accent colour; queued songs take these in turn.
pub const SEGMENT_COLORS: [&str; 5] = ["#5cc8ff", "#ffd166", "#7bd88f", "#b18cff", "#ff9f5c"];

pub fn segment_color(index: usize, s: &OverlaySettings) -> String {
    if index == 0 {
        s.accent_color.clone()
    } else {
        SEGMENT_COLORS[(index - 1) % SEGMENT_COLORS.len()].to_string()
    }
}

/// The full-width progress line: one segment per song (the one on air, then
/// the queued ones), each as wide as the song is long. `position` is seconds
/// from the start of the first segment.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub lengths: Vec<f64>,
    pub position: f64,
    pub opacity: f64,
}

/// Pixel span (x0, x1 exclusive) of each segment, with a small gap between them.
pub fn line_edges(lengths: &[f64], frame_w: u32) -> Vec<(i32, i32)> {
    let total: f64 = lengths.iter().sum();
    if total <= 0.0 {
        return Vec::new();
    }
    let fw = frame_w as f64;
    let gap = if lengths.len() > 1 { (fw / 640.0).round().max(2.0) as i32 } else { 0 };
    let mut edges = Vec::new();
    let mut start = 0.0;
    for length in lengths {
        let x0 = (fw * start / total).round() as i32;
        start += length;
        let x1 = (fw * start / total).round() as i32;
        edges.push((x0, x1));
    }
    // Cut the gap out of the end of every segment but the last.
    let last = edges.len() - 1;
    edges.into_iter().enumerate().map(|(i, (x0, x1))| (x0, if i < last { x0.max(x1 - gap) } else { x1 })).collect()
}

/// How many pixels of the line are filled; the line is redrawn only when this changes.
pub fn line_pixels(line: &Line, size: (u32, u32)) -> i32 {
    let total: f64 = line.lengths.iter().sum();
    if total <= 0.0 {
        return 0;
    }
    (size.0 as f64 * (line.position / total).clamp(0.0, 1.0)).round() as i32
}

/// The progress line across the bottom edge: each song a segment in its own
/// colour, faint ahead of the playhead and solid behind it.
pub fn render_line(line: &Line, s: &OverlaySettings, size: (u32, u32)) -> Canvas {
    let (frame_w, frame_h) = (size.0, size.1 as i32);
    let height = ((s.line_height as f64 * frame_h as f64 / 720.0).round() as i32).max(1);
    let alpha = line.opacity.clamp(0.0, 1.0);
    let top = frame_h - height;
    let mut layer = Canvas::new_at(0, top, frame_w, height as u32);
    let fill_to = line_pixels(line, size);
    for (i, (x0, x1)) in line_edges(&line.lengths, frame_w).into_iter().enumerate() {
        if x1 <= x0 {
            continue;
        }
        let color = segment_color(i, s);
        layer.rect(x0, top, x1 - 1, frame_h - 1, rgba(&color, 0.5 * alpha));
        if fill_to > x0 {
            layer.rect(x0, top, fill_to.min(x1) - 1, frame_h - 1, rgba(&color, alpha));
        }
    }
    layer
}

/// Sizes of the queue card at 720p and 100 % scale.
mod queue_base {
    pub const PADDING: f32 = 12.0;
    pub const HEADING: f32 = 11.0;
    pub const ROW: f32 = 14.0;
    pub const ROW_HEIGHT: f32 = 22.0;
    pub const DOT: f32 = 8.0;
    pub const MIN_WIDTH: f32 = 220.0;
}

#[derive(Clone, Debug, PartialEq)]
pub struct QueueItem {
    pub title: String,
    pub artist: String,
    /// countdown to when it starts, "3:42"
    pub time: String,
    pub color: String,
}

/// A small card listing what's queued after the song on air.
#[derive(Clone, Debug, PartialEq)]
pub struct QueueCard {
    pub items: Vec<QueueItem>,
    /// queued songs not listed
    pub more: usize,
    pub opacity: f64,
}

/// The queue card goes in the opposite corner to the song card: the other
/// edge (top/bottom) and the right side, or the left if the song card is right.
pub fn queue_corner(s: &OverlaySettings) -> (&'static str, &'static str) {
    let (vertical, horizontal) = s.corner();
    (
        if vertical == "top" { "bottom" } else { "top" },
        if horizontal == "right" { "left" } else { "right" },
    )
}

pub fn render_queue(card: &QueueCard, s: &OverlaySettings, size: (u32, u32)) -> Canvas {
    let (frame_w, frame_h) = (size.0 as f32, size.1 as f32);
    let k = frame_h / 720.0 * s.scale as f32 / 100.0;
    let px = |v: f32| round(v * k).max(1.0);
    let (pad, heading_px, row_px, row_height, dot, min_w) = (
        px(queue_base::PADDING),
        px(queue_base::HEADING),
        px(queue_base::ROW),
        px(queue_base::ROW_HEIGHT),
        px(queue_base::DOT),
        px(queue_base::MIN_WIDTH),
    );
    let f_heading = Face::new("heading", heading_px);
    let f_row = Face::new("status", row_px);
    let f_soft = Face::new("body", row_px);
    let dot_gap = round(8.0 * k);
    let col_gap = round(14.0 * k);

    let heading = spaced("ОЧЕРЕДЬ");
    let count = (card.items.len() + card.more).to_string();
    let prefix = "через ";
    let prefix_w = f_soft.width(prefix);
    let times_w = prefix_w + card.items.iter().map(|i| time_width(&i.time, &f_row)).fold(0.0, f32::max);
    let max_label = round(frame_w * 0.3) - 2.0 * pad - dot - dot_gap - col_gap - times_w;

    let rows: Vec<(&QueueItem, String, String)> = card
        .items
        .iter()
        .map(|item| {
            let title = fit(&item.title, &f_row, max_label);
            let rest = max_label - f_row.width(&title);
            let mut artist = String::new();
            if !item.artist.is_empty() && rest > f_soft.width("  ·  …") + 20.0 * k {
                artist = fit(&format!("  ·  {}", item.artist), &f_soft, rest);
            }
            (item, title, artist)
        })
        .collect();
    let more = if card.more > 0 { format!("и ещё {}", card.more) } else { String::new() };

    let label_w = rows
        .iter()
        .map(|(_, t, a)| f_row.width(t) + if a.is_empty() { 0.0 } else { f_soft.width(a) })
        .fold(0.0, f32::max);
    let row_w = dot + dot_gap + label_w + col_gap + times_w;
    let head_w = f_heading.width(&heading) + round(8.0 * k) + f_heading.width(&count);
    let card_w = min_w.max(round(2.0 * pad + row_w.max(head_w).max(f_soft.width(&more))));
    let head_h = round(heading_px * 1.9);
    let card_h = 2.0 * pad + head_h + row_height * (rows.len() + usize::from(!more.is_empty())) as f32;

    let margin = round(s.margin as f32 * frame_h / 720.0);
    let (vertical, horizontal) = queue_corner(s);
    let x = if horizontal == "left" { margin } else { frame_w - margin - card_w };
    let y = if vertical == "top" { margin } else { frame_h - margin - card_h };

    let mut layer = Canvas::new_at(
        x.floor() as i32 - 1,
        y.floor() as i32 - 1,
        (card_w + 3.0).ceil() as u32,
        (card_h + 3.0).ceil() as u32,
    );
    let stroke = if s.stroke { round(s.stroke_width as f32 * k).max(1.0) } else { 0.0 };
    layer.rounded_rect(
        x,
        y,
        x + card_w,
        y + card_h,
        round(s.radius as f32 * k * 0.7),
        Some(rgba(&s.background_color, s.background_opacity)),
        (stroke > 0.0).then(|| rgba(&s.stroke_color, s.stroke_opacity)),
        stroke,
    );

    let white = [255, 255, 255, 255];
    let soft = [255, 255, 255, 170];
    let faint = [255, 255, 255, 130];
    let (tx, mut ty) = (x + pad, y + pad);
    f_heading.draw(&mut layer, tx, ty, &heading, faint);
    f_heading.draw(&mut layer, tx + f_heading.width(&heading) + round(8.0 * k), ty, &count, white);
    ty += head_h;

    let ascent = f_row.ascent();
    for (item, title, artist) in &rows {
        let cy = ty + round(ascent * 0.62);
        let r = dot / 2.0;
        layer.ellipse(tx, cy - r, tx + dot, cy + r, rgba(&item.color, 1.0));
        let lx = tx + dot + dot_gap;
        f_row.draw(&mut layer, lx, ty, title, white);
        if !artist.is_empty() {
            f_soft.draw(&mut layer, lx + f_row.width(title), ty, artist, soft);
        }
        // Countdown to the song, right-aligned: "через 2:15".
        let time_x = x + card_w - pad - time_width(&item.time, &f_row);
        f_soft.draw(&mut layer, time_x - prefix_w, ty, prefix, faint);
        draw_time(&mut layer, time_x, ty, &item.time, &f_row, white);
        ty += row_height;
    }
    if !more.is_empty() {
        f_soft.draw(&mut layer, tx + dot + dot_gap, ty, &more, faint);
    }

    if card.opacity < 1.0 {
        layer.multiply_alpha(card.opacity as f32);
    }
    layer
}

/// An empty frame: what ffplayout shows while there is no card.
pub fn blank() -> Canvas {
    Canvas::new(2, 2)
}

/// The whole overlay frame: the progress line, then the queue card, then the song card.
pub fn compose(
    card: Option<&Card>,
    line: Option<&Line>,
    s: &OverlaySettings,
    size: (u32, u32),
    queue: Option<&QueueCard>,
) -> Canvas {
    let mut layers = Vec::new();
    if let Some(line) = line {
        layers.push(render_line(line, s, size));
    }
    if let Some(queue) = queue.filter(|q| !q.items.is_empty() && q.opacity > 0.0) {
        layers.push(render_queue(queue, s, size));
    }
    if let Some(card) = card {
        layers.push(render(card, s, size));
    }
    if layers.is_empty() {
        return blank();
    }
    let mut image = Canvas::new(size.0, size.1);
    for layer in &layers {
        image.overlay(layer);
    }
    image
}

/// Replace the card atomically, so ffplayout never reads half a file.
pub fn write(png: &[u8], target: &Path) -> Result<()> {
    let dir = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // A temp file of its own, so two writes at once can't rename each other's.
    let mut tmp = tempfile::Builder::new().prefix(&format!(".{name}.")).suffix(".tmp").tempfile_in(dir)?;
    tmp.write_all(png)?;
    tmp.as_file().set_permissions(std::fs::Permissions::from_mode(0o644))?;
    tmp.persist(target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> OverlaySettings {
        OverlaySettings::default()
    }

    fn hex(p: Rgba) -> String {
        format!("#{:02x}{:02x}{:02x}", p[0], p[1], p[2])
    }

    #[test]
    fn render_draws_card_in_corner() {
        let card = Card {
            heading: "Now playing".into(),
            title: "Get Lucky".into(),
            artist: "Daft Punk".into(),
            status: "Next song in {time}".into(),
            time: "3:42".into(),
            progress: Some(0.4),
            thumb: None,
            opacity: 1.0,
        };
        let image = render(&card, &s(), (1280, 720));
        assert!(image.w < 700 && image.h < 400, "a layer as big as the card");
        let b = image.bbox().unwrap();
        assert!(b.0 < 100 && b.3 > 600, "{b:?}"); // bottom-left
        let top_right = OverlaySettings { position: "top-right".into(), ..s() };
        let b = render(&card, &top_right, (1280, 720)).bbox().unwrap();
        assert!(b.2 > 1180 && b.1 < 100, "{b:?}");
    }

    #[test]
    fn render_fades() {
        let card = Card {
            heading: "Up next".into(),
            title: "Song".into(),
            artist: String::new(),
            status: "Starts in {time}".into(),
            time: "0:10".into(),
            progress: None,
            thumb: Some(PathBuf::from("/nope.jpg")),
            opacity: 0.5,
        };
        let max = render(&card, &s(), (1280, 720)).max_alpha();
        assert!(max > 0 && max <= 128, "{max}");
    }

    #[test]
    fn compose_draws_full_width_line_at_the_bottom() {
        let line = Line { lengths: vec![200.0], position: 50.0, opacity: 1.0 };
        let image = compose(None, Some(&line), &s(), (1280, 720), None);
        assert_eq!(image.bbox(), Some((0, 716, 1280, 720)));
        assert_eq!(hex(image.pixel(100, 718)), "#ff5c7a"); // filled: accent
        assert!(image.pixel(1000, 718)[3] < 140); // rest: faint track
        let empty = compose(None, None, &s(), (1280, 720), None);
        assert_eq!((empty.w, empty.h), (2, 2));
    }

    #[test]
    fn line_has_a_segment_per_queued_song() {
        // On air 200 s (half played), then queued songs of 100 s and 100 s.
        let line = Line { lengths: vec![200.0, 100.0, 100.0], position: 100.0, opacity: 1.0 };
        let image = compose(None, Some(&line), &s(), (1280, 720), None);
        let edges = line_edges(&line.lengths, 1280);
        assert_eq!(edges.iter().map(|e| e.0).collect::<Vec<_>>(), [0, 640, 960]);
        assert_eq!(edges[2].1, 1280);
        assert!(edges[0].1 < 640); // a gap between segments
        assert_eq!(image.pixel(100, 718), [0xff, 0x5c, 0x7a, 255]); // played part of the song on air
        assert_eq!(hex(image.pixel(500, 718)), "#ff5c7a"); // rest of it: faint accent
        assert!(image.pixel(500, 718)[3] < 140);
        let second = image.pixel(800, 718);
        let third = image.pixel(1100, 718);
        assert_eq!(hex(second), SEGMENT_COLORS[0]);
        assert!(second[3] < 140);
        assert_eq!(hex(third), SEGMENT_COLORS[1]);
        assert_eq!(image.pixel(edges[0].1 as u32, 718)[3], 0); // gap
    }

    #[test]
    fn one_song_is_one_unbroken_line() {
        assert_eq!(line_edges(&[180.0], 1280), [(0, 1280)]);
    }

    fn item(title: &str, artist: &str, time: &str, color: &str) -> QueueItem {
        QueueItem { title: title.into(), artist: artist.into(), time: time.into(), color: color.into() }
    }

    #[test]
    fn queue_card_lists_countdowns() {
        let items = vec![item("Песня", "Исполнитель", "2:15", SEGMENT_COLORS[0]), item("Ещё", "", "5:20", SEGMENT_COLORS[1])];
        let image = render_queue(&QueueCard { items, more: 0, opacity: 0.4 }, &s(), (1280, 720));
        assert!(image.bbox().is_some());
    }

    #[test]
    fn queue_card_sits_opposite_the_song_card() {
        let queue = QueueCard { items: vec![item("Song", "Artist", "3:00", SEGMENT_COLORS[0])], more: 2, opacity: 1.0 };
        let b = render_queue(&queue, &s(), (1280, 720)).bbox().unwrap(); // song card bottom-left
        assert!(b.2 > 1180 && b.1 < 100, "{b:?}"); // queue top-right
        let top_right = OverlaySettings { position: "top-right".into(), ..s() };
        let b = render_queue(&queue, &top_right, (1280, 720)).bbox().unwrap();
        assert!(b.0 < 100 && b.3 > 600, "{b:?}"); // queue bottom-left
        let dim = render_queue(&QueueCard { more: 0, opacity: 0.4, ..queue }, &s(), (1280, 720));
        assert!(dim.max_alpha() <= 103);
    }

    /// Writes sample frames to $SAMPLE_DIR for eyeballing:
    /// `SAMPLE_DIR=/tmp/x cargo test write_samples -- --ignored`
    #[test]
    #[ignore]
    fn write_samples() {
        let dir = PathBuf::from(std::env::var("SAMPLE_DIR").expect("SAMPLE_DIR"));
        let thumb = std::env::var("SAMPLE_THUMB").ok().map(PathBuf::from);
        let now = Card {
            heading: "Сейчас играет".into(),
            title: "Bohemian Rhapsody".into(),
            artist: "Queen".into(),
            status: "Следующая песня через {time}".into(),
            time: "4:51".into(),
            progress: Some(0.12),
            thumb: thumb.clone(),
            opacity: 1.0,
        };
        let next = Card {
            heading: "Далее".into(),
            title: "A very long song title that will not fit on the card at all, really".into(),
            artist: "Modern Talking".into(),
            status: "Начнётся через {time}".into(),
            time: "0:18".into(),
            progress: None,
            thumb,
            opacity: 0.6,
        };
        let queue = QueueCard {
            items: vec![
                item("Cheri Cheri Lady", "Modern Talking", "2:15", SEGMENT_COLORS[0]),
                item("Ещё одна песня", "", "5:20", SEGMENT_COLORS[1]),
            ],
            more: 3,
            opacity: 1.0,
        };
        let line = Line { lengths: vec![200.0, 100.0, 140.0], position: 80.0, opacity: 1.0 };
        let size = (1280, 720);
        let frames = [
            ("now", compose(Some(&now), None, &s(), size, Some(&queue))),
            ("next", compose(Some(&next), None, &s(), size, None)),
            ("line", compose(None, Some(&line), &s(), size, Some(&QueueCard { opacity: 0.4, ..queue.clone() }))),
        ];
        for (name, image) in frames {
            std::fs::write(dir.join(format!("rust-{name}.png")), image.to_png()).unwrap();
        }
    }

    #[test]
    fn concurrent_writes_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("card.png");
        let mut image = Canvas::new(64, 36);
        image.rect(0, 0, 63, 35, [255, 0, 0, 128]);
        let png = image.to_png();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..25 {
                        write(&png, &target).unwrap();
                    }
                });
            }
        });
        let decoded = image::open(&target).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (64, 36));
        let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["card.png"]);
    }
}
