//! A small RGBA canvas with the drawing the song card needs: anti-aliased
//! rounded boxes and dots, text, blur and compositing.
//!
//! Drawing follows Pillow's ImageDraw, which the card was designed with:
//! painting moves every channel, alpha included, toward the ink by the
//! coverage (so a half-transparent line on an opaque box leaves a
//! half-transparent line). Whole layers are combined with ordinary
//! "source over" compositing, like Pillow's alpha_composite.

use std::collections::HashMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ab_glyph::{point, Font, FontArc, GlyphId, PxScale, ScaleFont};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::ImageEncoder;

pub type Rgba = [u8; 4];

/// An RGBA image covering x ox..ox+w, y oy..oy+h of the video frame. All
/// drawing takes frame coordinates and clips to that area, so a layer can be
/// just as big as what it shows.
#[derive(Clone)]
pub struct Canvas {
    pub w: u32,
    pub h: u32,
    pub ox: i32,
    pub oy: i32,
    pub px: Vec<u8>,
}

/// Signed distance from (x, y) to a rounded box; negative inside.
fn box_distance(x: f32, y: f32, l: f32, t: f32, r: f32, b: f32, radius: f32) -> f32 {
    let (cx, cy) = ((l + r) / 2.0, (t + b) / 2.0);
    let (hx, hy) = ((r - l) / 2.0, (b - t) / 2.0);
    let radius = radius.min(hx).min(hy).max(0.0);
    let qx = (x - cx).abs() - hx + radius;
    let qy = (y - cy).abs() - hy + radius;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

fn coverage(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

impl Canvas {
    /// A whole frame.
    pub fn new(w: u32, h: u32) -> Canvas {
        Canvas::new_at(0, 0, w, h)
    }

    /// The part of the frame from (ox, oy), w × h pixels.
    pub fn new_at(ox: i32, oy: i32, w: u32, h: u32) -> Canvas {
        Canvas { w, h, ox, oy, px: vec![0; (w as usize) * (h as usize) * 4] }
    }

    /// Frame columns and rows this canvas covers (end exclusive).
    fn xs(&self) -> (i32, i32) {
        (self.ox, self.ox + self.w as i32)
    }

    fn ys(&self) -> (i32, i32) {
        (self.oy, self.oy + self.h as i32)
    }

    fn index(&self, x: i32, y: i32) -> Option<usize> {
        let (lx, ly) = (x - self.ox, y - self.oy);
        if lx < 0 || ly < 0 || lx >= self.w as i32 || ly >= self.h as i32 {
            return None;
        }
        Some((ly as usize * self.w as usize + lx as usize) * 4)
    }

    /// The pixel at frame position (x, y); transparent outside the canvas.
    pub fn pixel(&self, x: u32, y: u32) -> Rgba {
        match self.index(x as i32, y as i32) {
            Some(i) => [self.px[i], self.px[i + 1], self.px[i + 2], self.px[i + 3]],
            None => [0, 0, 0, 0],
        }
    }

    /// Moves the pixel toward `ink` by `cov` (0..1), alpha included.
    pub fn paint(&mut self, x: i32, y: i32, ink: Rgba, cov: f32) {
        if cov <= 0.0 {
            return;
        }
        let Some(i) = self.index(x, y) else { return };
        if cov >= 1.0 {
            self.px[i..i + 4].copy_from_slice(&ink);
        } else {
            for c in 0..4 {
                let old = self.px[i + c] as f32;
                self.px[i + c] = (old + (ink[c] as f32 - old) * cov).round() as u8;
            }
        }
    }

    /// Fills the box with corners (x0, y0) and (x1, y1), both included.
    pub fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, ink: Rgba) {
        let ((cx0, cx1), (cy0, cy1)) = (self.xs(), self.ys());
        for y in y0.max(cy0)..=y1.min(cy1 - 1) {
            for x in x0.max(cx0)..=x1.min(cx1 - 1) {
                self.paint(x, y, ink, 1.0);
            }
        }
    }

    /// A rounded box covering pixels x0..=x1, y0..=y1 (Pillow's box), with an
    /// optional outline `width` pixels wide drawn inside its edge.
    #[allow(clippy::too_many_arguments)]
    pub fn rounded_rect(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        radius: f32,
        fill: Option<Rgba>,
        outline: Option<Rgba>,
        width: f32,
    ) {
        let (l, t, r, b) = (x0, y0, x1 + 1.0, y1 + 1.0);
        if r <= l || b <= t {
            return;
        }
        let outline = outline.filter(|_| width > 0.0);
        let ((cx0, cx1), (cy0, cy1)) = (self.xs(), self.ys());
        for y in (t.floor() as i32).max(cy0)..(b.ceil() as i32).min(cy1) {
            for x in (l.floor() as i32).max(cx0)..(r.ceil() as i32).min(cx1) {
                let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
                let d = box_distance(cx, cy, l, t, r, b, radius);
                let outer = coverage(d);
                if outer <= 0.0 {
                    continue;
                }
                if let Some(ink) = fill {
                    self.paint(x, y, ink, outer);
                }
                if let Some(ink) = outline {
                    let inner = coverage(box_distance(
                        cx,
                        cy,
                        l + width,
                        t + width,
                        r - width,
                        b - width,
                        (radius - width).max(0.0),
                    ));
                    let inner = if r - l <= 2.0 * width || b - t <= 2.0 * width { 0.0 } else { inner };
                    self.paint(x, y, ink, (outer - inner).max(0.0));
                }
            }
        }
    }

    /// A filled ellipse in the box x0..=x1, y0..=y1.
    pub fn ellipse(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, ink: Rgba) {
        let (rx, ry) = ((x1 + 1.0 - x0) / 2.0, (y1 + 1.0 - y0) / 2.0);
        let (cx, cy) = (x0 + rx, y0 + ry);
        if rx <= 0.0 || ry <= 0.0 {
            return;
        }
        let ((cx0, cx1), (cy0, cy1)) = (self.xs(), self.ys());
        for y in (y0.floor() as i32).max(cy0)..((y1 + 1.0).ceil() as i32).min(cy1) {
            for x in (x0.floor() as i32).max(cx0)..((x1 + 1.0).ceil() as i32).min(cx1) {
                let (dx, dy) = ((x as f32 + 0.5 - cx) / rx, (y as f32 + 0.5 - cy) / ry);
                let d = ((dx * dx + dy * dy).sqrt() - 1.0) * rx.min(ry);
                self.paint(x, y, ink, coverage(d));
            }
        }
    }

    /// Puts `src` over this canvas with src's first pixel at frame position (dx, dy).
    pub fn composite(&mut self, src: &Canvas, dx: i32, dy: i32) {
        let ((cx0, cx1), (cy0, cy1)) = (self.xs(), self.ys());
        let (x0, x1) = (dx.max(cx0), (dx + src.w as i32).min(cx1));
        let (y0, y1) = (dy.max(cy0), (dy + src.h as i32).min(cy1));
        for y in y0..y1 {
            for x in x0..x1 {
                let si = ((y - dy) as usize * src.w as usize + (x - dx) as usize) * 4;
                let s = [src.px[si], src.px[si + 1], src.px[si + 2], src.px[si + 3]];
                if s[3] == 0 {
                    continue;
                }
                let i = self.index(x, y).expect("clipped to the canvas");
                if s[3] == 255 {
                    self.px[i..i + 4].copy_from_slice(&s);
                    continue;
                }
                let sa = s[3] as f32 / 255.0;
                let da = self.px[i + 3] as f32 / 255.0;
                let oa = sa + da * (1.0 - sa);
                for c in 0..3 {
                    let value = (s[c] as f32 * sa + self.px[i + c] as f32 * da * (1.0 - sa)) / oa;
                    self.px[i + c] = value.round().clamp(0.0, 255.0) as u8;
                }
                self.px[i + 3] = (oa * 255.0).round() as u8;
            }
        }
    }

    /// Puts a layer over this canvas where it belongs in the frame.
    pub fn overlay(&mut self, layer: &Canvas) {
        self.composite(layer, layer.ox, layer.oy);
    }

    pub fn multiply_alpha(&mut self, factor: f32) {
        let factor = factor.max(0.0);
        for a in self.px.iter_mut().skip(3).step_by(4) {
            *a = (*a as f32 * factor).round().min(255.0) as u8;
        }
    }

    /// Gaussian blur of the alpha channel (the shadow is black, so colour
    /// doesn't matter), as three box blurs. Spreads only within the canvas.
    pub fn blur_alpha(&mut self, sigma: f32) {
        if sigma <= 0.0 {
            return;
        }
        let Some((bx0, by0, bx1, by1)) = self.local_bbox() else { return };
        let spread = (sigma * 3.0).ceil() as usize + 2;
        let x0 = bx0.saturating_sub(spread);
        let y0 = by0.saturating_sub(spread);
        let x1 = (bx1 + spread).min(self.w as usize);
        let y1 = (by1 + spread).min(self.h as usize);
        let (w, h) = (x1 - x0, y1 - y0);
        let mut a: Vec<f32> = Vec::with_capacity(w * h);
        for y in y0..y1 {
            for x in x0..x1 {
                a.push(self.px[(y * self.w as usize + x) * 4 + 3] as f32);
            }
        }
        let mut tmp = vec![0.0f32; w * h];
        for size in box_sizes(sigma) {
            let radius = (size - 1) / 2;
            box_pass(&a, &mut tmp, w, h, radius, true);
            box_pass(&tmp, &mut a, w, h, radius, false);
        }
        for y in 0..h {
            for x in 0..w {
                let i = ((y + y0) * self.w as usize + x + x0) * 4;
                self.px[i] = 0;
                self.px[i + 1] = 0;
                self.px[i + 2] = 0;
                self.px[i + 3] = a[y * w + x].round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    fn local_bbox(&self) -> Option<(usize, usize, usize, usize)> {
        let (w, h) = (self.w as usize, self.h as usize);
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0, 0);
        for y in 0..h {
            for x in 0..w {
                if self.px[(y * w + x) * 4 + 3] != 0 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x + 1);
                    y1 = y1.max(y + 1);
                }
            }
        }
        (x0 != usize::MAX).then_some((x0, y0, x1, y1))
    }

    /// Bounding box in the frame (x0, y0, x1, y1; end exclusive) of the
    /// visible pixels, clipped to the frame's top-left.
    pub fn bbox(&self) -> Option<(u32, u32, u32, u32)> {
        let (x0, y0, x1, y1) = self.local_bbox()?;
        let frame = |v: usize, o: i32| (v as i32 + o).max(0) as u32;
        Some((frame(x0, self.ox), frame(y0, self.oy), frame(x1, self.ox), frame(y1, self.oy)))
    }

    pub fn max_alpha(&self) -> u8 {
        self.px.iter().skip(3).step_by(4).copied().max().unwrap_or(0)
    }

    pub fn to_png(&self) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Sub)
            .write_image(&self.px, self.w, self.h, image::ExtendedColorType::Rgba8)
            .expect("PNG encoding into memory");
        out.into_inner()
    }
}

