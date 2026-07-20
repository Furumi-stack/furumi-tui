use crate::library::models::TrackItem;

pub fn track_content_id(track: &TrackItem) -> Option<String> {
    track
        .fed
        .as_ref()
        .and_then(|fed| fed.content_id.as_deref())
        .or(track.content_id.as_deref())
        .and_then(music_dht::normalize_content_id)
}

pub fn track_can_share(track: &TrackItem) -> bool {
    track_content_id(track).is_some() || !track.file_path.trim().is_empty()
}

pub fn track_share_link(track: &TrackItem) -> Option<String> {
    let content_id = track_content_id(track).or_else(|| {
        (!track.file_path.trim().is_empty())
            .then(|| crate::library::audio_content_id(&track.file_path))
            .flatten()
    })?;
    Some(track_share_link_for_content_id(track, &content_id))
}

pub fn parse_frid_content_id(value: &str) -> Option<String> {
    let value = value.trim();
    let rest = value.strip_prefix("frid://")?;
    let content_id = rest
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    music_dht::normalize_content_id(content_id)
}

pub fn cached_track_share_link(track: &TrackItem) -> Option<String> {
    let content_id = track_content_id(track)?;
    Some(track_share_link_for_content_id(track, &content_id))
}

fn track_share_link_for_content_id(track: &TrackItem, content_id: &str) -> String {
    let label = track_share_label(track);
    if label.is_empty() {
        format!("frid://{content_id}")
    } else {
        format!("frid://{content_id}?t={}", percent_encode(&label))
    }
}

fn track_share_label(track: &TrackItem) -> String {
    let artists = track.artist_line();
    let title = track.title.trim();
    match (artists.trim().is_empty(), title.is_empty()) {
        (false, false) => format!("{}-{title}", artists.trim()),
        (true, false) => title.to_string(),
        (false, true) => artists.trim().to_string(),
        (true, true) => String::new(),
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(*byte))
            }
            byte => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::models::ArtistRef;

    #[test]
    fn share_link_uses_content_id_and_readable_label() {
        let track = TrackItem {
            id: 1,
            title: "Трек".into(),
            track_number: None,
            disc_number: None,
            duration_seconds: 1.0,
            artists: vec![ArtistRef {
                id: 1,
                name: "Артист".into(),
            }],
            featured_artists: Vec::new(),
            release_id: 1,
            release_title: "Release".into(),
            release_year: None,
            file_path: String::new(),
            content_id: Some(
                "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            ),
            cover_path: None,
            audio_format: None,
            audio_bitrate: None,
            audio_sample_rate: None,
            audio_bit_depth: None,
            file_size_bytes: None,
            play_count: 0,
            fed: None,
        };

        assert_eq!(
            track_share_link(&track).as_deref(),
            Some(
                "frid://b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef?t=%D0%90%D1%80%D1%82%D0%B8%D1%81%D1%82-%D0%A2%D1%80%D0%B5%D0%BA"
            )
        );
    }

    #[test]
    fn parses_frid_content_link() {
        assert_eq!(
            parse_frid_content_id(
                "frid://b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef?t=Artist-Track"
            )
            .as_deref(),
            Some("b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
        assert!(parse_frid_content_id("https://example.com").is_none());
    }
}
