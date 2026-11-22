//! Ranks new songs to download, from what you play most.
//!
//! The YouTube side (youtube.rs) collects candidates for the most-played songs
//! in the library: YouTube's related "Mix" for each of them, and other songs by
//! the most-played artists. This module throws out what's already in the
//! library and what isn't the song's music video, and ranks the rest by how
//! strongly your favourites point to it.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use rand::Rng;
use regex::Regex;
use serde_json::{Map, Value};

use crate::util::round_to;

const MIN_SECONDS: f64 = 60.0;
const MAX_SECONDS: f64 = 12.0 * 60.0;
/// Uploads that are not the song's own clip. Remixes are welcome.
static UNWANTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\blyrics?\b|\blyric\s+video\b|\bletra\b|\bтекст\b|\b(official\s+)?audio\b|\bvisuali[sz]er\b|\blive\b|\bconcert\b|\bcover\b|\bkaraoke\b|\binstrumental\b|\bsped\s*up\b|\bslowed\b|\bnightcore\b|\b8d\b|\breaction\b|\btutorial\b|\b\d+\s*hours?\b|\bloop\b|\bfull\s+album\b|\bplaylist\b|\bcompilation\b|\bbest\s+of\b|\bmix\s+20\d\d\b",
    )
    .unwrap()
});
static FEAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s*[(\[]?\b(feat|ft|featuring|with)\b\.?.*$").unwrap());
static NON_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\W_]+").unwrap());

/// The same song under slightly different names maps to one key.
pub fn song_key(artist: &str, title: &str) -> String {
    fn norm(text: &str) -> String {
        let text = FEAT.replace(&text.to_lowercase(), "").into_owned();
        NON_WORD.replace_all(&text, " ").trim().to_string()
    }
    format!("{}|{}", norm(artist), norm(title))
}

/// Candidates found from one favourite song (kind "mix") or artist ("artist").
pub struct Source {
    pub kind: &'static str,
    /// The song's title, or the artist.
    pub name: String,
    pub plays: i64,
    pub entries: Vec<Value>,
}

struct Found {
    entry: Map<String, Value>,
    artist: String,
    score: f64,
    related: Vec<(String, i64)>,
    by: Option<(String, i64)>,
}

/// New songs best first, each with the reasons it was picked.
///
/// entries are YouTube results ({"id", "title", "channel", "duration", ...});
/// split(title, channel) gives (artist, song title); artist_songs counts
/// library songs per lowercased artist.
pub fn rank(
    sources: &[Source],
    have_ids: &HashSet<String>,
    have_keys: &HashSet<String>,
    artist_songs: &HashMap<String, i64>,
    split: impl Fn(&str, &str) -> (String, String),
    limit: usize,
) -> Vec<Map<String, Value>> {
    let mut order: Vec<String> = Vec::new();
    let mut found: HashMap<String, Found> = HashMap::new();
    for source in sources {
        let mut weight = 1.0 + (source.plays as f64).ln_1p();
        if source.kind == "artist" {
            weight *= 0.7;
        }
        let count = source.entries.len();
        for (position, entry) in source.entries.iter().enumerate() {
            let Some(obj) = entry.as_object() else { continue };
            let vid = obj.get("id").and_then(Value::as_str).unwrap_or("");
            let title = obj.get("title").and_then(Value::as_str).unwrap_or("");
            if vid.is_empty() || have_ids.contains(vid) || UNWANTED.is_match(title) {
                continue;
            }
            let live = obj.get("live_status").and_then(Value::as_str).unwrap_or("");
            if matches!(live, "is_live" | "is_upcoming" | "post_live") {
                continue;
            }
            if let Some(length) = obj.get("duration").and_then(Value::as_f64) {
                if !(MIN_SECONDS..=MAX_SECONDS).contains(&length) {
                    continue;
                }
            }
            let channel = obj.get("channel").and_then(Value::as_str).unwrap_or("");
            let (artist, song) = split(title, channel);
            let key = song_key(&artist, &song);
            if have_keys.contains(&key) {
                continue;
            }
            let item = found.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                let mut entry = obj.clone();
                entry.insert("artist".into(), Value::from(artist.clone()));
                entry.insert("song".into(), Value::from(song.clone()));
                Found { entry, artist: artist.clone(), score: 0.0, related: Vec::new(), by: None }
            });
            // Another upload of the same song still counts for it; the first one found is kept.
            // Earlier in a Mix means more closely related.
            item.score += weight * (1.0 - position as f64 / (count + 1) as f64);
            if source.kind == "mix" {
                item.related.push((source.name.clone(), source.plays));
            } else {
                item.by = Some((source.name.clone(), source.plays));
            }
        }
    }

    let mut items: Vec<Found> = order.into_iter().filter_map(|k| found.remove(&k)).collect();
    for item in &mut items {
        let songs = artist_songs.get(&item.artist.to_lowercase()).copied().unwrap_or(0);
        if songs > 0 {
            item.score += 0.4 * (songs as f64).ln_1p();
        }
        if item.related.len() > 1 {
            item.score *= 1.0 + 0.15 * (item.related.len() - 1) as f64;
        }
        let reasons = reasons(item, songs);
        item.entry.insert("reasons".into(), Value::from(reasons));
    }
    // Stable, like Python's sorted(): equal scores keep the order found.
    items.sort_by(|a, b| b.score.total_cmp(&a.score));
    items.truncate(limit);
    items
        .into_iter()
        .map(|mut item| {
            item.entry.insert("score".into(), Value::from(round_to(item.score, 3)));
            item.entry
        })
        .collect()
}

