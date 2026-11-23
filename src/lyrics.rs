//! Content-addressed lyrics and the playback timeline shared by the UI and scripts.
pub(crate) use crate::library::lyrics::{SavedLyrics, identity, merge};
use music_dht::lyrics::LyricsVersion;

pub(crate) fn clean_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter_map(|c| match c {
            '\t' => Some(' '),
            '\n' => Some('\n'),
            c if !c.is_control() => Some(c),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Default)]
pub(crate) struct Document {
    pub lines: Vec<String>,
    times: Vec<u64>,
    pub instrumental: bool,
}

impl Document {
    pub fn new(version: &LyricsVersion) -> Self {
        let timed = version
            .synced_lrc
            .as_deref()
            .map(parse_lrc)
            .unwrap_or_default();
        let (mut times, lines): (Vec<_>, Vec<_>) = timed.into_iter().unzip();
        let lines = if times.is_empty() {
            let text = version
                .plain_text
                .as_deref()
                .or(version.synced_lrc.as_deref());
            text.map(clean_text)
                .map(|text| text.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        } else {
            // Some LRC releases split a duet over two lines with the same timestamp.
            let mut combined: Vec<String> = Vec::new();
            let mut previous = None;
            let mut keep = Vec::new();
            for (time, line) in times.into_iter().zip(lines) {
                if previous == Some(time) {
                    if let Some(last) = combined.last_mut() {
                        last.push_str(" / ");
                        last.push_str(&line);
                    }
                } else {
                    keep.push(time);
                    combined.push(line);
                    previous = Some(time);
                }
            }
            times = keep;
            combined
        };
        Self {
            lines,
            times,
            instrumental: version.instrumental,
        }
    }

    pub fn timed(&self) -> bool {
        !self.times.is_empty()
    }

    pub fn line_at(&self, position: f64) -> Option<usize> {
        let millis = (position * 1000.0) as u64;
        self.times
            .partition_point(|&time| time <= millis)
            .checked_sub(1)
    }

    pub fn time(&self, index: usize) -> Option<f64> {
        self.times.get(index).map(|&time| time as f64 / 1000.0)
    }
}

fn parse_lrc(value: &str) -> Vec<(u64, String)> {
    let offset = value
        .lines()
        .find_map(|line| {
            line.trim()
                .trim_start_matches('\u{feff}')
                .strip_prefix("[offset:")?
                .strip_suffix(']')?
                .trim()
                .parse::<i64>()
                .ok()
        })
        .unwrap_or_default()
        .clamp(-3_600_000, 3_600_000);
    let mut lines = Vec::new();
    for line in value.lines() {
        let mut rest = line.trim().trim_start_matches('\u{feff}');
        let mut times = Vec::new();
        while let Some((tag, tail)) = rest.strip_prefix('[').and_then(|s| s.split_once(']')) {
            let Some((minutes, seconds)) = tag.split_once(':') else {
                break;
            };
            let (Ok(minutes), Ok(seconds)) = (minutes.parse::<u32>(), seconds.parse::<f64>())
            else {
                break;
            };
            if minutes > 1440 || !(0.0..60.0).contains(&seconds) {
                break;
            }
            let ms = ((f64::from(minutes) * 60.0 + seconds) * 1000.0).round() as i64;
            times.push((ms + offset).max(0) as u64);
            rest = tail;
        }
        if !times.is_empty() {
            let line = clean_text(rest.trim());
            lines.extend(times.into_iter().map(|time| (time, line.clone())));
        }
    }
    lines.sort_by_key(|(time, _)| *time);
    lines
}

pub(crate) fn manual_version(label: &str, text: &str) -> anyhow::Result<LyricsVersion> {
    let label = clean_text(label);
    let text = clean_text(text);
    anyhow::ensure!(
        !text.trim().is_empty() && text.len() <= music_dht::lyrics::MAX_LYRICS_TEXT_BYTES,
        "Enter lyrics up to 256 KiB"
    );
    let timed = !parse_lrc(&text).is_empty();
    let mut version = LyricsVersion {
        id: "manual".into(),
        label: if label.trim().is_empty() {
            "Manual".into()
        } else {
            label.trim().into()
        },
        source: "manual".into(),
        source_id: None,
        language: None,
        plain_text: (!timed).then(|| text.clone()),
        synced_lrc: timed.then_some(text),
        instrumental: false,
    };
    version.id = identity(&version);
    version.validate()?;
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn lrc_handles_offsets_repeated_timestamps_and_seeks() {
        let version = manual_version(
            "Duet",
            "[offset:-500]\n[00:02][00:08]Hello\n[00:02]Hi\n[00:04.25]\n[00:05]Bye",
        )
        .unwrap();
        let doc = Document::new(&version);
        assert_eq!(doc.lines, ["Hello / Hi", "", "Bye", "Hello"]);
        assert_eq!(doc.line_at(1.49), None);
        assert_eq!(doc.line_at(1.5), Some(0));
        assert_eq!(doc.line_at(8.0), Some(3));
        assert_eq!(doc.line_at(3.8), Some(1));
    }

    #[test]
    fn versions_are_deduplicated_by_representations() {
        let first = manual_version("One", "Words").unwrap();
        let second = manual_version("Another label", "Words").unwrap();
        let timed = manual_version("Timed", "[00:01]Words").unwrap();
        let mut versions = vec![first.clone()];
        merge(&mut versions, [second, timed], &HashSet::new());
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0], first);
    }
}