/// Box widths whose three passes approximate a Gaussian of `sigma`.
fn box_sizes(sigma: f32) -> [usize; 3] {
    let n = 3.0;
    let ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut wl = ideal.floor() as i32;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wl = wl.max(1);
    let wu = wl + 2;
    let wlf = wl as f32;
    let m = ((12.0 * sigma * sigma - n * wlf * wlf - 4.0 * n * wlf - 3.0 * n) / (-4.0 * wlf - 4.0)).round() as i32;
    let size = |i: i32| if i < m { wl as usize } else { wu as usize };
    [size(0), size(1), size(2)]
}

fn box_pass(src: &[f32], dst: &mut [f32], w: usize, h: usize, radius: usize, horizontal: bool) {
    let (lines, len) = if horizontal { (h, w) } else { (w, h) };
    let at = |line: usize, i: usize| if horizontal { line * w + i } else { i * w + line };
    let r = radius as isize;
    let norm = 1.0 / (2 * radius + 1) as f32;
    for line in 0..lines {
        let mut sum = 0.0f32;
        for i in -r..=r {
            if i >= 0 && (i as usize) < len {
                sum += src[at(line, i as usize)];
            }
        }
        for i in 0..len {
            dst[at(line, i)] = sum * norm;
            let out = i as isize - r;
            let inn = i as isize + r + 1;
            if out >= 0 {
                sum -= src[at(line, out as usize)];
            }
            if (inn as usize) < len {
                sum += src[at(line, inn as usize)];
            }
        }
    }
}