/// Short reasons, e.g. ["Like Cheri Cheri Lady +2", "More Bad Boys Blue"].
fn reasons(item: &Found, songs: i64) -> Vec<String> {
    let mut out = Vec::new();
    let mut related = item.related.clone();
    related.sort_by(|a, b| b.1.cmp(&a.1));
    if let Some(first) = related.first() {
        let others = related.iter().map(|(n, _)| n).collect::<HashSet<_>>().len() - 1;
        out.push(if others > 0 { format!("Like {} +{others}", first.0) } else { format!("Like {}", first.0) });
    }
    if item.by.is_some() || songs > 0 {
        let name = item.by.as_ref().map(|b| b.0.as_str()).unwrap_or(&item.artist);
        out.push(format!("More {name}"));
    }
    out
}

/// k different items, likelier the heavier they are.
pub fn weighted_sample<T>(items: Vec<T>, weights: &[f64], k: usize, rng: &mut impl Rng) -> Vec<T> {
    let mut pool: Vec<(T, f64)> = items.into_iter().zip(weights.iter().map(|w| w.max(0.01))).collect();
    let mut picked = Vec::new();
    while !pool.is_empty() && picked.len() < k {
        let total: f64 = pool.iter().map(|(_, w)| w).sum();
        let mut x = rng.random_range(0.0..total);
        let mut index = pool.len() - 1;
        for (i, (_, w)) in pool.iter().enumerate() {
            if x < *w {
                index = i;
                break;
            }
            x -= w;
        }
        picked.push(pool.remove(index).0);
    }
    picked
}

fn score(item: &Map<String, Value>) -> f64 {
    item.get("score").and_then(Value::as_f64).unwrap_or(0.0)
}

fn id(item: &Map<String, Value>) -> &str {
    item.get("id").and_then(Value::as_str).unwrap_or("")
}

