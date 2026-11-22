//! Turns the queue into ffplayout's daily playlists.
//!
//! ffplayout plays one JSON playlist per day and finds "now" by wall clock: a
//! clip's start time is the day start plus the lengths of all clips before it.
//! So the panel never edits what already played. It keeps every slot up to and
//! including the current one exactly as written and rewrites only what follows.
//!
//! The clip that crosses the day boundary is split in two: the first part closes
//! today's playlist and the second part opens tomorrow's, so playback continues
//! through midnight without a cut.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DAY: f64 = 86400.0;
/// ffplayout skips clips shorter than one second.
pub const MIN_PART: f64 = 1.0;
/// A clip may overrun the day end by this much instead of being split.
const OVERRUN_TOLERANCE: f64 = 0.5;

pub fn r3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Slot {
    /// "gap" | "queue" | "auto" | "ad"
    pub kind: String,
    /// What ffplayout plays; "" makes ffplayout generate black/silence.
    pub path: String,
    /// ffplayout "in"
    pub seek: f64,
    /// ffplayout "out"
    pub out: f64,
    /// Full media length.
    pub duration: f64,
    #[serde(default)]
    pub track_id: Option<i64>,
    #[serde(default)]
    pub queue_id: Option<i64>,
    #[serde(default)]
    pub auto_id: Option<i64>,
    #[serde(default)]
    pub title: String,
    /// Set on the first part of a split clip.
    #[serde(default)]
    pub full_out: Option<f64>,
    /// 1 on the second part of a split clip.
    #[serde(default)]
    pub part: i64,
}

impl Slot {
    pub fn new(kind: &str, path: &str, seek: f64, out: f64, duration: f64) -> Slot {
        Slot {
            kind: kind.into(),
            path: path.into(),
            seek,
            out,
            duration,
            track_id: None,
            queue_id: None,
            auto_id: None,
            title: String::new(),
            full_out: None,
            part: 0,
        }
    }

    pub fn length(&self) -> f64 {
        self.out - self.seek
    }

    /// The whole clip's end, for a clip split at midnight too.
    pub fn end(&self) -> f64 {
        self.full_out.unwrap_or(self.out)
    }

    pub fn is_gap(&self) -> bool {
        self.kind == "gap"
    }

    pub fn is_ad(&self) -> bool {
        self.kind == "ad"
    }

    pub fn to_media(&self) -> Value {
        let mut media = json!({
            "in": r3(self.seek),
            "out": r3(self.out),
            "duration": r3(self.duration),
            "source": self.path,
        });
        if !self.title.is_empty() {
            media["title"] = json!(self.title);
        }
        media
    }
}

pub fn gap(length: f64) -> Slot {
    Slot::new("gap", "", 0.0, r3(length), r3(length))
}

/// The part of a split clip that belongs to the next day.
pub fn continuation(slot: &Slot) -> Option<Slot> {
    let full_out = slot.full_out?;
    if full_out - slot.out < MIN_PART {
        return None;
    }
    Some(Slot { seek: slot.out, out: full_out, full_out: None, part: 1, ..slot.clone() })
}

fn head(item: &Slot, room: f64) -> Slot {
    Slot { out: r3(item.seek + room), full_out: Some(item.out), ..item.clone() }
}