// --- text ---------------------------------------------------------------------

pub fn font_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARD_FONT_DIR").unwrap_or_else(|_| "/usr/share/fonts/opentype/inter".into()))
}

/// Font files for each kind of text, best first.
fn font_files(kind: &str) -> &'static [&'static str] {
    match kind {
        "heading" => &["Inter-SemiBold.otf", "Inter-Medium.otf", "Inter-Regular.otf"],
        "title" => &["InterDisplay-Bold.otf", "Inter-Bold.otf", "Inter-SemiBold.otf", "Inter-Regular.otf"],
        "status" => &["Inter-Medium.otf", "Inter-Regular.otf"],
        _ => &["Inter-Regular.otf"],
    }
}

fn load_font(kind: &str) -> Option<FontArc> {
    let dir = font_dir();
    for file in font_files(kind) {
        if let Ok(bytes) = std::fs::read(dir.join(file)) {
            if let Ok(font) = FontArc::try_from_vec(bytes) {
                return Some(font);
            }
        }
    }
    tracing::warn!("No font for the song card's {kind} text in {}", dir.display());
    None
}

fn font(kind: &'static str) -> Option<FontArc> {
    static FONTS: OnceLock<Mutex<HashMap<&'static str, Option<FontArc>>>> = OnceLock::new();
    let fonts = FONTS.get_or_init(Default::default);
    let mut fonts = fonts.lock().unwrap_or_else(|e| e.into_inner());
    fonts.entry(kind).or_insert_with(|| load_font(kind)).clone()
}