/// Up to `limit` suggestions, leaving out the ones shown in recent lists.
///
/// Close scores are shuffled a little so the same list doesn't come back in
/// the same order; recently shown songs only fill up a short list.
pub fn fresh_picks(
    ranked: Vec<Map<String, Value>>,
    recently_shown: &HashSet<String>,
    limit: usize,
    rng: &mut impl Rng,
) -> Vec<Map<String, Value>> {
    let mut jittered: Vec<(f64, Map<String, Value>)> =
        ranked.into_iter().map(|item| (-score(&item) * rng.random_range(0.75..1.25), item)).collect();
    jittered.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (seen, new): (Vec<_>, Vec<_>) =
        jittered.into_iter().map(|(_, item)| item).partition(|item| recently_shown.contains(id(item)));
    new.into_iter().chain(seen).take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::youtube::split_title;
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use serde_json::json;

    fn e(vid: &str, title: &str) -> Value {
        e_len(vid, title, 220)
    }

    fn e_len(vid: &str, title: &str, duration: i64) -> Value {
        json!({"id": format!("{vid:x<11}"), "title": title, "channel": "Someone", "duration": duration})
    }

    fn split(title: &str, channel: &str) -> (String, String) {
        split_title(title, channel, None, None)
    }

    fn titles(ranked: &[Map<String, Value>]) -> Vec<&str> {
        ranked.iter().map(|r| r["title"].as_str().unwrap()).collect()
    }

    fn reasons(item: &Map<String, Value>) -> Vec<&str> {
        item["reasons"].as_array().unwrap().iter().map(|r| r.as_str().unwrap()).collect()
    }

    #[test]
    fn song_key_ignores_case_punctuation_and_features() {
        assert_eq!(song_key("Daft Punk", "Get Lucky (feat. Pharrell)"), song_key("daft punk", "Get Lucky!"));
    }

    #[test]
    fn ranks_new_songs_by_how_strongly_favourites_point_to_them() {
        let sources = [
            Source {
                kind: "mix",
                name: "Bohemian Rhapsody".into(),
                plays: 12,
                entries: vec![
                    e("seed1", "Queen - Bohemian Rhapsody (Official Video)"),
                    e("a", "Queen - Don't Stop Me Now (Official Video)"),
                    e("b", "Muse - Uprising"),
                    e("c", "Queen - Somebody To Love (Lyrics)"),
                    e("d", "Queen - Live Aid 1985 (Live)"),
                    e_len("e", "Queen - Greatest Hits", 3600),
                    e("f", "David Bowie - Starman"),
                ],
            },
            Source {
                kind: "mix",
                name: "Levitating".into(),
                plays: 2,
                entries: vec![e("g", "Dua Lipa - Physical"), e("f2", "David Bowie - Starman (2012 Remaster)")],
            },
        ];
        let ranked = rank(
            &sources,
            &HashSet::from([format!("{:x<11}", "seed1")]),
            &HashSet::from([song_key("Muse", "Uprising")]),
            &HashMap::from([("queen".to_string(), 3)]),
            split,
            20,
        );
        // Owned, lyric, live and too-long uploads are left out.
        assert_eq!(
            titles(&ranked),
            ["Queen - Don't Stop Me Now (Official Video)", "David Bowie - Starman", "Dua Lipa - Physical"]
        );
        assert_eq!(reasons(&ranked[0]), ["Like Bohemian Rhapsody", "More Queen"]);
        assert_eq!(reasons(&ranked[1]), ["Like Bohemian Rhapsody +1"]);
    }

    #[test]
    fn remixes_are_suggested_next_to_the_original() {
        let ranked = rank(
            &[Source {
                kind: "mix",
                name: "Cheri Cheri Lady".into(),
                plays: 6,
                entries: vec![e("a", "Modern Talking - Cheri Cheri Lady (Remix 2024)")],
            }],
            &HashSet::new(),
            &HashSet::from([song_key("Modern Talking", "Cheri Cheri Lady")]),
            &HashMap::new(),
            split,
            20,
        );
        assert_eq!(titles(&ranked), ["Modern Talking - Cheri Cheri Lady (Remix 2024)"]);
    }

    #[test]
    fn artist_source_reason() {
        let ranked = rank(
            &[Source { kind: "artist", name: "Queen".into(), plays: 30, entries: vec![e("a", "Queen - Radio Ga Ga")] }],
            &HashSet::new(),
            &HashSet::new(),
            &HashMap::from([("queen".to_string(), 3)]),
            split,
            20,
        );
        assert_eq!(reasons(&ranked[0]), ["More Queen"]);
    }

    #[test]
    fn fresh_picks_skip_recently_shown_and_fill_up_with_them() {
        let ranked: Vec<Map<String, Value>> = (0..10)
            .map(|i| json!({"id": i.to_string(), "score": 10.0 - i as f64}).as_object().unwrap().clone())
            .collect();
        let shown = HashSet::from(["0".to_string(), "1".into(), "2".into()]);
        let picks = fresh_picks(ranked.clone(), &shown, 5, &mut StdRng::seed_from_u64(1));
        assert_eq!(picks.len(), 5);
        assert!(picks.iter().all(|p| !shown.contains(id(p))));
        let short = fresh_picks(ranked[..4].to_vec(), &shown, 4, &mut StdRng::seed_from_u64(1));
        assert_eq!(id(&short[0]), "3");
        let ids: HashSet<&str> = short.iter().map(id).collect();
        assert_eq!(ids, HashSet::from(["0", "1", "2", "3"]));
    }

    #[test]
    fn weighted_sample_picks_distinct_items() {
        for seed in 0..20 {
            let picked = weighted_sample(
                vec!['a', 'b', 'c', 'd', 'e', 'f'],
                &[50.0, 1.0, 1.0, 1.0, 1.0, 1.0],
                3,
                &mut StdRng::seed_from_u64(seed),
            );
            assert_eq!(picked.iter().collect::<HashSet<_>>().len(), 3);
        }
        assert_eq!(weighted_sample(vec!['a'], &[0.0], 3, &mut rand::rng()), ['a']);
    }
}