/// Fill today after `prefix`, then all of tomorrow, from `upcoming`.
///
/// `prefix` is today's playlist up to and including the clip on air. It is
/// returned unchanged as the start of today's list. `upcoming` is consumed
/// lazily and only as far as needed to fill tomorrow.
pub fn layout(prefix: Vec<Slot>, upcoming: impl Iterator<Item = Slot>) -> (Vec<Slot>, Vec<Slot>) {
    let mut today = prefix;
    let mut tomorrow = Vec::new();
    let mut pos: f64 = today.iter().map(Slot::length).sum();
    let mut tomorrow_pos = 0.0;

    let tail = today.last().and_then(continuation);
    for mut item in tail.into_iter().chain(upcoming) {
        if item.length() < MIN_PART {
            continue;
        }

        if pos < DAY {
            let room = DAY - pos;
            if item.length() <= room + OVERRUN_TOLERANCE {
                pos += item.length();
                today.push(item);
                continue;
            }

            pos = DAY;
            if room >= MIN_PART {
                let first = head(&item, room);
                let rest = continuation(&first);
                today.push(first);
                match rest {
                    Some(rest) => item = rest,
                    None => continue,
                }
            }
            // Otherwise less than a second is left: ffplayout moves on to the
            // next day by itself, so the clip starts tomorrow in full.
        }

        let room = DAY - tomorrow_pos;
        if item.length() > room + OVERRUN_TOLERANCE {
            // Split tomorrow's last clip the same way, so the playlist doesn't
            // change (and make ffplayout reload) once tomorrow becomes today.
            if room >= MIN_PART {
                tomorrow.push(head(&item, room));
            }
            break;
        }
        tomorrow_pos += item.length();
        tomorrow.push(item);
        if tomorrow_pos >= DAY {
            break;
        }
    }

    (today, tomorrow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn track(length: f64, track_id: i64) -> Slot {
        Slot {
            track_id: Some(track_id),
            ..Slot::new("auto", &format!("/media/{track_id}.mp4"), 0.0, length, length)
        }
    }

    fn total(slots: &[Slot]) -> f64 {
        r3(slots.iter().map(Slot::length).sum())
    }

    #[test]
    fn prefix_is_kept_unchanged() {
        let prefix = vec![gap(1000.0), track(200.0, 7)];
        let (today, _) = layout(prefix.clone(), vec![track(100.0, 8)].into_iter());
        assert_eq!(today[..2], prefix[..]);
        assert_eq!(today[2].track_id, Some(8));
    }

    #[test]
    fn fills_today_exactly_then_tomorrow() {
        let (today, tomorrow) = layout(vec![gap(DAY - 400.0)], (0..2000).map(|i| track(100.0, i)));
        assert_eq!(total(&today), DAY);
        assert_eq!(today.len(), 5);
        assert!(total(&tomorrow) >= DAY);
    }

    #[test]
    fn clip_crossing_midnight_is_split() {
        let (today, tomorrow) =
            layout(vec![gap(DAY - 50.0)], vec![track(100.0, 3), track(100.0, 4)].into_iter());
        let (head, tail) = (today.last().unwrap(), &tomorrow[0]);
        assert_eq!((head.seek, head.out, head.full_out, head.part), (0.0, 50.0, Some(100.0), 0));
        assert_eq!((tail.seek, tail.out, tail.full_out, tail.part), (50.0, 100.0, None, 1));
        assert_eq!(total(&today), DAY);
        assert_eq!(tomorrow[1].track_id, Some(4));
    }

    #[test]
    fn less_than_a_second_before_midnight_starts_clip_tomorrow() {
        let (today, tomorrow) = layout(vec![gap(DAY - 0.6)], vec![track(100.0, 3)].into_iter());
        assert_eq!(today.len(), 1);
        assert_eq!((tomorrow[0].seek, tomorrow[0].out, tomorrow[0].part), (0.0, 100.0, 0));
    }

    #[test]
    fn small_overrun_is_not_split() {
        let (today, tomorrow) =
            layout(vec![gap(DAY - 100.0)], vec![track(100.4, 3), track(50.0, 4)].into_iter());
        assert_eq!(today.last().unwrap().out, 100.4);
        assert_eq!(today.last().unwrap().full_out, None);
        assert_eq!(tomorrow[0].track_id, Some(4));
    }

    #[test]
    fn tiny_tail_is_dropped() {
        let (today, tomorrow) =
            layout(vec![gap(DAY - 100.0)], vec![track(100.8, 3), track(50.0, 4)].into_iter());
        assert_eq!(today.last().unwrap().out, 100.0);
        assert_eq!(tomorrow[0].track_id, Some(4));
    }

    #[test]
    fn split_clip_on_air_continues_tomorrow() {
        let head = Slot {
            track_id: Some(9),
            queue_id: Some(1),
            full_out: Some(100.0),
            ..Slot::new("queue", "/media/x.mp4", 0.0, 50.0, 100.0)
        };
        let (_, tomorrow) = layout(vec![gap(DAY - 50.0), head], vec![track(30.0, 2)].into_iter());
        assert_eq!((tomorrow[0].part, tomorrow[0].seek, tomorrow[0].track_id), (1, 50.0, Some(9)));
        assert_eq!(tomorrow[1].track_id, Some(2));
    }

    #[test]
    fn trimmed_clip_has_no_continuation() {
        let skipped = Slot::new("queue", "/media/x.mp4", 0.0, 12.0, 100.0);
        assert!(continuation(&skipped).is_none());
    }

    #[test]
    fn tomorrow_stops_after_a_full_day() {
        let consumed = Cell::new(0);
        let source = (0..10_000).map(|i| {
            consumed.set(consumed.get() + 1);
            track(600.0, i)
        });
        let (_, tomorrow) = layout(vec![gap(DAY - 10.0)], source);
        assert_eq!(total(&tomorrow), DAY);
        let last = tomorrow.last().unwrap();
        assert_eq!(last.full_out, Some(600.0));
        assert!(last.out < 600.0);
        assert!(consumed.get() < 200);
    }

    #[test]
    fn tomorrow_unchanged_when_it_becomes_today() {
        let tracks: Vec<Slot> = (0..3000).map(|i| track(70.0 + (i % 13) as f64, i)).collect();
        let (_, tomorrow) = layout(vec![gap(DAY - 30.0)], tracks.clone().into_iter());
        // After midnight the tail of track 0 is on air; nothing else changed.
        assert_eq!((tomorrow[0].part, tomorrow[0].track_id), (1, Some(0)));
        let (new_today, _) = layout(tomorrow[..1].to_vec(), tracks[1..].iter().cloned());
        assert_eq!(new_today, tomorrow);
    }

    #[test]
    fn empty_upcoming_leaves_tomorrow_empty() {
        let (today, tomorrow) = layout(vec![gap(100.0), track(30.0, 1)], std::iter::empty());
        assert_eq!(total(&today), 130.0);
        assert!(tomorrow.is_empty());
    }

    #[test]
    fn slots_written_by_the_python_panel_still_load() {
        let old = r#"{"kind": "auto", "path": "/media/a.mp4", "seek": 0.0, "out": 10.5, "duration": 10.5,
            "track_id": 3, "queue_id": null, "auto_id": 7, "title": "A", "full_out": null, "part": 0}"#;
        let slot: Slot = serde_json::from_str(old).unwrap();
        assert_eq!((slot.auto_id, slot.out), (Some(7), 10.5));
    }
}