/// A font at a pixel size (the em square, like Pillow's truetype size).
pub struct Face {
    font: Option<FontArc>,
    scale: PxScale,
}

impl Face {
    pub fn new(kind: &'static str, size: f32) -> Face {
        let font = font(kind);
        let scale = match &font {
            Some(f) => {
                let units = f.units_per_em().unwrap_or(1000.0);
                PxScale::from(size * f.height_unscaled() / units)
            }
            None => PxScale::from(size),
        };
        Face { font, scale }
    }

    pub fn ascent(&self) -> f32 {
        self.font.as_ref().map(|f| f.as_scaled(self.scale).ascent()).unwrap_or(0.0)
    }

    fn glyphs(&self, text: &str) -> Vec<(GlyphId, f32)> {
        let Some(font) = &self.font else { return Vec::new() };
        let scaled = font.as_scaled(self.scale);
        let mut x = 0.0;
        let mut prev: Option<GlyphId> = None;
        let mut out = Vec::new();
        for ch in text.chars() {
            let id = scaled.glyph_id(ch);
            if let Some(prev) = prev {
                x += scaled.kern(prev, id);
            }
            out.push((id, x));
            x += scaled.h_advance(id);
            prev = Some(id);
        }
        out.push((GlyphId(0), x));
        out
    }

    pub fn width(&self, text: &str) -> f32 {
        self.glyphs(text).last().map(|g| g.1).unwrap_or(0.0)
    }

    /// Draws `text` with its top (the ascender line) at y, like Pillow's default anchor.
    pub fn draw(&self, canvas: &mut Canvas, x: f32, y: f32, text: &str, ink: Rgba) {
        let Some(font) = &self.font else { return };
        let baseline = y + self.ascent();
        let glyphs = self.glyphs(text);
        for (id, offset) in &glyphs[..glyphs.len().saturating_sub(1)] {
            let glyph = id.with_scale_and_position(self.scale, point(x + offset, baseline));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, c| {
                    canvas.paint(bounds.min.x as i32 + gx as i32, bounds.min.y as i32 + gy as i32, ink, c);
                });
            }
        }
    }
}

// --- images -------------------------------------------------------------------

/// A JPEG thumbnail scaled to 16:9 at `height`, with rounded corners. Cached.
pub fn thumbnail(path: &Path, height: u32, radius: f32, mtime: f64) -> Option<Canvas> {
    type Key = (PathBuf, u32, u32, u64);
    static CACHE: OnceLock<Mutex<HashMap<Key, Option<Canvas>>>> = OnceLock::new();
    let key = (path.to_path_buf(), height, radius.to_bits(), mtime.to_bits());
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return hit.clone();
    }
    let made = make_thumbnail(path, height, radius);
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= 64 {
        cache.clear();
    }
    cache.insert(key, made.clone());
    made
}

