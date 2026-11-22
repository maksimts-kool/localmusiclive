//! Song categories: free-form names stored per track as a JSON list.

use std::collections::{HashMap, HashSet};

const MAX_LENGTH: usize = 32;
const MAX_PER_TRACK: usize = 8;

pub fn fold(text: &str) -> String {
    text.to_lowercase()
}

/// Trimmed, de-duplicated names, reusing the casing of existing categories.
pub fn clean_categories<'a>(
    values: impl IntoIterator<Item = &'a str>,
    existing: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let known: HashMap<String, &str> = existing.into_iter().map(|n| (fold(n), n)).collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for value in values {
        let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
        let name: String = collapsed.chars().take(MAX_LENGTH).collect::<String>().trim().to_string();
        let key = fold(&name);
        if name.is_empty() || seen.contains(&key) {
            continue;
        }
        let name = known.get(&key).map(|n| n.to_string()).unwrap_or(name);
        seen.insert(key);
        out.push(name);
    }
    out.truncate(MAX_PER_TRACK);
    out
}

/// The names in a track's `categories` column.
pub fn categories_of(json: &str) -> Vec<String> {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Array(items)) => {
            items.into_iter().filter_map(|v| v.as_str().map(str::to_string)).collect()
        }
        _ => Vec::new(),
    }
}

pub fn to_json(names: &[String]) -> String {
    serde_json::to_string(names).unwrap_or_else(|_| "[]".into())
}

/// A genre tag like "Pop; Dance/Electronic" as starting categories.
pub fn split_genres(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut names = clean_categories(text.split([';', ',', '/', '|']), std::iter::empty());
    names.truncate(3);
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_and_reuses_existing_casing() {
        let names = clean_categories(["  rock  ", "Rock", "new   wave", ""], ["ROCK"]);
        assert_eq!(names, ["ROCK", "new wave"]);
    }

    #[test]
    fn genres() {
        assert_eq!(split_genres("Pop; Dance/Electronic"), ["Pop", "Dance", "Electronic"]);
        assert!(split_genres("").is_empty());
    }
}
