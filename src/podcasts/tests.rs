use super::*;

const ITUNES_NS: &str = "http://www.itunes.com/dtds/podcast-1.0.dtd";

fn rss(channel_extra: &str, items: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:itunes="{ITUNES_NS}" xmlns:content="http://purl.org/rss/1.0/modules/content/">
<channel>
  <title>Test Cast</title>
  <link>https://example.test/cast</link>
  {channel_extra}
  {items}
</channel>
</rss>"#
    )
}

fn item(guid: &str, title: &str, date: &str) -> String {
    format!(
        r#"<item>
  <title>{title}</title>
  <guid isPermaLink="false">{guid}</guid>
  <pubDate>{date}</pubDate>
  <enclosure url="https://cdn.example.test/{guid}.mp3" length="1234" type="audio/mpeg"/>
</item>"#
    )
}

mod feed_parsing {
    use super::*;
    use crate::podcasts::feed::{html_to_text, parse_date, parse_duration};

    #[test]
    fn rss_with_itunes_tags_cdata_and_fallbacks() {
        let xml = rss(
            r#"<description><![CDATA[<p>Weekly <b>talk</b> show.</p><script>evil()</script>]]></description>
  <itunes:author>Jane Host</itunes:author>
  <itunes:image href="https://example.test/cover.jpg"/>
  <image><url>https://example.test/small.png</url><title>ignored</title></image>"#,
            r#"<item>
  <title>  Episode   one </title>
  <itunes:title>One</itunes:title>
  <description>&lt;p&gt;First &amp;amp; best&lt;/p&gt;</description>
  <guid>ep-1</guid>
  <pubDate>Tue, 02 Jan 2024 10:00:00 GMT</pubDate>
  <itunes:duration>1:02:03</itunes:duration>
  <itunes:image href="https://example.test/ep1.jpg"/>
  <link>https://example.test/ep1</link>
  <enclosure url="https://cdn.example.test/1.mp3" length="5000" type="audio/mpeg"/>
</item>
<item>
  <title></title>
  <itunes:title>Only itunes title</itunes:title>
  <content:encoded><![CDATA[<div>Long <i>show</i> notes</div>]]></content:encoded>
  <guid>ep-2</guid>
  <itunes:duration>754</itunes:duration>
  <enclosure url="https://cdn.example.test/2.m4a" type="audio/x-m4a"/>
</item>
<item>
  <itunes:summary>Summary only</itunes:summary>
  <guid>ep-3</guid>
  <enclosure url="https://cdn.example.test/3.mp3"/>
</item>"#,
        );
        let feed = parse_feed(xml.as_bytes()).unwrap();
        assert_eq!(feed.title, "Test Cast");
        assert_eq!(feed.author.as_deref(), Some("Jane Host"));
        assert_eq!(feed.description.as_deref(), Some("Weekly talk show."));
        assert_eq!(feed.link.as_deref(), Some("https://example.test/cast"));
        assert_eq!(
            feed.image_url.as_deref(),
            Some("https://example.test/cover.jpg")
        );
        assert_eq!(feed.episodes.len(), 3);

        let first = &feed.episodes[0];
        assert_eq!(first.guid, "ep-1");
        assert_eq!(first.title, "Episode one");
        assert_eq!(first.description.as_deref(), Some("First & best"));
        assert_eq!(first.published_at, Some(1_704_189_600));
        assert_eq!(first.duration_seconds, Some(3723.0));
        assert_eq!(first.audio_url, "https://cdn.example.test/1.mp3");
        assert_eq!(first.mime_type.as_deref(), Some("audio/mpeg"));
        assert_eq!(first.byte_len, Some(5000));
        assert_eq!(first.link.as_deref(), Some("https://example.test/ep1"));
        assert_eq!(
            first.image_url.as_deref(),
            Some("https://example.test/ep1.jpg")
        );

        let second = &feed.episodes[1];
        assert_eq!(second.title, "Only itunes title");
        assert_eq!(second.description.as_deref(), Some("Long show notes"));
        assert_eq!(second.duration_seconds, Some(754.0));
        assert_eq!(second.byte_len, None);
        assert_eq!(second.published_at, None);

        let third = &feed.episodes[2];
        assert_eq!(third.title, "Untitled episode");
        assert_eq!(third.description.as_deref(), Some("Summary only"));
        assert_eq!(third.mime_type, None);
    }

    #[test]
    fn entities_numeric_refs_and_unknown_html_entities() {
        let xml = rss(
            "",
            r#"<item>
  <title>A&nbsp;&amp;&nbsp;B &#8212; &#x41;&#1055; &mdash; &bogus; R&amp;D &copy;</title>
  <description>x &lt; y &amp;&amp; z</description>
  <guid>e</guid>
  <enclosure url="https://cdn.example.test/a.mp3?x=1&amp;y=2" type="audio/mpeg"/>
</item>"#,
        );
        let feed = parse_feed(xml.as_bytes()).unwrap();
        let episode = &feed.episodes[0];
        assert_eq!(episode.title, "A & B — AП — &bogus; R&D ©");
        assert_eq!(episode.description.as_deref(), Some("x < y && z"));
        assert_eq!(episode.audio_url, "https://cdn.example.test/a.mp3?x=1&y=2");
    }

