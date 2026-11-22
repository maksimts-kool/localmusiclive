//! Which TV channel a recorded commercial aired on, from the channel logo in
//! the top-right corner of the picture.
//!
//! Sixteen frames of that corner are turned into edge maps and averaged: the
//! picture moves and blurs out, a logo stays put and stands out. The result is
//! compared with templates made the same way from commercials whose channel is
//! known (logos/*.png), allowing a few pixels of shift.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use crate::util::run;

const W: usize = 160;
const H: usize = 90;
const FRAMES: usize = 16;
/// How much the corner of a commercial must resemble a logo template. Measured
/// on 25 commercials: those with the logo scored 0.73-0.99, the rest 0.37 or less.
const THRESHOLD: f32 = 0.55;
const SHIFT: i32 = 4;

pub const PERVIY: &str = "perviy";
pub const ROSSIYA1: &str = "rossiya1";

struct Template {
    channel: &'static str,
    z: Vec<f32>,
    /// x0, y0, x1, y1 (inclusive): around the logo.
    bbox: (usize, usize, usize, usize),
}

fn zscore(map: &[f32]) -> Vec<f32> {
    let n = map.len() as f32;
    let mean = map.iter().sum::<f32>() / n;
    let sd = (map.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / n).sqrt();
    let sd = if sd > 0.0 { sd } else { 1.0 };
    map.iter().map(|v| (v - mean) / sd).collect()
}

fn template(channel: &'static str, png: &[u8]) -> Template {
    let image = image::load_from_memory(png).expect("logo template").to_luma8();
    assert_eq!((image.width() as usize, image.height() as usize), (W, H));
    let z = zscore(&image.pixels().map(|p| p.0[0] as f32).collect::<Vec<_>>());
    let max = z.iter().cloned().fold(f32::MIN, f32::max);
    let (mut x0, mut y0, mut x1, mut y1) = (W, H, 0, 0);
    for (i, v) in z.iter().enumerate() {
        if *v > 0.35 * max {
            let (x, y) = (i % W, i / W);
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
    }
    let pad = 3;
    let bbox = (
        x0.saturating_sub(pad).max(SHIFT as usize),
        y0.saturating_sub(pad).max(SHIFT as usize),
        (x1 + pad).min(W - 1 - SHIFT as usize),
        (y1 + pad).min(H - 1 - SHIFT as usize),
    );
    Template { channel, z, bbox }
}

fn templates() -> &'static [Template] {
    static CELL: OnceLock<Vec<Template>> = OnceLock::new();
    CELL.get_or_init(|| {
        vec![
            template(PERVIY, include_bytes!("logos/perviy.png")),
            template(ROSSIYA1, include_bytes!("logos/rossiya1.png")),
        ]
    })
}

/// Average edge strength of `frames` (gray, W×H each).
pub fn edge_map(frames: &[u8]) -> Option<Vec<f32>> {
    let n = frames.len() / (W * H);
    if n == 0 {
        return None;
    }
    let mut acc = vec![0f32; W * H];
    for frame in frames.chunks_exact(W * H) {
        for y in 1..H - 1 {
            for x in 1..W - 1 {
                let i = y * W + x;
                let gx = frame[i + 1] as f32 - frame[i - 1] as f32;
                let gy = frame[i + W] as f32 - frame[i - W] as f32;
                acc[i] += (gx * gx + gy * gy).sqrt();
            }
        }
    }
    Some(acc.into_iter().map(|v| v / n as f32).collect())
}

