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

/// A parsed `frid://` share link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FridLink {
    /// Canonical content id (`b3:<64 hex>`).
    pub content_id: String,
    /// Human-readable "artists-title" label from the `t` query parameter.
    pub label: Option<String>,
}

pub fn parse_frid_link(value: &str) -> Option<FridLink> {
    let value = value.trim();
    let rest = value.strip_prefix("frid://")?;
    let mut parts = rest.splitn(2, ['?', '#']);
    let content_id =
        music_dht::normalize_content_id(parts.next().unwrap_or_default().trim_end_matches('/'))?;
    let label = parts.next().and_then(|query| {
        query.split('&').find_map(|pair| {
            let value = pair.strip_prefix("t=")?;
            let decoded = percent_decode(value);
            let decoded = decoded.trim();
            (!decoded.is_empty()).then(|| decoded.to_string())
        })
    });
    Some(FridLink { content_id, label })
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

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 3 <= bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                match hex {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            // Liberal form-encoding acceptance; our encoder never emits '+'.
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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
        let link = parse_frid_link(
            "frid://b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef?t=Artist-Track"
        )
        .expect("valid link");
        assert_eq!(
            link.content_id,
            "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert_eq!(link.label.as_deref(), Some("Artist-Track"));
        assert!(parse_frid_link("https://example.com").is_none());
    }

    #[test]
    fn parses_percent_encoded_label() {
        let link = parse_frid_link(
            "frid://B3:0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef?t=Rammstein-Du%20Riechst%20So%20Gut"
        )
        .expect("valid link");
        assert_eq!(
            link.content_id,
            "b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert_eq!(link.label.as_deref(), Some("Rammstein-Du Riechst So Gut"));

        // Cyrillic labels and a missing label both survive.
        let link = parse_frid_link(
            "frid://b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef?t=%D0%90%D1%80%D1%82%D0%B8%D1%81%D1%82"
        )
        .expect("valid link");
        assert_eq!(link.label.as_deref(), Some("Артист"));
        let link = parse_frid_link(
            "frid://b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .expect("valid link");
        assert_eq!(link.label, None);
    }

    #[test]
    fn share_link_round_trips_through_parse() {
        let track = TrackItem {
            id: 1,
            title: "Du Riechst So Gut".into(),
            track_number: None,
            disc_number: None,
            duration_seconds: 1.0,
            artists: vec![ArtistRef {
                id: 1,
                name: "Rammstein".into(),
            }],
            featured_artists: Vec::new(),
            release_id: 1,
            release_title: "Herzeleid".into(),
            release_year: None,
            file_path: String::new(),
            content_id: Some(
                "b3:5ebd9e61e1154a64470e6ee4dd1225550f380dd575d01fd4ffba6a5bd1b34104".into(),
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
        let link = track_share_link(&track).expect("link");
        let parsed = parse_frid_link(&link).expect("parses back");
        assert_eq!(
            parsed.content_id,
            "b3:5ebd9e61e1154a64470e6ee4dd1225550f380dd575d01fd4ffba6a5bd1b34104"
        );
        assert_eq!(parsed.label.as_deref(), Some("Rammstein-Du Riechst So Gut"));
    }
}