    #[test]
    fn atom_feed() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title type="text">Atom Cast</title>
  <subtitle>About &lt;b&gt;things&lt;/b&gt;</subtitle>
  <link rel="self" href="https://example.test/atom.xml"/>
  <link rel="alternate" href="https://example.test/"/>
  <logo>https://example.test/logo.png</logo>
  <author><name>Atom Author</name></author>
  <entry>
    <title>Entry one</title>
    <id>urn:uuid:1</id>
    <published>2024-03-01T12:30:00+02:00</published>
    <updated>2024-03-05T00:00:00Z</updated>
    <summary type="html">&lt;p&gt;Entry summary&lt;/p&gt;</summary>
    <link rel="alternate" href="https://example.test/e1"/>
    <link rel="enclosure" href="https://cdn.example.test/e1.ogg" type="audio/ogg" length="42"/>
  </entry>
  <entry>
    <title>Entry two</title>
    <id>urn:uuid:2</id>
    <updated>2024-03-06T00:00:00Z</updated>
    <content type="xhtml"><div xmlns="http://www.w3.org/1999/xhtml"><p>Inline</p><p>xhtml</p></div></content>
    <link rel="enclosure" href="https://cdn.example.test/e2.mp3"/>
  </entry>
  <entry>
    <title>No audio</title>
    <id>urn:uuid:3</id>
    <link rel="alternate" href="https://example.test/e3"/>
  </entry>
</feed>"#;
        let feed = parse_feed(xml.as_bytes()).unwrap();
        assert_eq!(feed.title, "Atom Cast");
        assert_eq!(feed.description.as_deref(), Some("About things"));
        assert_eq!(feed.link.as_deref(), Some("https://example.test/"));
        assert_eq!(
            feed.image_url.as_deref(),
            Some("https://example.test/logo.png")
        );
        assert_eq!(feed.author.as_deref(), Some("Atom Author"));
        assert_eq!(feed.episodes.len(), 2);
        let first = &feed.episodes[0];
        assert_eq!(first.guid, "urn:uuid:1");
        assert_eq!(first.published_at, Some(1_709_289_000));
        assert_eq!(first.description.as_deref(), Some("Entry summary"));
        assert_eq!(first.link.as_deref(), Some("https://example.test/e1"));
        assert_eq!(first.audio_url, "https://cdn.example.test/e1.ogg");
        assert_eq!(first.byte_len, Some(42));
        let second = &feed.episodes[1];
        assert_eq!(second.published_at, Some(1_709_683_200));
        assert_eq!(second.description.as_deref(), Some("Inline xhtml"));
    }

    #[test]
    fn items_without_audio_are_skipped_and_media_content_is_a_fallback() {
        let xml = r#"<rss xmlns:media="http://search.yahoo.com/mrss/"><channel><title>T</title>
<item><title>No enclosure</title><guid>a</guid></item>
<item><title>Image enclosure</title><guid>b</guid><enclosure url="https://x.test/b.jpg" type="image/jpeg"/></item>
<item><title>Empty url</title><guid>c</guid><enclosure url="" type="audio/mpeg"/></item>
<item><title>Media</title><guid>d</guid>
  <media:content url="https://x.test/d.png" medium="image"/>
  <media:group><media:content url="https://x.test/d.mp3" type="audio/mpeg" fileSize="77"/></media:group>
  <media:thumbnail url="https://x.test/d-thumb.jpg"/>
</item>
<item><title>Video</title><guid>e</guid><enclosure url="https://x.test/e.mp4" type="video/mp4"/></item>
</channel></rss>"#;
        let feed = parse_feed(xml.as_bytes()).unwrap();
        let titles: Vec<_> = feed.episodes.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["Media", "Video"]);
        assert_eq!(feed.episodes[0].audio_url, "https://x.test/d.mp3");
        assert_eq!(feed.episodes[0].byte_len, Some(77));
        assert_eq!(
            feed.episodes[0].image_url.as_deref(),
            Some("https://x.test/d-thumb.jpg")
        );
    }

    #[test]
    fn duplicate_guids_keep_the_first_and_guid_falls_back_to_the_url() {
        let items = [
            item("dup", "First", "Mon, 01 Jan 2024 00:00:00 +0000"),
            item("dup", "Second", "Tue, 02 Jan 2024 00:00:00 +0000"),
            r#"<item><title>No guid</title><enclosure url="https://cdn.example.test/noguid.mp3" type="audio/mpeg"/></item>"#.to_string(),
            r#"<item><title>Blank guid</title><guid>  </guid><enclosure url="https://cdn.example.test/blank.mp3" type="audio/mpeg"/></item>"#.to_string(),
        ]
        .concat();
        let feed = parse_feed(rss("", &items).as_bytes()).unwrap();
        let pairs: Vec<_> = feed
            .episodes
            .iter()
            .map(|e| (e.guid.as_str(), e.title.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("dup", "First"),
                ("https://cdn.example.test/noguid.mp3", "No guid"),
                ("https://cdn.example.test/blank.mp3", "Blank guid"),
            ]
        );
    }

    #[test]
    fn durations() {
        let cases = [
            ("01:02:03", Some(3723.0)),
            ("1:02:03", Some(3723.0)),
            ("62:03", Some(3723.0)),
            ("3723", Some(3723.0)),
            ("3723.5", Some(3723.5)),
            ("00:00:01.25", Some(1.25)),
            (" 45:00 ", Some(2700.0)),
            ("0", None),
            ("00:00:00", None),
            ("", None),
            ("abc", None),
            ("1:2:3:4", None),
            ("-5", None),
            ("1e3", None),
            ("NaN", None),
            ("12:", None),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_duration(input), expected, "{input:?}");
        }
    }

    #[test]
    fn dates() {
        let reference = Some(1_704_189_600); // 2024-01-02 10:00:00 UTC
        let cases = [
            ("Tue, 02 Jan 2024 10:00:00 GMT", reference),
            ("Tue, 02 Jan 2024 10:00:00 +0000", reference),
            ("02 Jan 2024 10:00:00 UT", reference),
            ("Tue, 2 Jan 2024 13:00:00 +0300", reference),
            ("Tue,  02  Jan  2024   05:00:00   EST", reference),
            ("Tue, 02 Jan 2024 02:00:00 PST", reference),
            ("Tuesday, 02 January 2024 10:00 GMT", reference),
            ("Tue, 02 Jan 24 10:00:00 GMT", reference),
            ("Tue, 02 Jan 2024 13:00:00 +03:00", reference),
            ("Tue, 02 Jan 2024 10:00:00", reference),
            ("Вт, 02 янв 2024 13:00:00 +0300", reference),
            ("2024-01-02T10:00:00Z", reference),
            ("2024-01-02T13:00:00+03:00", reference),
            ("2024-01-02T10:00:00.123Z", reference),
            ("2024-01-02 10:00:00", reference),
            ("2024-01-02", Some(1_704_153_600)),
            ("not a date", None),
            ("Tue, 31 Feb 2024 10:00:00 GMT", None),
            ("Tue, 02 Foo 2024 10:00:00 GMT", None),
            ("2024-13-02T10:00:00Z", None),
            ("", None),
        ];
        for (input, expected) in cases {
            assert_eq!(parse_date(input), expected, "{input:?}");
        }
    }

    #[test]
    fn non_ascii_zones_do_not_panic() {
        for input in [
            "Mon, 07 Nov 2022 10:00:00 +й0",
            "Mon, 07 Nov 2022 10:00:00 -йй",
            "Mon, 07 Nov 2022 10:00+й0",
            "Mon, 07 Nov 2022 10:00:00 +0й00",
            "2022-11-07T10:00:00+0й0",
            "2022-11-07T10:00:00+й",
            "2022-11-07T10:00:00-€:00",
            "2022-11-07T10:00:00+日本",
        ] {
            assert_eq!(parse_date(input), None, "{input:?}");
        }
        // Exhaustive-ish: every short suffix of mixed ASCII / multi-byte
        // characters after a sign.
        let alphabet = ['0', '9', ':', 'й', '€', '日', '+', '-'];
        for a in alphabet {
            for b in alphabet {
                for c in alphabet {
                    for prefix in ["Mon, 07 Nov 2022 10:00:00 ", "2022-11-07T10:00:00"] {
                        for sign in ['+', '-'] {
                            let _ = parse_date(&format!("{prefix}{sign}{a}{b}{c}"));
                        }
                    }
                }
            }
        }
        // In a feed, the bad date only loses the date.
        let xml = rss("", &item("z", "Zone", "Mon, 07 Nov 2022 10:00:00 +й0"));
        let feed = parse_feed(xml.as_bytes()).unwrap();
        assert_eq!(feed.episodes.len(), 1);
        assert_eq!(feed.episodes[0].published_at, None);
    }

    #[test]
    fn media_urls_are_resolved_and_must_be_http() {
        let xml = rss(
            r#"<itunes:image href="/img/cover.jpg"/>"#,
            r#"<item><guid>rel</guid><title>Relative</title>
  <itunes:image href="ep.png"/>
  <enclosure url="/audio/1.mp3" type="audio/mpeg"/></item>
<item><guid>proto</guid><title>Protocol relative</title>
  <enclosure url="//cdn.example.test/2.mp3" type="audio/mpeg"/></item>
<item><guid>file</guid><title>Local file</title>
  <enclosure url="file:///etc/passwd" type="audio/mpeg"/></item>
<item><guid>ftp</guid><title>FTP</title>
  <enclosure url="ftp://example.test/3.mp3" type="audio/mpeg"/></item>
<item><guid>fallback</guid><title>Media fallback</title>
  <enclosure url="file:///tmp/x.mp3" type="audio/mpeg"/>
  <media:content xmlns:media="http://search.yahoo.com/mrss/" url="https://cdn.example.test/4.mp3" type="audio/mpeg"/></item>
<item><guid>abs</guid><title>Absolute</title>
  <enclosure url="HTTPS://cdn.example.test/5.mp3" type="audio/mpeg"/></item>"#,
        );
        let feed =
            parse_feed_with_base(xml.as_bytes(), Some("https://host.test/feeds/rss.xml")).unwrap();
        let urls: Vec<(&str, &str)> = feed
            .episodes
            .iter()
            .map(|e| (e.guid.as_str(), e.audio_url.as_str()))
            .collect();
        assert_eq!(
            urls,
            [
                ("rel", "https://host.test/audio/1.mp3"),
                ("proto", "https://cdn.example.test/2.mp3"),
                ("fallback", "https://cdn.example.test/4.mp3"),
                ("abs", "HTTPS://cdn.example.test/5.mp3"),
            ]
        );
        assert_eq!(
            feed.image_url.as_deref(),
            Some("https://host.test/img/cover.jpg")
        );
        assert_eq!(
            feed.episodes[0].image_url.as_deref(),
            Some("https://host.test/feeds/ep.png")
        );
        for episode in &feed.episodes {
            assert!(is_remote_url(&episode.audio_url), "{}", episode.audio_url);
        }

        // Without a base, relative media cannot be resolved and is dropped;
        // protocol-relative URLs default to https.
        let feed = parse_feed(xml.as_bytes()).unwrap();
        let guids: Vec<&str> = feed.episodes.iter().map(|e| e.guid.as_str()).collect();
        assert_eq!(guids, ["proto", "fallback", "abs"]);
        assert_eq!(feed.image_url, None);
    }

    #[test]
    fn rss_with_an_unrelated_default_namespace() {
        for ns in ["http://example.test/x", "http://www.w3.org/2005/Atom"] {
            let xml = format!(
                r#"<rss version="2.0" xmlns="{ns}" xmlns:atom="http://www.w3.org/2005/Atom"><channel><title>T</title>
<atom:link href="https://a.test/feed" rel="self"/>
<item><guid>1</guid><title>One</title><enclosure url="https://a.test/1.mp3" type="audio/mpeg"/></item>
</channel></rss>"#
            );
            let feed = parse_feed(xml.as_bytes()).unwrap();
            assert_eq!(feed.title, "T", "{ns}");
            assert_eq!(feed.episodes.len(), 1, "{ns}");
            assert_eq!(feed.episodes[0].title, "One");
        }
    }

    #[test]
    fn html_pages_and_garbage_are_rejected() {
        let html = br#"<!DOCTYPE html><html><head><title>Vivat</title></head><body><p>Hi &nbsp; there</p></body></html>"#;
        let err = parse_feed(html).unwrap_err().to_string();
        assert!(err.contains("not an RSS or Atom feed"), "{err}");
        for garbage in [
            &b""[..],
            b"hello world",
            b"\x00\xff\xfe garbage",
            b"{\"json\": true}",
            b"<",
            b"<<<>>>",
        ] {
            let err = parse_feed(garbage).unwrap_err().to_string();
            assert!(
                err.contains("not an RSS or Atom feed"),
                "{garbage:?}: {err}"
            );
        }
    }

    #[test]
    fn truncated_and_malformed_xml_never_panics() {
        let items: String = (0..5)
            .map(|i| item(&format!("g{i}"), &format!("T {i} &amp; &nbsp;"), "bad date"))
            .collect();
        let xml = rss(
            "<description><![CDATA[cdata]]></description><itunes:image href=\"x\"/>",
            &items,
        );
        let bytes = xml.as_bytes();
        for len in 0..=bytes.len() {
            let _ = parse_feed(&bytes[..len]);
        }
        // Mismatched end tags are tolerated.
        let sloppy = r#"<rss><channel><title>S</title><item><title>A</b></title><guid>1</guid><enclosure url="https://x.test/1.mp3"/></item></channel></rss>"#;
        let feed = parse_feed(sloppy.as_bytes()).unwrap();
        assert_eq!(feed.episodes.len(), 1);
        assert_eq!(feed.episodes[0].title, "A");
        // Truncation after complete items keeps them.
        let third = xml.find("<title>T 3").unwrap();
        let cut = &bytes[..xml[..third].rfind("<item>").unwrap()];
        let feed = parse_feed(cut).unwrap();
        assert_eq!(feed.episodes.len(), 3);
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        let depth = 100_000;
        let mut xml = String::from("<rss><channel><title>Deep</title>");
        xml.push_str(&"<x>".repeat(depth));
        xml.push_str(&"</x>".repeat(depth));
        xml.push_str(&item("after", "After", ""));
        xml.push_str("</channel></rss>");
        let feed = parse_feed(xml.as_bytes()).unwrap();
        assert_eq!(feed.episodes.len(), 1);
    }

    #[test]
    fn cyrillic_titles_in_utf8_and_windows_1251() {
        let xml = rss(
            "<itunes:author>Иван Петров</itunes:author>",
            &item(
                "1",
                "Выпуск №1: «Привет»",
                "Mon, 01 Jan 2024 00:00:00 +0300",
            ),
        )
        .replace("Test Cast", "Подкаст");
        let feed = parse_feed(xml.as_bytes()).unwrap();
        assert_eq!(feed.title, "Подкаст");
        assert_eq!(feed.author.as_deref(), Some("Иван Петров"));
        assert_eq!(feed.episodes[0].title, "Выпуск №1: «Привет»");

        let cp1251 = xml.replace("encoding=\"UTF-8\"", "encoding=\"windows-1251\"");
        let (encoded, _, unmappable) = encoding_rs::WINDOWS_1251.encode(&cp1251);
        assert!(!unmappable);
        let feed = parse_feed(&encoded).unwrap();
        assert_eq!(feed.title, "Подкаст");
        assert_eq!(feed.episodes[0].title, "Выпуск №1: «Привет»");

        // Invalid UTF-8 without a declaration is decoded lossily.
        let mut broken = xml.into_bytes();
        let at = broken
            .windows(7)
            .position(|w| w == b"Test Ca")
            .unwrap_or(10);
        broken.insert(at, 0xff);
        assert!(parse_feed(&broken).is_ok());
    }

    #[test]
    fn namespace_prefixes_are_matched_tolerantly() {
        // Undeclared `itunes:` prefix and a custom prefix bound to the
        // iTunes namespace.
        let xml = format!(
            r#"<rss><channel><title>P</title><itunes:author>Undeclared</itunes:author>
<item xmlns:it="{ITUNES_NS}"><guid>1</guid><it:title>Custom prefix</it:title><it:duration>90</it:duration>
<enclosure url="https://x.test/1.mp3" type="audio/mpeg"/></item></channel></rss>"#
        );
        let feed = parse_feed(xml.as_bytes()).unwrap();
        assert_eq!(feed.author.as_deref(), Some("Undeclared"));
        assert_eq!(feed.episodes[0].title, "Custom prefix");
        assert_eq!(feed.episodes[0].duration_seconds, Some(90.0));
    }

    #[test]
    fn long_descriptions_are_truncated() {
        let long = "word ".repeat(2000);
        let text = html_to_text(&long, 4000);
        assert!(text.chars().count() <= 4000);
        assert!(text.ends_with('…'));
        let xml = rss(&format!("<description>{long}</description>"), "");
        let feed = parse_feed(xml.as_bytes()).unwrap();
        assert!(feed.description.unwrap().chars().count() <= 4000);
    }
}