fn make_thumbnail(path: &Path, height: u32, radius: f32) -> Option<Canvas> {
    let image = image::open(path).ok()?.to_rgba8();
    let width = (height as f32 * 16.0 / 9.0).round() as u32;
    let image = image::imageops::resize(&image, width, height, image::imageops::FilterType::Lanczos3);
    let mut canvas = Canvas { w: width, h: height, ox: 0, oy: 0, px: image.into_raw() };
    let (r, b) = (width as f32, height as f32);
    for y in 0..height {
        for x in 0..width {
            let cov = coverage(box_distance(x as f32 + 0.5, y as f32 + 0.5, 0.0, 0.0, r, b, radius));
            let i = ((y * width + x) * 4 + 3) as usize;
            canvas.px[i] = (cov * 255.0).round() as u8;
        }
    }
    Some(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounded_box_is_solid_inside_and_soft_at_the_corner() {
        let mut c = Canvas::new(40, 40);
        c.rounded_rect(5.0, 5.0, 34.0, 34.0, 8.0, Some([255, 0, 0, 200]), None, 0.0);
        assert_eq!(c.pixel(20, 20), [255, 0, 0, 200]);
        assert_eq!(c.pixel(5, 20), [255, 0, 0, 200]);
        assert_eq!(c.pixel(34, 20), [255, 0, 0, 200]);
        assert_eq!(c.pixel(35, 20)[3], 0);
        assert_eq!(c.pixel(5, 5)[3], 0);
        assert_eq!(c.bbox(), Some((5, 5, 35, 35)));
    }

    #[test]
    fn outline_replaces_the_fill_at_the_edge() {
        let mut c = Canvas::new(40, 40);
        c.rounded_rect(0.0, 0.0, 39.0, 39.0, 0.0, Some([0, 0, 0, 230]), Some([255, 255, 255, 41]), 2.0);
        assert_eq!(c.pixel(0, 20), [255, 255, 255, 41]);
        assert_eq!(c.pixel(1, 20), [255, 255, 255, 41]);
        assert_eq!(c.pixel(2, 20), [0, 0, 0, 230]);
    }

    #[test]
    fn blur_spreads_alpha_and_keeps_it_bounded() {
        let mut c = Canvas::new(100, 100);
        c.rect(40, 40, 59, 59, [0, 0, 0, 110]);
        c.blur_alpha(6.0);
        assert!(c.pixel(50, 50)[3] > 60 && c.pixel(50, 50)[3] <= 110);
        assert!(c.pixel(30, 50)[3] > 0);
        assert_eq!(c.pixel(2, 2)[3], 0);
    }

    #[test]
    fn composite_over() {
        let mut dst = Canvas::new(1, 1);
        dst.rect(0, 0, 0, 0, [0, 0, 255, 255]);
        let mut src = Canvas::new(1, 1);
        src.rect(0, 0, 0, 0, [255, 0, 0, 128]);
        dst.composite(&src, 0, 0);
        let p = dst.pixel(0, 0);
        assert_eq!(p[3], 255);
        assert!(p[0] == 128 && p[2] == 127);
    }

    #[test]
    fn a_layer_draws_in_frame_coordinates() {
        let mut layer = Canvas::new_at(100, 50, 20, 10);
        layer.rect(0, 0, 1000, 1000, [255, 0, 0, 255]);
        assert_eq!(layer.bbox(), Some((100, 50, 120, 60)));
        assert_eq!(layer.pixel(105, 55), [255, 0, 0, 255]);
        let mut frame = Canvas::new(200, 100);
        frame.overlay(&layer);
        assert_eq!(frame.bbox(), Some((100, 50, 120, 60)));
        let mut edge = Canvas::new_at(-5, -5, 10, 10);
        edge.rect(-5, -5, 4, 4, [0, 0, 0, 255]);
        frame.overlay(&edge);
        assert_eq!(frame.pixel(0, 0)[3], 255);
    }

    #[test]
    fn box_sizes_grow_with_sigma() {
        let small = box_sizes(2.0);
        let big = box_sizes(18.0);
        assert!(small.iter().all(|s| s % 2 == 1));
        assert!(big[0] > small[0]);
    }
}