/// Normalised cross-correlation with the template around its logo, at the best shift.
fn score(map: &[f32], t: &Template) -> f32 {
    let (x0, y0, x1, y1) = t.bbox;
    let mut best = f32::MIN;
    for dy in -SHIFT..=SHIFT {
        for dx in -SHIFT..=SHIFT {
            let (mut a, mut b) = (Vec::new(), Vec::new());
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let (xx, yy) = ((x as i32 + dx) as usize, (y as i32 + dy) as usize);
                    a.push(map[yy * W + xx]);
                    b.push(t.z[y * W + x]);
                }
            }
            let n = a.len() as f32;
            let (ma, mb) = (a.iter().sum::<f32>() / n, b.iter().sum::<f32>() / n);
            let (mut num, mut da, mut db) = (0f32, 0f32, 0f32);
            for (p, q) in a.iter().zip(&b) {
                num += (p - ma) * (q - mb);
                da += (p - ma).powi(2);
                db += (q - mb).powi(2);
            }
            if da > 0.0 && db > 0.0 {
                best = best.max(num / (da.sqrt() * db.sqrt()));
            }
        }
    }
    best
}

/// The channel whose logo the edge map shows, if any.
pub fn detect(map: &[f32]) -> Option<&'static str> {
    templates()
        .iter()
        .map(|t| (t.channel, score(map, t)))
        .filter(|(_, s)| *s >= THRESHOLD)
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(channel, _)| channel)
}

/// The channel of a video file of `duration` seconds; None if no known logo
/// (or the video could not be read).
pub async fn channel_of(path: &Path, duration: f64) -> Option<&'static str> {
    let span = (duration - 1.0).max(1.0);
    let filter = format!("fps={FRAMES}/{span:.3},scale=640:360,crop={W}:{H}:{}:0,format=gray", 640 - W);
    let args: Vec<String> = [
        "-v", "error", "-ss", "0.5", "-i", &path.to_string_lossy(), "-t", &format!("{span:.3}"),
        "-vf", &filter, "-frames:v", &FRAMES.to_string(), "-f", "rawvideo", "-",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let out = run("ffmpeg", &args, Duration::from_secs(120), false).await;
    if out.code != 0 {
        return None;
    }
    detect(&edge_map(&out.stdout)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(png: &[u8]) -> Vec<f32> {
        image::load_from_memory(png).unwrap().to_luma8().pixels().map(|p| p.0[0] as f32).collect()
    }

    #[test]
    fn templates_match_themselves() {
        for t in templates() {
            let (x0, y0, x1, y1) = t.bbox;
            assert!(x0 < x1 && y0 < y1, "{} bbox {:?}", t.channel, t.bbox);
            assert_eq!(detect(&t.z), Some(t.channel));
        }
    }

    /// Edge maps of real commercials (logos/samples): Первый, Россия 1, and
    /// two with other marks in the corner (Пятый канал's logo, an 18+ badge).
    #[test]
    fn recognises_real_commercials() {
        assert_eq!(detect(&sample(include_bytes!("logos/samples/perviy.png"))), Some(PERVIY));
        assert_eq!(detect(&sample(include_bytes!("logos/samples/rossiya1.png"))), Some(ROSSIYA1));
        assert_eq!(detect(&sample(include_bytes!("logos/samples/channel5.png"))), None);
        assert_eq!(detect(&sample(include_bytes!("logos/samples/none.png"))), None);
    }

    #[test]
    fn edge_map_averages_frames() {
        let mut frames = vec![0u8; W * H * 2];
        frames[W * 10 + 11] = 100; // first frame: one bright pixel
        let map = edge_map(&frames).unwrap();
        assert_eq!(map[W * 10 + 10], 50.0); // |gx| = 100, averaged over 2 frames
        assert!(edge_map(&[]).is_none());
    }

    /// Scores real corners extracted as in channel_of: LOGO_FRAMES=dir with <id>.gray files.
    #[test]
    #[ignore]
    fn scores_extracted_corners() {
        let dir = std::env::var("LOGO_FRAMES").expect("LOGO_FRAMES");
        let mut paths: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            let map = edge_map(&std::fs::read(&path).unwrap()).unwrap();
            let scores: Vec<String> = templates().iter().map(|t| format!("{}={:.2}", t.channel, score(&map, t))).collect();
            println!("{} {:?} {}", path.display(), detect(&map), scores.join(" "));
        }
    }
}