mod store_tests {
    use super::*;

    fn parsed(guid: &str, title: &str, published_at: Option<i64>) -> ParsedEpisode {
        ParsedEpisode {
            guid: guid.to_string(),
            title: title.to_string(),
            description: None,
            published_at,
            duration_seconds: Some(60.0),
            audio_url: format!("https://cdn.example.test/{guid}.mp3"),
            mime_type: Some("audio/mpeg".to_string()),
            byte_len: Some(1000),
            link: None,
            image_url: None,
        }
    }

    fn feed(title: &str, episodes: Vec<ParsedEpisode>) -> ParsedFeed {
        ParsedFeed {
            title: title.to_string(),
            author: Some("Author".to_string()),
            description: Some("About".to_string()),
            link: None,
            image_url: Some("https://example.test/cover.png".to_string()),
            episodes,
        }
    }

    #[test]
    fn add_list_and_episode_order() {
        let store = Store::open_in_memory().unwrap();
        let b = store
            .add_podcast(
                "https://b.test/feed",
                &feed(
                    "beta",
                    vec![
                        parsed("old", "Old", Some(100)),
                        parsed("undated", "Undated", None),
                        parsed("new", "New", Some(300)),
                        parsed("mid", "Mid", Some(200)),
                    ],
                ),
                10,
            )
            .unwrap();
        let a = store
            .add_podcast("https://a.test/feed", &feed("Alpha", vec![]), 11)
            .unwrap();
        let list = store.list_podcasts().unwrap();
        let titles: Vec<_> = list.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, ["Alpha", "beta"]);
        assert_eq!(list[0].id, a);
        assert_eq!(list[0].episode_count, 0);
        assert_eq!(list[0].latest_published_at, None);
        let beta = &list[1];
        assert_eq!(beta.id, b);
        assert_eq!(beta.episode_count, 4);
        assert_eq!(beta.latest_published_at, Some(300));
        assert_eq!(beta.last_checked_at, Some(10));
        assert_eq!(beta.last_error, None);
        assert_eq!(beta.author.as_deref(), Some("Author"));
        assert_eq!(store.podcast(b).unwrap().as_ref(), Some(beta));
        assert_eq!(
            store.podcast_by_feed_url("https://b.test/feed").unwrap(),
            Some(beta.clone())
        );
        assert_eq!(store.podcast(9999).unwrap(), None);

