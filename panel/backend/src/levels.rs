//! Song levels: Отлично, Хорошо (every new song) or Плохо.
//!
//! The level steers autoplay (how often a song comes up), new-song
//! suggestions (which favourites they start from) and library sorting.

pub const GREAT: i64 = 2;
pub const GOOD: i64 = 1;
pub const BAD: i64 = 0;
pub const ALL: [i64; 3] = [GREAT, GOOD, BAD];

pub fn valid(level: i64) -> bool {
    ALL.contains(&level)
}

/// How often autoplay picks a song, relative to a Хорошо one.
pub fn autoplay_weight(level: i64) -> f64 {
    match level {
        GREAT => 3.0,
        GOOD => 1.0,
        _ => 0.35,
    }
}

/// How much a song's plays count towards suggestions; Плохо songs don't.
pub fn favourite_factor(level: i64) -> i64 {
    match level {
        GREAT => 3,
        GOOD => 1,
        _ => 0,
    }
}

/// `favourite_factor` in SQL.
pub const FAVOURITE_SQL: &str = "(CASE level WHEN 2 THEN 3 WHEN 1 THEN 1 ELSE 0 END)";