        let episodes = store.episodes(b).unwrap();
        let guids: Vec<_> = episodes.iter().map(|e| e.guid.as_str()).collect();
        assert_eq!(guids, ["new", "mid", "old", "undated"]);
        let first = &episodes[0];
        assert_eq!(store.episode(first.id).unwrap().as_ref(), Some(first));
        assert_eq!(first.byte_len, Some(1000));
        assert_eq!(first.podcast_id, b);
    }

    #[test]
    fn apply_feed_counts_changes_and_keeps_missing_episodes() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .add_podcast(
                "https://a.test/feed",
                &feed(
                    "Cast",
                    vec![
                        parsed("1", "One", Some(1)),
                        parsed("2", "Two", Some(2)),
                        parsed("3", "Three", Some(3)),
                    ],
                ),
                10,
            )
            .unwrap();
        let before = store.episodes(id).unwrap();
        store.record_check(id, 15, Some("boom")).unwrap();
        assert_eq!(
            store.podcast(id).unwrap().unwrap().last_error.as_deref(),
            Some("boom")
        );

        let mut changed = parsed("2", "Two (remastered)", Some(2));
        changed.duration_seconds = Some(61.5);
        let next = feed(
            "Cast renamed",
            vec![
                parsed("4", "Four", Some(4)),
                changed,
                parsed("3", "Three", Some(3)),
            ],
        );
        let summary = store.apply_feed(id, &next, 20).unwrap();
        assert_eq!(
            summary,
            RefreshSummary {
                new_episodes: 1,
                updated_episodes: 1,
                not_modified: false
            }
        );
        // Re-applying the same feed changes nothing.
        let again = store.apply_feed(id, &next, 21).unwrap();
        assert_eq!(again, RefreshSummary::default());

        let podcast = store.podcast(id).unwrap().unwrap();
        assert_eq!(podcast.title, "Cast renamed");
        assert_eq!(podcast.episode_count, 4);
        assert_eq!(podcast.last_checked_at, Some(21));
        assert_eq!(podcast.last_error, None);

        let after = store.episodes(id).unwrap();
        let guids: Vec<_> = after.iter().map(|e| e.guid.as_str()).collect();
        assert_eq!(guids, ["4", "3", "2", "1"], "missing episode 1 is kept");
        for old in &before {
            let new = after.iter().find(|e| e.guid == old.guid).unwrap();
            assert_eq!(new.id, old.id, "row ids are stable");
        }
        let two = after.iter().find(|e| e.guid == "2").unwrap();
        assert_eq!(two.title, "Two (remastered)");
        assert_eq!(two.duration_seconds, Some(61.5));

        assert!(store.apply_feed(9999, &next, 22).is_err());
    }

    #[test]
    fn duplicate_feed_url_is_rejected() {
        let store = Store::open_in_memory().unwrap();
        store
            .add_podcast("https://a.test/feed", &feed("Cast", vec![]), 1)
            .unwrap();
        let err = store
            .add_podcast("https://a.test/feed", &feed("Cast", vec![]), 2)
            .unwrap_err();
        assert!(err.to_string().contains("already subscribed"), "{err}");
        assert_eq!(store.list_podcasts().unwrap().len(), 1);
    }

    #[test]
    fn validators_covers_and_removal_cascade() {
        let store = Store::open_in_memory().unwrap();
        let id = store
            .add_podcast(
                "https://a.test/feed",
                &feed("Cast", vec![parsed("1", "One", Some(1))]),
                1,
            )
            .unwrap();
        let other = store
            .add_podcast(
                "https://b.test/feed",
                &feed("Other", vec![parsed("1", "One", Some(1))]),
                1,
            )
            .unwrap();
        assert_eq!(store.http_validators(id).unwrap(), (None, None));
        store
            .set_http_validators(id, Some("\"v1\""), Some("Mon, 01 Jan 2024 00:00:00 GMT"))
            .unwrap();
        assert_eq!(
            store.http_validators(id).unwrap(),
            (
                Some("\"v1\"".to_string()),
                Some("Mon, 01 Jan 2024 00:00:00 GMT".to_string())
            )
        );
        store
            .set_cover(id, Some("https://example.test/c.png"), Some("/tmp/c.png"))
            .unwrap();
        assert_eq!(
            store.cover_source(id).unwrap(),
            (
                Some("https://example.test/c.png".to_string()),
                Some("/tmp/c.png".to_string())
            )
        );
        assert_eq!(
            store.podcast(id).unwrap().unwrap().cover_path.as_deref(),
            Some("/tmp/c.png")
        );
        let episode_id = store.episodes(id).unwrap()[0].id;

        assert_eq!(
            store.remove_podcast(id).unwrap().as_deref(),
            Some("/tmp/c.png")
        );
        assert_eq!(store.podcast(id).unwrap(), None);
        assert_eq!(store.episode(episode_id).unwrap(), None);
        assert!(store.episodes(id).unwrap().is_empty());
        assert_eq!(store.episodes(other).unwrap().len(), 1);
        assert_eq!(store.remove_podcast(id).unwrap(), None);
    }

    #[test]
    fn ids_are_never_reused_after_removal() {
        let store = Store::open_in_memory().unwrap();
        let first = store
            .add_podcast(
                "https://a.test/feed",
                &feed("A", vec![parsed("1", "One", Some(1))]),
                1,
            )
            .unwrap();
        let second = store
            .add_podcast(
                "https://b.test/feed",
                &feed(
                    "B",
                    vec![parsed("1", "One", Some(1)), parsed("2", "Two", Some(2))],
                ),
                1,
            )
            .unwrap();
        let removed_episodes: Vec<i64> = store
            .episodes(second)
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        // Remove the newest subscription, then subscribe to something else.
        store.remove_podcast(second).unwrap();
        let third = store
            .add_podcast(
                "https://c.test/feed",
                &feed("C", vec![parsed("1", "One", Some(1))]),
                2,
            )
            .unwrap();
        assert!(third > second && second > first);
        let new_episode = store.episodes(third).unwrap()[0].id;
        assert!(
            removed_episodes.iter().all(|old| new_episode > *old),
            "{new_episode} reuses one of {removed_episodes:?}"
        );
        // A refresh of the removed podcast that finishes late cannot touch
        // the new subscription.
        assert!(store.apply_feed(second, &feed("B", vec![]), 3).is_err());
        assert_eq!(store.podcast(third).unwrap().unwrap().title, "C");
    }

    #[test]
    fn migrations_are_idempotent_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("podcasts.sqlite3");
        let id = {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.schema_version().unwrap(), 1);
            store
                .add_podcast(
                    "https://a.test/feed",
                    &feed("Cast", vec![parsed("1", "One", Some(1))]),
                    1,
                )
                .unwrap()
        };
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 1);
        assert_eq!(store.podcast(id).unwrap().unwrap().episode_count, 1);
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.list_podcasts().unwrap().len(), 1);
    }
}

mod mapping {
    use super::*;

    fn podcast() -> Podcast {
        Podcast {
            id: 3,
            feed_url: "https://a.test/feed".to_string(),
            title: "Cast".to_string(),
            author: None,
            description: None,
            link: None,
            image_url: None,
            cover_path: Some("/covers/3.png".to_string()),
            episode_count: 1,
            latest_published_at: None,
            last_checked_at: None,
            last_error: None,
        }
    }

    fn episode() -> Episode {
        Episode {
            id: 42,
            podcast_id: 3,
            guid: "g".to_string(),
            title: "Episode".to_string(),
            description: None,
            published_at: Some(1_704_189_600),
            duration_seconds: Some(125.5),
            audio_url: "https://cdn.example.test/e.mp3".to_string(),
            mime_type: Some("audio/mpeg".to_string()),
            byte_len: Some(9_999),
            link: None,
            image_url: None,
        }
    }

    #[test]
    fn episode_track_maps_fields() {
        let track = episode_track(&podcast(), &episode());
        assert_eq!(track.id, -(EPISODE_TRACK_ID_BASE + 42));
        assert_eq!(track.title, "Episode");
        assert_eq!(track.artists.len(), 1);
        assert_eq!(track.artists[0].id, -1);
        assert_eq!(track.artists[0].name, "Cast");
        assert!(track.featured_artists.is_empty());
        assert_eq!(track.release_id, -1);
        assert_eq!(track.release_title, "Cast");
        assert_eq!(track.release_year, Some(2024));
        assert_eq!(track.track_number, None);
        assert_eq!(track.disc_number, None);
        assert_eq!(track.file_path, "https://cdn.example.test/e.mp3");
        assert_eq!(track.content_id, None);
        assert_eq!(track.cover_path.as_deref(), Some("/covers/3.png"));
        assert_eq!(track.audio_format.as_deref(), Some("MP3"));
        assert_eq!(track.file_size_bytes, Some(9_999));
        assert_eq!(track.duration_seconds, 125.5);
        assert_eq!(track.play_count, 0);
        assert!(track.fed.is_none());

        let mut bare = episode();
        bare.duration_seconds = None;
        bare.published_at = None;
        bare.mime_type = Some("audio/x-m4a".to_string());
        bare.byte_len = None;
        let track = episode_track(&podcast(), &bare);
        assert_eq!(track.duration_seconds, 0.0);
        assert_eq!(track.release_year, None);
        assert_eq!(track.audio_format.as_deref(), Some("M4A"));
        assert_eq!(track.file_size_bytes, None);
    }

    #[test]
    fn track_id_inverse() {
        let track = episode_track(&podcast(), &episode());
        assert_eq!(episode_id_from_track_id(track.id), Some(42));
        assert_eq!(episode_id_from_track_id(1), None);
        assert_eq!(episode_id_from_track_id(0), None);
        assert_eq!(episode_id_from_track_id(-1), None);
        assert_eq!(episode_id_from_track_id(-EPISODE_TRACK_ID_BASE), None);
        assert_eq!(episode_id_from_track_id(i64::MIN), None);
        assert_eq!(episode_id_from_track_id(i64::MAX), None);
        // Only the band reserved for episodes maps back.
        let max = EPISODE_TRACK_ID_BASE - 1;
        assert_eq!(
            episode_id_from_track_id(-(EPISODE_TRACK_ID_BASE + max)),
            Some(max)
        );
        assert_eq!(
            episode_id_from_track_id(-(EPISODE_TRACK_ID_BASE + 1)),
            Some(1)
        );
        assert_eq!(episode_id_from_track_id(-(2 * EPISODE_TRACK_ID_BASE)), None);
        // Hash-derived placeholder ids (as used by device sync) spread over
        // the whole negative range and must not look like episodes.
        let mut hits = 0;
        for seed in 0..10_000u64 {
            let hash = blake3::hash(&seed.to_le_bytes());
            let bytes: [u8; 8] = hash.as_bytes()[..8].try_into().unwrap();
            let placeholder = -(i64::from_le_bytes(bytes) & i64::MAX);
            hits += usize::from(episode_id_from_track_id(placeholder).is_some());
        }
        assert_eq!(hits, 0);
    }

    #[test]
    fn remote_urls() {
        assert!(is_remote_url("http://a.test/x.mp3"));
        assert!(is_remote_url("HTTPS://a.test/x.mp3"));
        assert!(!is_remote_url("/music/x.mp3"));
        assert!(!is_remote_url("ftp://a.test/x"));
        assert!(!is_remote_url("http:/"));
        assert!(!is_remote_url(""));
    }

    #[test]
    fn normalizes_feed_urls() {
        let ok = [
            (
                "https://podster.test/rss.xml?pid=778",
                "https://podster.test/rss.xml?pid=778",
            ),
            ("  http://a.test/feed  ", "http://a.test/feed"),
            ("a.test/feed.xml", "https://a.test/feed.xml"),
            ("feed://a.test/rss", "https://a.test/rss"),
            ("feed:https://a.test/rss", "https://a.test/rss"),
            ("podcast://a.test/rss", "https://a.test/rss"),
            ("HTTPS://A.TEST/Feed", "https://a.test/Feed"),
            ("a.test:8080/rss", "https://a.test:8080/rss"),
            // A URL embedded in the query is not a scheme.
            (
                "example.test/rss?src=https://other.test",
                "https://example.test/rss?src=https://other.test",
            ),
            (
                "localhost:8080/feed?u=http://x.test/",
                "https://localhost:8080/feed?u=http://x.test/",
            ),
        ];
        for (input, expected) in ok {
            assert_eq!(normalize_feed_url(input).unwrap(), expected, "{input:?}");
        }
        for bad in [
            "",
            "   ",
            "ftp://a.test/feed",
            "https://a.test/my feed",
            "https://",
            "file:///etc/passwd",
            "javascript:alert(1)",
        ] {
            assert!(normalize_feed_url(bad).is_err(), "{bad:?}");
        }
    }
}

mod network {
    use super::*;
    use crate::podcasts::test_server::{Route, TestServer};
    use std::sync::Arc;

    fn png() -> Vec<u8> {
        let image = image::RgbImage::from_pixel(2, 2, image::Rgb([200, 10, 10]));
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn feed_xml(server: &TestServer, guids: &[&str]) -> String {
        let items: String = guids
            .iter()
            .enumerate()
            .map(|(i, guid)| {
                item(
                    guid,
                    &format!("Episode {guid}"),
                    &format!("Mon, 0{} Jan 2024 00:00:00 GMT", i + 1),
                )
            })
            .collect();
        rss(
            &format!(r#"<itunes:image href="{}"/>"#, server.url("/cover.png")),
            &items,
        )
    }

    struct Env {
        server: TestServer,
        store: Arc<Store>,
        fetcher: Fetcher,
        covers: tempfile::TempDir,
    }

    fn env() -> Env {
        let covers = tempfile::tempdir().unwrap();
        Env {
            server: TestServer::start(),
            store: Arc::new(Store::open_in_memory().unwrap()),
            fetcher: Fetcher::new(covers.path().join("covers")).unwrap(),
            covers,
        }
    }

    #[tokio::test]
    async fn subscribe_stores_metadata_and_cover() {
        let env = env();
        let xml = feed_xml(&env.server, &["a", "b"]);
        env.server.set(
            "/feed.xml",
            Route::new(xml, "application/rss+xml").etag("\"v1\""),
        );
        env.server.set("/cover.png", Route::new(png(), "image/png"));

        let id = subscribe(
            Arc::clone(&env.store),
            &env.fetcher,
            &env.server.url("/feed.xml"),
            100,
        )
        .await
        .unwrap();
        let podcast = env.store.podcast(id).unwrap().unwrap();
        assert_eq!(podcast.title, "Test Cast");
        assert_eq!(podcast.episode_count, 2);
        assert_eq!(podcast.last_checked_at, Some(100));
        let cover = podcast.cover_path.clone().unwrap();
        assert!(cover.ends_with(".png"), "{cover}");
        assert!(std::path::Path::new(&cover).is_file());
        assert!(cover.starts_with(&*env.covers.path().to_string_lossy()));
        assert_eq!(
            env.store.http_validators(id).unwrap().0.as_deref(),
            Some("\"v1\"")
        );
        // Only the feed and the cover were requested — never the audio.
        assert!(env.server.requests("/a.mp3").is_empty());

        let err = subscribe(
            Arc::clone(&env.store),
            &env.fetcher,
            &env.server.url("/feed.xml"),
            101,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("already subscribed to Test Cast"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn subscribe_rejects_non_feeds_without_storing() {
        let env = env();
        env.server.set(
            "/page",
            Route::new("<!DOCTYPE html><html><body>Hi</body></html>", "text/html"),
        );
        env.server
            .set("/missing", Route::new("nope", "text/plain").status(404));
        let err = subscribe(
            Arc::clone(&env.store),
            &env.fetcher,
            &env.server.url("/page"),
            1,
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("not an RSS or Atom feed"),
            "{err:#}"
        );
        let err = subscribe(
            Arc::clone(&env.store),
            &env.fetcher,
            &env.server.url("/missing"),
            1,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("404"), "{err:#}");
        assert!(env.store.list_podcasts().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refresh_uses_etag_and_picks_up_new_episodes() {
        let env = env();
        let feed_url = env.server.url("/feed.xml");
        env.server.set(
            "/feed.xml",
            Route::new(feed_xml(&env.server, &["a"]), "application/rss+xml").etag("\"v1\""),
        );
        env.server.set("/cover.png", Route::new(png(), "image/png"));
        let id = subscribe(Arc::clone(&env.store), &env.fetcher, &feed_url, 1)
            .await
            .unwrap();

        let summary = refresh(Arc::clone(&env.store), &env.fetcher, id, 2)
            .await
            .unwrap();
        assert_eq!(
            summary,
            RefreshSummary {
                not_modified: true,
                ..Default::default()
            }
        );
        assert_eq!(
            env.store.podcast(id).unwrap().unwrap().last_checked_at,
            Some(2)
        );
        let cover_requests = env.server.requests("/cover.png").len();
        assert_eq!(cover_requests, 1, "cover is not downloaded again");

        env.server.set(
            "/feed.xml",
            Route::new(feed_xml(&env.server, &["a", "b"]), "application/rss+xml").etag("\"v2\""),
        );
        let summary = refresh(Arc::clone(&env.store), &env.fetcher, id, 3)
            .await
            .unwrap();
        assert_eq!(summary.new_episodes, 1);
        assert!(!summary.not_modified);
        assert_eq!(env.store.episodes(id).unwrap().len(), 2);
        assert_eq!(
            env.store.http_validators(id).unwrap().0.as_deref(),
            Some("\"v2\"")
        );
        let sent: Vec<_> = env.server.requests("/feed.xml");
        assert_eq!(sent.len(), 3);

        // A deleted cover file is downloaded again on the next refresh.
        let cover = env.store.podcast(id).unwrap().unwrap().cover_path.unwrap();
        std::fs::remove_file(&cover).unwrap();
        refresh(Arc::clone(&env.store), &env.fetcher, id, 4)
            .await
            .unwrap();
        assert!(std::path::Path::new(&cover).is_file());
        assert_eq!(env.server.requests("/cover.png").len(), 2);
    }

    #[tokio::test]
    async fn relative_enclosures_resolve_against_the_feed_url() {
        let env = env();
        let xml = rss(
            "",
            r#"<item><guid>r</guid><title>Relative</title>
  <enclosure url="../audio/r.mp3" type="audio/mpeg"/></item>
<item><guid>f</guid><title>Local file</title>
  <enclosure url="file:///etc/passwd" type="audio/mpeg"/></item>"#,
        );
        env.server
            .set("/feeds/main.xml", Route::new(xml, "application/rss+xml"));
        let id = subscribe(
            Arc::clone(&env.store),
            &env.fetcher,
            &env.server.url("/feeds/main.xml"),
            1,
        )
        .await
        .unwrap();
        let episodes = env.store.episodes(id).unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].audio_url, env.server.url("/audio/r.mp3"));
    }

    #[tokio::test]
    async fn refresh_failure_is_recorded() {
        let env = env();
        let feed_url = env.server.url("/feed.xml");
        env.server.set(
            "/feed.xml",
            Route::new(feed_xml(&env.server, &["a"]), "application/rss+xml"),
        );
        let id = subscribe(Arc::clone(&env.store), &env.fetcher, &feed_url, 1)
            .await
            .unwrap();
        env.server
            .set("/feed.xml", Route::new("oops", "text/plain").status(500));
        let err = refresh(Arc::clone(&env.store), &env.fetcher, id, 5)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("500"));
        let podcast = env.store.podcast(id).unwrap().unwrap();
        assert_eq!(podcast.last_checked_at, Some(5));
        assert!(podcast.last_error.unwrap().contains("500"));
        assert_eq!(
            podcast.episode_count, 1,
            "episodes survive a failed refresh"
        );
    }

    #[tokio::test]
    async fn feed_body_is_capped() {
        let env = env();
        let huge = vec![b' '; fetch::MAX_FEED_BYTES + 1];
        env.server
            .set("/declared", Route::new(huge.clone(), "application/rss+xml"));
        env.server.set(
            "/streamed",
            Route::new(huge, "application/rss+xml").no_length(),
        );
        for path in ["/declared", "/streamed"] {
            let result = env
                .fetcher
                .fetch_feed(&env.server.url(path), None, None)
                .await;
            let err = result.err().expect("oversized feed must fail");
            assert!(format!("{err:#}").contains("too large"), "{path}: {err:#}");
        }
    }

    #[tokio::test]
    async fn cover_must_be_an_image() {
        let env = env();
        env.server
            .set("/not-image", Route::new("hello", "image/png"));
        assert!(
            env.fetcher
                .fetch_cover(1, &env.server.url("/not-image"))
                .await
                .is_err()
        );
    }
}

mod streaming {
    use crate::podcasts::stream::{self, MAX_BUFFERED};
    use crate::podcasts::test_server::{Route, TestServer, noise};
    use std::io::{ErrorKind, Read, Seek, SeekFrom};
    use std::time::{Duration, Instant};

    const MIB: usize = 1024 * 1024;

    fn read_to_end(reader: &mut impl Read) -> Vec<u8> {
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        out
    }

    fn read_n(reader: &mut impl Read, len: usize) -> Vec<u8> {
        let mut out = vec![0; len];
        reader.read_exact(&mut out).unwrap();
        out
    }

    fn serve(route: Route) -> (TestServer, String) {
        let server = TestServer::start();
        server.set("/audio.mp3", route);
        let url = server.url("/audio.mp3");
        (server, url)
    }

    #[test]
    fn sequential_read_matches_source() {
        let body = noise(3 * MIB + 123, 1);
        let (server, url) = serve(Route::new(body.clone(), "audio/mpeg; charset=binary"));
        let mut opened = stream::open(&url).unwrap();
        assert_eq!(opened.byte_len, Some(body.len() as u64));
        assert_eq!(opened.mime_type.as_deref(), Some("audio/mpeg"));
        assert!(opened.seekable);
        assert_eq!(read_to_end(&mut opened.reader), body);
        let mut buf = [0u8; 16];
        assert_eq!(opened.reader.read(&mut buf).unwrap(), 0, "EOF stays EOF");
        assert_eq!(
            server.requests("/audio.mp3"),
            [Some("bytes=0-".to_string())]
        );
    }

    #[test]
    fn seeks_forward_backward_and_from_end() {
        let body = noise(12 * MIB, 2);
        let (server, url) = serve(Route::new(body.clone(), "audio/mpeg"));
        let mut reader = stream::open(&url).unwrap().reader;
        assert_eq!(read_n(&mut reader, 1000), body[..1000]);

        // Far forward: outside the read-ahead window -> new range request.
        let far = 11 * MIB + 7;
        assert_eq!(
            reader.seek(SeekFrom::Start(far as u64)).unwrap(),
            far as u64
        );
        assert_eq!(read_n(&mut reader, 70_000), body[far..far + 70_000]);
        assert!(
            server
                .requests("/audio.mp3")
                .contains(&Some(format!("bytes={far}-")))
        );

        // Small backward seek within the kept window: no new request.
        let requests = server.requests("/audio.mp3").len();
        reader.seek(SeekFrom::Current(-50_000)).unwrap();
        let pos = far + 20_000;
        assert_eq!(read_n(&mut reader, 1000), body[pos..pos + 1000]);
        assert_eq!(server.requests("/audio.mp3").len(), requests);

        // Back into the permanently kept head: no new request.
        reader.seek(SeekFrom::Start(10)).unwrap();
        assert_eq!(read_n(&mut reader, 100), body[10..110]);
        assert_eq!(server.requests("/audio.mp3").len(), requests);

        // Backward outside the window and the head.
        let back = 2 * MIB + 3;
        reader.seek(SeekFrom::Start(back as u64)).unwrap();
        assert_eq!(read_n(&mut reader, 300_000), body[back..back + 300_000]);

        // From the end.
        let end = reader.seek(SeekFrom::End(-100)).unwrap();
        assert_eq!(end, body.len() as u64 - 100);
        assert_eq!(read_to_end(&mut reader), body[body.len() - 100..]);

        // Past the end.
        let beyond = reader.seek(SeekFrom::End(50)).unwrap();
        assert_eq!(beyond, body.len() as u64 + 50);
        let mut buf = [0u8; 8];
        assert_eq!(reader.read(&mut buf).unwrap(), 0);
        assert_eq!(
            reader
                .seek(SeekFrom::Current(-(beyond as i64) - 1))
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn server_without_range_support() {
        let body = noise(3 * MIB, 3);
        let (server, url) = serve(Route::new(body.clone(), "audio/mpeg").no_ranges());
        let opened = stream::open(&url).unwrap();
        assert!(opened.seekable, "length is known");
        let mut reader = opened.reader;
        assert_eq!(read_n(&mut reader, 400_000), body[..400_000]);
        // Forward seek: read and discard on the same response.
        reader.seek(SeekFrom::Start(2 * MIB as u64)).unwrap();
        assert_eq!(read_n(&mut reader, 1000), body[2 * MIB..2 * MIB + 1000]);
        // Backward seek outside window and head: restart from 0.
        reader.seek(SeekFrom::Start(300_000)).unwrap();
        let _ = read_n(&mut reader, 1);
        reader.seek(SeekFrom::Start(300_000)).unwrap();
        assert_eq!(read_to_end(&mut reader), body[300_000..]);
        let requests = server.requests("/audio.mp3");
        assert!(requests.len() >= 2, "{requests:?}");
        assert!(requests[1..].iter().all(Option::is_none), "{requests:?}");
        reader.seek(SeekFrom::End(-10)).unwrap();
        assert_eq!(read_to_end(&mut reader), body[body.len() - 10..]);
    }

    #[test]
    fn unknown_length_is_not_seekable_from_end() {
        let body = noise(MIB + 5, 4);
        let (_server, url) = serve(
            Route::new(body.clone(), "audio/mpeg")
                .no_ranges()
                .no_length(),
        );
        let opened = stream::open(&url).unwrap();
        assert!(!opened.seekable);
        assert_eq!(opened.byte_len, None);
        let mut reader = opened.reader;
        assert_eq!(
            reader.seek(SeekFrom::End(0)).unwrap_err().kind(),
            ErrorKind::Unsupported
        );
        assert_eq!(read_to_end(&mut reader), body);
    }

    #[test]
    fn transient_disconnects_are_retried() {
        let body = noise(2 * MIB, 5);
        let (server, url) = serve(Route::new(body.clone(), "audio/mpeg").cut(700_000, 2));
        let mut reader = stream::open(&url).unwrap().reader;
        assert_eq!(read_to_end(&mut reader), body);
        let requests = server.requests("/audio.mp3");
        assert_eq!(requests.len(), 3, "{requests:?}");
        assert_eq!(requests[1], Some("bytes=700000-".to_string()));
    }

    #[test]
    fn persistent_failure_surfaces_an_error() {
        let body = noise(2 * MIB, 6);
        let (_server, url) = serve(Route::new(body, "audio/mpeg").cut(100_000, 100));
        let mut reader = stream::open(&url).unwrap().reader;
        let mut out = Vec::new();
        let started = Instant::now();
        let err = reader.read_to_end(&mut out).unwrap_err();
        assert_eq!(out.len(), 100_000);
        assert_eq!(err.kind(), ErrorKind::TimedOut, "{err}");
        assert!(err.to_string().contains("stalled"), "{err}");
        // Bounded by the starvation deadline, not by a retry count.
        assert!(started.elapsed() >= stream::STARVE_TIMEOUT);
        assert!(started.elapsed() < stream::STARVE_TIMEOUT * 3);
    }

    /// An outage that ends while buffered audio is still playing must not
    /// end playback: the downloader keeps retrying instead of giving up
    /// after a fixed number of attempts.
    #[test]
    fn outage_shorter_than_the_buffer_is_bridged() {
        let body = noise(3 * MIB, 10);
        let (server, url) = serve(Route::new(body.clone(), "audio/mpeg").cut(MIB, 4));
        let mut reader = stream::open(&url).unwrap().reader;
        assert_eq!(read_n(&mut reader, 1000), body[..1000]);
        // "Play" buffered audio while the server fails four times and then
        // recovers.
        std::thread::sleep(Duration::from_millis(4500));
        assert_eq!(read_to_end(&mut reader), body[1000..]);
        let requests = server.requests("/audio.mp3");
        assert_eq!(requests.len(), 5, "{requests:?}");
        assert!(
            requests[1..]
                .iter()
                .all(|range| range.as_deref() == Some("bytes=1048576-")),
            "{requests:?}"
        );
    }

    #[test]
    fn client_errors_are_reported_without_waiting() {
        let body = noise(2 * MIB, 11);
        let (server, url) = serve(Route::new(body, "audio/mpeg").cut(100_000, 1));
        let mut reader = stream::open(&url).unwrap().reader;
        // The signed URL "expires" after the first response.
        server.set("/audio.mp3", Route::new("gone", "text/plain").status(404));
        let started = Instant::now();
        let mut out = Vec::new();
        let err = reader.read_to_end(&mut out).unwrap_err();
        assert_eq!(out.len(), 100_000);
        assert!(err.to_string().contains("404"), "{err}");
        assert!(
            started.elapsed() < stream::STARVE_TIMEOUT,
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn large_body_keeps_memory_bounded() {
        let body = noise(20 * MIB, 7);
        let (_server, url) = serve(Route::new(body.clone(), "audio/mpeg"));
        let mut reader = stream::open(&url).unwrap().reader;
        let mut buf = vec![0u8; 32 * 1024];
        let mut pos = 0usize;
        let mut hasher_ok = true;
        loop {
            let read = reader.read(&mut buf).unwrap();
            if read == 0 {
                break;
            }
            hasher_ok &= buf[..read] == body[pos..pos + read];
            pos += read;
            if pos % (2 * MIB) < 32 * 1024 && pos + stream::READ_AHEAD as usize <= body.len() {
                // Let the downloader run ahead until it pauses at its cap.
                let deadline = Instant::now() + Duration::from_secs(5);
                while reader.buffered_bytes() < stream::READ_AHEAD as usize
                    && Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            assert!(reader.buffered_bytes() <= MAX_BUFFERED);
        }
        assert_eq!(pos, body.len());
        assert!(hasher_ok, "data mismatch");
        let peak = reader.peak_buffered_bytes();
        assert!(peak <= MAX_BUFFERED, "peak {peak} > {MAX_BUFFERED}");
        assert!(
            peak > 4 * MIB,
            "read-ahead should have buffered, peak {peak}"
        );
        // Far seeks restart the window without growing its allocation.
        for target in [MIB + 5, 15 * MIB, 3 * MIB] {
            reader.seek(SeekFrom::Start(target as u64)).unwrap();
            assert_eq!(read_n(&mut reader, 1000), body[target..target + 1000]);
            let deadline = Instant::now() + Duration::from_secs(5);
            while reader.buffered_bytes() < stream::READ_AHEAD as usize && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        // The allocation, not just the length, is bounded.
        let allocated = reader.peak_allocated_bytes();
        assert!(
            allocated <= MAX_BUFFERED,
            "allocated {allocated} > {MAX_BUFFERED}"
        );
    }

    #[test]
    fn dropping_the_reader_stops_the_thread() {
        let body = noise(20 * MIB, 8);
        let (_server, url) = serve(Route::new(body.clone(), "audio/mpeg"));
        let mut reader = stream::open(&url).unwrap().reader;
        assert_eq!(read_n(&mut reader, 100_000), body[..100_000]);
        let exited = reader.exit_probe();
        let deadline = Instant::now() + Duration::from_secs(5);
        // Let it fill the read-ahead window (it then pauses).
        while reader.buffered_bytes() < stream::READ_AHEAD as usize && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!exited());
        drop(reader);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(exited(), "downloader thread did not stop");

        // Also while actively downloading (not paused).
        let mut reader = stream::open(&url).unwrap().reader;
        let exited = reader.exit_probe();
        let _ = read_n(&mut reader, 10);
        drop(reader);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !exited() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(exited(), "downloader thread did not stop");
    }

    /// The documented calling convention: `open` on Tokio's blocking pool,
    /// reads on another plain thread (like the audio thread).
    #[test]
    fn open_from_spawn_blocking() {
        let body = noise(MIB, 9);
        let (server, url) = serve(Route::new(body.clone(), "audio/mpeg"));
        server.set("/missing.mp3", Route::new("x", "text/plain").status(404));
        let missing = server.url("/missing.mp3");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (opened, failed) = runtime.block_on(async move {
            let opened = tokio::task::spawn_blocking(move || stream::open(&url))
                .await
                .unwrap();
            let failed = tokio::task::spawn_blocking(move || stream::open(&missing).is_err())
                .await
                .unwrap();
            (opened, failed)
        });
        assert!(failed);
        let mut reader = opened.unwrap().reader;
        let read = std::thread::spawn(move || read_to_end(&mut reader))
            .join()
            .unwrap();
        assert_eq!(read, body);
        drop(runtime);
    }

    #[test]
    fn open_fails_for_http_errors() {
        let (_server, url) = serve(Route::new("gone", "text/plain").status(404));
        let err = stream::open(&url).err().expect("404 must fail");
        assert!(format!("{err:#}").contains("404"), "{err:#}");
    }
}

/// Manual check against a real feed (not run by default):
/// `FURUMI_PODCAST_FEED=/path/to/rss.xml cargo test podcasts::tests::manual_feed -- --ignored --nocapture`
#[test]
#[ignore]
fn manual_feed() {
    let Ok(path) = std::env::var("FURUMI_PODCAST_FEED") else {
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    let feed = parse_feed(&bytes).unwrap();
    let dated = feed
        .episodes
        .iter()
        .filter(|e| e.published_at.is_some())
        .count();
    let timed = feed
        .episodes
        .iter()
        .filter(|e| e.duration_seconds.is_some())
        .count();
    println!(
        "title={:?} author={:?} image={:?} episodes={} dated={} with_duration={}",
        feed.title,
        feed.author,
        feed.image_url,
        feed.episodes.len(),
        dated,
        timed
    );
    for episode in feed.episodes.iter().take(3).chain(feed.episodes.last()) {
        println!(
            "  {:?} | {:?} | {:?} | {}",
            episode.title, episode.published_at, episode.duration_seconds, episode.audio_url
        );
    }
}

/// Manual streaming check against a real episode URL (not run by default):
/// `FURUMI_PODCAST_AUDIO=https://... cargo test podcasts::tests::manual_stream -- --ignored --nocapture`
#[test]
#[ignore]
fn manual_stream() {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(url) = std::env::var("FURUMI_PODCAST_AUDIO") else {
        return;
    };
    let started = std::time::Instant::now();
    let mut opened = stream::open(&url).unwrap();
    println!(
        "len={:?} mime={:?} seekable={} opened in {:?}",
        opened.byte_len,
        opened.mime_type,
        opened.seekable,
        started.elapsed()
    );
    let mut buf = vec![0u8; 256 * 1024];
    opened.reader.read_exact(&mut buf).unwrap();
    println!("first bytes {:02x?}", &buf[..4]);
    if let Some(len) = opened.byte_len {
        let started = std::time::Instant::now();
        opened.reader.seek(SeekFrom::Start(len / 2)).unwrap();
        opened.reader.read_exact(&mut buf).unwrap();
        println!("seek to middle + 256 KiB in {:?}", started.elapsed());
    }
}

#[test]
fn http_stream_is_send_and_sync() {
    fn check<T: Send + Sync + std::io::Read + std::io::Seek>() {}
    check::<HttpStream>();
}
