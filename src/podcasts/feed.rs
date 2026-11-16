//! Tolerant RSS 2.0 / RSS 1.0 / Atom parser for podcast feeds.
//!
//! Real-world feeds are frequently sloppy: undeclared namespace prefixes,
//! HTML entities that XML does not define, mismatched closing tags, legacy
//! single-byte encodings and creative date formats. The parser is a single
//! iterative pass over `quick-xml` events (no recursion), keeps only the
//! fields Furumi needs and never panics on malformed input.

use std::borrow::Cow;
use std::collections::HashSet;

use anyhow::{Result, bail};
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;
use reqwest::Url;

/// Upper bound on episodes taken from a single feed document.
pub const MAX_EPISODES: usize = 20_000;
/// Upper bound (in characters) for plain-text descriptions.
pub const MAX_DESCRIPTION_CHARS: usize = 4000;
/// Upper bound (in characters) for titles and other short fields.
const MAX_SHORT_CHARS: usize = 1000;
/// Raw text captured for one element is capped to keep memory bounded on
/// hostile or broken documents.
const MAX_CAPTURE_BYTES: usize = 512 * 1024;
/// Elements nested deeper than this are skipped (their events are ignored).
const MAX_DEPTH: usize = 256;
const UNTITLED_EPISODE: &str = "Untitled episode";
const UNTITLED_PODCAST: &str = "Untitled podcast";

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFeed {
    pub title: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub link: Option<String>,
    pub image_url: Option<String>,
    pub episodes: Vec<ParsedEpisode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEpisode {
    pub guid: String,
    pub title: String,
    pub description: Option<String>,
    pub published_at: Option<i64>,
    pub duration_seconds: Option<f64>,
    pub audio_url: String,
    pub mime_type: Option<String>,
    pub byte_len: Option<u64>,
    pub link: Option<String>,
    pub image_url: Option<String>,
}

/// Parses an RSS or Atom document. Fails for documents that are not feeds
/// (for example an HTML page) with "not an RSS or Atom feed".
///
/// Without a base URL, relative media URLs cannot be resolved and such
/// episodes are dropped; prefer [`parse_feed_with_base`] when the feed URL is
/// known.
#[cfg(test)]
pub fn parse_feed(bytes: &[u8]) -> Result<ParsedFeed> {
    parse_feed_with_base(bytes, None)
}

/// [`parse_feed`] resolving relative enclosure and image URLs against
/// `base_url` (the final URL the feed was fetched from). Episodes whose audio
/// URL does not resolve to an absolute `http`/`https` URL are dropped, so a
/// feed can never point playback at a local file.
pub fn parse_feed_with_base(bytes: &[u8], base_url: Option<&str>) -> Result<ParsedFeed> {
    let text = decode_document(bytes);
    let parser = Parser {
        base: base_url.and_then(|base| Url::parse(base).ok()),
        ..Parser::default()
    };
    parser.run(&text)
}

// ---------------------------------------------------------------------------
// Character encoding

/// Decodes the raw document to UTF-8. A BOM wins; otherwise an ASCII-compatible
/// encoding named in the XML declaration (windows-1251, koi8-r, ...) is
/// honoured; everything else is decoded as lossy UTF-8.
fn decode_document(bytes: &[u8]) -> Cow<'_, str> {
    if let Some((encoding, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        let (text, _) = encoding.decode_without_bom_handling(&bytes[bom_len..]);
        return text;
    }
    if let Some(encoding) = declared_encoding(bytes).and_then(encoding_rs::Encoding::for_label)
        && encoding != encoding_rs::UTF_8
        && encoding.is_ascii_compatible()
    {
        let (text, _) = encoding.decode_without_bom_handling(bytes);
        return text;
    }
    String::from_utf8_lossy(bytes)
}

/// The `encoding="..."` label of a leading XML declaration, if any.
fn declared_encoding(bytes: &[u8]) -> Option<&[u8]> {
    let head = &bytes[..bytes.len().min(512)];
    let start = head.iter().position(|b| !b.is_ascii_whitespace())?;
    let head = &head[start..];
    if !head.starts_with(b"<?xml") {
        return None;
    }
    let end = find(head, b"?>")?;
    let decl = &head[..end];
    let at = find(decl, b"encoding")? + b"encoding".len();
    let rest = &decl[at..];
    let rest = &rest[rest.iter().position(|b| !b.is_ascii_whitespace())?..];
    let rest = rest.strip_prefix(b"=")?;
    let rest = &rest[rest.iter().position(|b| !b.is_ascii_whitespace())?..];
    let quote = *rest.first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let rest = &rest[1..];
    let len = rest.iter().position(|&b| b == quote)?;
    Some(&rest[..len])
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---------------------------------------------------------------------------
// Namespaces

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ns {
    /// No namespace, or the RSS 0.9x/1.0 namespace.
    Plain,
    Atom,
    Itunes,
    Content,
    Media,
    Dc,
    Other,
}

fn classify_uri(uri: &str) -> Ns {
    let uri = uri.trim().to_ascii_lowercase();
    if uri.is_empty() {
        Ns::Plain
    } else if uri.contains("w3.org/2005/atom") {
        Ns::Atom
    } else if uri.contains("itunes.com/dtds/podcast") {
        Ns::Itunes
    } else if uri.contains("purl.org/rss/1.0/modules/content") {
        Ns::Content
    } else if uri.contains("search.yahoo.com/mrss") {
        Ns::Media
    } else if uri.contains("purl.org/dc/elements") {
        Ns::Dc
    } else if uri.contains("purl.org/rss/1.0")
        || uri.contains("my.netscape.com/rdf")
        || uri.contains("backend.userland.com/rss")
    {
        Ns::Plain
    } else {
        Ns::Other
    }
}

/// Classification of a prefix nobody declared (common in sloppy feeds).
fn classify_prefix(prefix: &str) -> Ns {
    match prefix.to_ascii_lowercase().as_str() {
        "" => Ns::Plain,
        "itunes" => Ns::Itunes,
        "content" => Ns::Content,
        "media" => Ns::Media,
        "dc" => Ns::Dc,
        "atom" | "atom10" => Ns::Atom,
        _ => Ns::Other,
    }
}

// ---------------------------------------------------------------------------
// Parser state

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Rss,
    Atom,
}

#[derive(Debug)]
struct Elem {
    ns: Ns,
    /// ASCII-lowercased local name.
    local: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    FeedTitle,
    FeedDescription,
    FeedItunesSummary,
    FeedLink,
    FeedItunesAuthor,
    FeedAtomAuthor,
    FeedDcCreator,
    FeedManagingEditor,
    FeedImageUrl,
    FeedLogo,
    FeedIcon,
    ItemTitle,
    ItemItunesTitle,
    ItemDescription,
    ItemContent,
    ItemSummary,
    ItemGuid,
    ItemPubDate,
    ItemDcDate,
    ItemUpdated,
    ItemDuration,
    ItemLink,
}

#[derive(Debug)]
struct Capture {
    /// Stack depth of the captured element.
    depth: usize,
    field: Field,
    text: String,
}

#[derive(Debug, Default)]
struct FeedAcc {
    title: Option<String>,
    description: Option<String>,
    itunes_summary: Option<String>,
    link: Option<String>,
    itunes_author: Option<String>,
    atom_author: Option<String>,
    dc_creator: Option<String>,
    managing_editor: Option<String>,
    itunes_image: Option<String>,
    image_url: Option<String>,
    logo: Option<String>,
    icon: Option<String>,
}

#[derive(Debug, Clone)]
struct Media {
    url: String,
    mime: Option<String>,
    len: Option<u64>,
}

#[derive(Debug, Default)]
struct ItemAcc {
    title: Option<String>,
    itunes_title: Option<String>,
    description: Option<String>,
    content: Option<String>,
    summary: Option<String>,
    guid: Option<String>,
    pub_date: Option<String>,
    dc_date: Option<String>,
    updated: Option<String>,
    duration: Option<String>,
    link: Option<String>,
    enclosure: Option<Media>,
    media_content: Option<Media>,
    itunes_image: Option<String>,
    thumbnail: Option<String>,
}

#[derive(Debug, Default)]
struct Parser {
    kind: Option<Kind>,
    stack: Vec<Elem>,
    /// Namespace declarations: (depth of declaring element, prefix, uri).
    ns_decls: Vec<(usize, String, String)>,
    /// Start tags ignored because they exceeded `MAX_DEPTH`.
    skipped_depth: usize,
    capture: Option<Capture>,
    feed: FeedAcc,
    item: Option<ItemAcc>,
    item_depth: usize,
    episodes: Vec<ParsedEpisode>,
    seen_guids: HashSet<String>,
    full: bool,
    /// URL the document was fetched from, for relative media URLs.
    base: Option<Url>,
}

/// Sets `slot` unless it already holds a value (first occurrence wins).
fn set_once(slot: &mut Option<String>, value: String) {
    if slot.is_none() && !value.trim().is_empty() {
        *slot = Some(value);
    }
}

impl Parser {
    fn run(mut self, text: &str) -> Result<ParsedFeed> {
        let mut reader = Reader::from_str(text);
        {
            let config = reader.config_mut();
            config.check_end_names = false;
            config.allow_unmatched_ends = true;
            config.allow_dangling_amp = true;
            config.expand_empty_elements = false;
            config.trim_text(false);
        }
        let mut malformed = None;
        loop {
            match reader.read_event() {
                Ok(Event::Eof) => break,
                Ok(event) => {
                    self.handle(event)?;
                    if self.full {
                        break;
                    }
                }
                Err(err) => {
                    if self.kind.is_none() {
                        bail!("not an RSS or Atom feed (invalid XML: {err})");
                    }
                    malformed = Some(err.to_string());
                    break;
                }
            }
        }
        if self.kind.is_none() {
            bail!("not an RSS or Atom feed");
        }
        if let Some(err) = malformed {
            if self.episodes.is_empty() {
                bail!("malformed feed: {err}");
            }
            tracing::warn!("podcast feed is malformed, keeping what parsed: {err}");
        }
        Ok(self.finish())
    }

    fn handle(&mut self, event: Event<'_>) -> Result<()> {
        match event {
            Event::Start(start) => self.on_start(&start, false)?,
            Event::Empty(start) => self.on_start(&start, true)?,
            Event::End(end) => {
                let name = end.name();
                let local = local_name(name.as_ref());
                self.on_end(&local);
            }
            Event::Text(text) => {
                if let Some(capture) = self.capture.as_mut() {
                    match text.xml10_content() {
                        Ok(value) => push_capped(&mut capture.text, &value),
                        Err(_) => push_capped(&mut capture.text, &String::from_utf8_lossy(&text)),
                    }
                }
            }
            Event::CData(data) => {
                if let Some(capture) = self.capture.as_mut() {
                    match data.decode() {
                        Ok(value) => push_capped(&mut capture.text, &value),
                        Err(_) => push_capped(&mut capture.text, &String::from_utf8_lossy(&data)),
                    }
                }
            }
            Event::GeneralRef(reference) => {
                if let Some(capture) = self.capture.as_mut() {
                    let raw = String::from_utf8_lossy(&reference);
                    match resolve_entity(&raw) {
                        Some(ch) => push_capped(&mut capture.text, ch.encode_utf8(&mut [0; 4])),
                        None => {
                            // Keep unknown entities verbatim instead of failing.
                            push_capped(&mut capture.text, "&");
                            push_capped(&mut capture.text, &raw);
                            push_capped(&mut capture.text, ";");
                        }
                    }
                }
            }
            Event::Comment(_) | Event::Decl(_) | Event::PI(_) | Event::DocType(_) | Event::Eof => {}
        }
        Ok(())
    }

    fn resolve_ns(&self, prefix: &str) -> Ns {
        for (_, declared, uri) in self.ns_decls.iter().rev() {
            if declared == prefix {
                return classify_uri(uri);
            }
        }
        classify_prefix(prefix)
    }

    fn on_start(&mut self, start: &BytesStart<'_>, empty: bool) -> Result<()> {
        if self.stack.len() >= MAX_DEPTH {
            if !empty {
                self.skipped_depth += 1;
            }
            return Ok(());
        }
        let depth = self.stack.len() + 1;
        let attrs = Attrs::collect(start);
        for (key, value) in &attrs.raw {
            if key == "xmlns" {
                self.ns_decls.push((depth, String::new(), value.clone()));
            } else if let Some(prefix) = key.strip_prefix("xmlns:") {
                self.ns_decls
                    .push((depth, prefix.to_string(), value.clone()));
            }
        }
        let qname = start.name();
        let (prefix, local) = split_qname(qname.as_ref());
        let mut ns = self.resolve_ns(&prefix);
        if self.kind == Some(Kind::Rss) && prefix.is_empty() && matches!(ns, Ns::Atom | Ns::Other) {
            // RSS has no namespace of its own; an unprefixed element is a core
            // RSS element whatever default namespace the publisher declared.
            ns = Ns::Plain;
        }
        self.stack.push(Elem { ns, local });

        if self.kind.is_none() {
            let local = self.stack[0].local.as_str();
            self.kind = match local {
                "rss" | "rdf" => Some(Kind::Rss),
                "feed" => Some(Kind::Atom),
                _ => bail!("not an RSS or Atom feed (root element <{local}>)"),
            };
        } else if let Some(capture) = self.capture.as_mut() {
            // Markup nested in a captured field (e.g. Atom xhtml content):
            // keep words apart.
            push_capped(&mut capture.text, " ");
        } else {
            self.dispatch_start(&attrs);
        }

        if empty {
            self.close_top();
        }
        Ok(())
    }

    fn dispatch_start(&mut self, attrs: &Attrs) {
        let Some(kind) = self.kind else { return };
        let depth = self.stack.len();
        let local_at = |back: usize| {
            depth
                .checked_sub(back)
                .and_then(|index| self.stack.get(index))
                .map(|elem| elem.local.clone())
                .unwrap_or_default()
        };
        let ns = self.stack[depth - 1].ns;
        let local = local_at(1);
        let parent = local_at(2);
        let grandparent = local_at(3);
        let core = match kind {
            Kind::Rss => ns == Ns::Plain,
            Kind::Atom => matches!(ns, Ns::Plain | Ns::Atom),
        };

        if self.item.is_some() {
            let direct = depth == self.item_depth + 1;
            if let Some(field) = self.item_field(kind, ns, core, &local, direct, attrs) {
                self.begin_capture(field);
            }
            return;
        }

        let is_item = match kind {
            Kind::Rss => core && local == "item",
            Kind::Atom => core && local == "entry",
        };
        if is_item {
            self.item = Some(ItemAcc::default());
            self.item_depth = depth;
            return;
        }

        let feed_level = match kind {
            Kind::Rss => parent == "channel",
            Kind::Atom => depth == 2,
        };
        let field = if feed_level {
            self.feed_field(kind, ns, core, &local, attrs)
        } else if kind == Kind::Rss
            && core
            && local == "url"
            && parent == "image"
            && matches!(grandparent.as_str(), "channel" | "rdf")
        {
            Some(Field::FeedImageUrl)
        } else if kind == Kind::Atom && core && local == "name" && parent == "author" && depth == 3
        {
            Some(Field::FeedAtomAuthor)
        } else {
            None
        };
        if let Some(field) = field {
            self.begin_capture(field);
        }
    }

    fn feed_field(
        &mut self,
        kind: Kind,
        ns: Ns,
        core: bool,
        local: &str,
        attrs: &Attrs,
    ) -> Option<Field> {
        if core {
            return match (kind, local) {
                (_, "title") => Some(Field::FeedTitle),
                (Kind::Rss, "description") | (Kind::Atom, "subtitle") => {
                    Some(Field::FeedDescription)
                }
                (Kind::Rss, "link") => Some(Field::FeedLink),
                (Kind::Rss, "managingeditor") => Some(Field::FeedManagingEditor),
                (Kind::Atom, "link") => {
                    let rel = attrs.get("rel").unwrap_or_default().to_ascii_lowercase();
                    if rel.is_empty() || rel == "alternate" {
                        let href = attrs.get("href").unwrap_or_default();
                        set_once(&mut self.feed.link, clean_short(&href));
                    }
                    None
                }
                (Kind::Atom, "logo") => Some(Field::FeedLogo),
                (Kind::Atom, "icon") => Some(Field::FeedIcon),
                _ => None,
            };
        }
        match (ns, local) {
            (Ns::Itunes, "author") => Some(Field::FeedItunesAuthor),
            (Ns::Itunes, "summary") => Some(Field::FeedItunesSummary),
            (Ns::Itunes, "image") => {
                if let Some(href) = attrs.get("href") {
                    set_once(&mut self.feed.itunes_image, clean_short(&href));
                }
                None
            }
            (Ns::Dc, "creator") => Some(Field::FeedDcCreator),
            _ => None,
        }
    }

    fn item_field(
        &mut self,
        kind: Kind,
        ns: Ns,
        core: bool,
        local: &str,
        direct: bool,
        attrs: &Attrs,
    ) -> Option<Field> {
        let item = self.item.as_mut()?;
        // Media RSS may wrap content in <media:group>, so accept any depth.
        if ns == Ns::Media {
            match local {
                "content" => {
                    let url = attrs.get("url").unwrap_or_default();
                    let mime = attrs.get("type");
                    let medium = attrs.get("medium");
                    if item.media_content.is_none()
                        && is_playable(&url, mime.as_deref(), medium.as_deref(), false)
                    {
                        item.media_content = Some(Media {
                            url: url.trim().to_string(),
                            mime: clean_mime(mime.as_deref()),
                            len: parse_len(attrs.get("filesize")),
                        });
                    }
                }
                "thumbnail" => {
                    if let Some(url) = attrs.get("url") {
                        set_once(&mut item.thumbnail, clean_short(&url));
                    }
                }
                _ => {}
            }
            return None;
        }
        if !direct {
            return None;
        }
        if core {
            return match (kind, local) {
                (_, "title") => Some(Field::ItemTitle),
                (Kind::Rss, "description") => Some(Field::ItemDescription),
                (Kind::Rss, "guid") | (Kind::Atom, "id") => Some(Field::ItemGuid),
                (Kind::Rss, "pubdate") | (Kind::Atom, "published") => Some(Field::ItemPubDate),
                (Kind::Atom, "updated") => Some(Field::ItemUpdated),
                (Kind::Rss, "link") => Some(Field::ItemLink),
                (Kind::Atom, "summary") => Some(Field::ItemDescription),
                (Kind::Atom, "content") => Some(Field::ItemContent),
                (Kind::Rss, "enclosure") => {
                    let url = attrs.get("url").unwrap_or_default();
                    let mime = attrs.get("type");
                    if item.enclosure.is_none() && is_playable(&url, mime.as_deref(), None, true) {
                        item.enclosure = Some(Media {
                            url: url.trim().to_string(),
                            mime: clean_mime(mime.as_deref()),
                            len: parse_len(attrs.get("length")),
                        });
                    }
                    None
                }
                (Kind::Atom, "link") => {
                    let rel = attrs.get("rel").unwrap_or_default().to_ascii_lowercase();
                    let href = attrs.get("href").unwrap_or_default();
                    let mime = attrs.get("type");
                    if rel == "enclosure" {
                        if item.enclosure.is_none()
                            && is_playable(&href, mime.as_deref(), None, true)
                        {
                            item.enclosure = Some(Media {
                                url: href.trim().to_string(),
                                mime: clean_mime(mime.as_deref()),
                                len: parse_len(attrs.get("length")),
                            });
                        }
                    } else if rel.is_empty() || rel == "alternate" {
                        set_once(&mut item.link, clean_short(&href));
                    }
                    None
                }
                _ => None,
            };
        }
        match (ns, local) {
            (Ns::Itunes, "title") => Some(Field::ItemItunesTitle),
            (Ns::Itunes, "summary") => Some(Field::ItemSummary),
            (Ns::Itunes, "duration") => Some(Field::ItemDuration),
            (Ns::Itunes, "image") => {
                if let Some(href) = attrs.get("href") {
                    set_once(&mut item.itunes_image, clean_short(&href));
                }
                None
            }
            (Ns::Content, "encoded") => Some(Field::ItemContent),
            (Ns::Dc, "date") => Some(Field::ItemDcDate),
            _ => None,
        }
    }

    fn begin_capture(&mut self, field: Field) {
        self.capture = Some(Capture {
            depth: self.stack.len(),
            field,
            text: String::new(),
        });
    }

    fn on_end(&mut self, local: &str) {
        if self.skipped_depth > 0 {
            self.skipped_depth -= 1;
            return;
        }
        // Tolerate mismatched end tags: close up to the nearest open element
        // with the same local name, ignore the tag if none is open.
        let Some(index) = self.stack.iter().rposition(|elem| elem.local == local) else {
            return;
        };
        while self.stack.len() > index {
            self.close_top();
        }
    }

    fn close_top(&mut self) {
        let depth = self.stack.len();
        if depth == 0 {
            return;
        }
        if self
            .capture
            .as_ref()
            .is_some_and(|capture| capture.depth == depth)
            && let Some(capture) = self.capture.take()
        {
            self.store_capture(capture);
        }
        if depth == self.item_depth
            && let Some(item) = self.item.take()
        {
            self.finish_item(item);
        }
        while self
            .ns_decls
            .last()
            .is_some_and(|(decl_depth, _, _)| *decl_depth >= depth)
        {
            self.ns_decls.pop();
        }
        self.stack.pop();
    }

    fn store_capture(&mut self, capture: Capture) {
        let text = capture.text;
        if let Some(item) = self.item.as_mut() {
            let slot = match capture.field {
                Field::ItemTitle => &mut item.title,
                Field::ItemItunesTitle => &mut item.itunes_title,
                Field::ItemDescription => &mut item.description,
                Field::ItemContent => &mut item.content,
                Field::ItemSummary => &mut item.summary,
                Field::ItemGuid => &mut item.guid,
                Field::ItemPubDate => &mut item.pub_date,
                Field::ItemDcDate => &mut item.dc_date,
                Field::ItemUpdated => &mut item.updated,
                Field::ItemDuration => &mut item.duration,
                Field::ItemLink => &mut item.link,
                _ => return,
            };
            set_once(slot, text);
            return;
        }
        let feed = &mut self.feed;
        let slot = match capture.field {
            Field::FeedTitle => &mut feed.title,
            Field::FeedDescription => &mut feed.description,
            Field::FeedItunesSummary => &mut feed.itunes_summary,
            Field::FeedLink => &mut feed.link,
            Field::FeedItunesAuthor => &mut feed.itunes_author,
            Field::FeedAtomAuthor => &mut feed.atom_author,
            Field::FeedDcCreator => &mut feed.dc_creator,
            Field::FeedManagingEditor => &mut feed.managing_editor,
            Field::FeedImageUrl => &mut feed.image_url,
            Field::FeedLogo => &mut feed.logo,
            Field::FeedIcon => &mut feed.icon,
            _ => return,
        };
        set_once(slot, text);
    }

    fn finish_item(&mut self, item: ItemAcc) {
        if self.full {
            return;
        }
        let base = self.base.as_ref();
        let Some(media) = [item.enclosure, item.media_content]
            .into_iter()
            .flatten()
            .find_map(|media| {
                let url = resolve_http_url(&media.url, base)?;
                Some(Media { url, ..media })
            })
        else {
            return;
        };
        let guid = item
            .guid
            .as_deref()
            .map(|guid| guid.trim().to_string())
            .filter(|guid| !guid.is_empty())
            .unwrap_or_else(|| media.url.clone());
        if !self.seen_guids.insert(guid.clone()) {
            return;
        }
        let title = [item.title.as_deref(), item.itunes_title.as_deref()]
            .into_iter()
            .flatten()
            .map(clean_short)
            .find(|title| !title.is_empty())
            .unwrap_or_else(|| UNTITLED_EPISODE.to_string());
        let description = [
            item.description.as_deref(),
            item.content.as_deref(),
            item.summary.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(clean_long)
        .find(|text| !text.is_empty());
        let published_at = [
            item.pub_date.as_deref(),
            item.dc_date.as_deref(),
            item.updated.as_deref(),
        ]
        .into_iter()
        .flatten()
        .find_map(parse_date);
        self.episodes.push(ParsedEpisode {
            guid,
            title,
            description,
            published_at,
            duration_seconds: item.duration.as_deref().and_then(parse_duration),
            audio_url: media.url,
            mime_type: media.mime,
            byte_len: media.len,
            link: item
                .link
                .as_deref()
                .map(clean_short)
                .filter(|s| !s.is_empty()),
            image_url: [item.itunes_image, item.thumbnail]
                .into_iter()
                .flatten()
                .find_map(|url| resolve_http_url(&url, base)),
        });
        if self.episodes.len() >= MAX_EPISODES {
            self.full = true;
        }
    }

    fn finish(self) -> ParsedFeed {
        let feed = self.feed;
        let title = feed
            .title
            .as_deref()
            .map(clean_short)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| UNTITLED_PODCAST.to_string());
        let author = [
            feed.itunes_author.as_deref(),
            feed.atom_author.as_deref(),
            feed.dc_creator.as_deref(),
            feed.managing_editor.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(clean_short)
        .find(|author| !author.is_empty());
        let description = [feed.description.as_deref(), feed.itunes_summary.as_deref()]
            .into_iter()
            .flatten()
            .map(clean_long)
            .find(|text| !text.is_empty());
        let image_url = [feed.itunes_image, feed.image_url, feed.logo, feed.icon]
            .into_iter()
            .flatten()
            .find_map(|url| resolve_http_url(&url, self.base.as_ref()));
        ParsedFeed {
            title,
            author,
            description,
            link: feed
                .link
                .as_deref()
                .map(clean_short)
                .filter(|s| !s.is_empty()),
            image_url,
            episodes: self.episodes,
        }
    }
}

// ---------------------------------------------------------------------------
// Element and attribute helpers

fn split_qname(name: &[u8]) -> (String, String) {
    let name = String::from_utf8_lossy(name);
    match name.split_once(':') {
        Some((prefix, local)) => (prefix.to_string(), local.to_ascii_lowercase()),
        None => (String::new(), name.to_ascii_lowercase()),
    }
}

fn local_name(name: &[u8]) -> String {
    split_qname(name).1
}

/// Attributes of one element: raw keys with entity-decoded values.
struct Attrs {
    raw: Vec<(String, String)>,
}

impl Attrs {
    fn collect(start: &BytesStart<'_>) -> Attrs {
        let mut attributes = start.attributes();
        attributes.with_checks(false);
        let raw = attributes
            .flatten()
            .map(|attr| {
                let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
                let value = decode_entities(&String::from_utf8_lossy(&attr.value));
                (key, value)
            })
            .collect();
        Attrs { raw }
    }

    /// Value of the attribute whose local name matches `name` (any prefix).
    fn get(&self, name: &str) -> Option<String> {
        self.raw
            .iter()
            .find(|(key, _)| {
                let local = key.rsplit_once(':').map_or(key.as_str(), |(_, l)| l);
                local.eq_ignore_ascii_case(name) && !key.starts_with("xmlns")
            })
            .map(|(_, value)| value.clone())
    }
}

fn push_capped(text: &mut String, value: &str) {
    if text.len() >= MAX_CAPTURE_BYTES {
        return;
    }
    let room = MAX_CAPTURE_BYTES - text.len();
    if value.len() <= room {
        text.push_str(value);
    } else {
        let mut end = room;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        text.push_str(&value[..end]);
    }
}

fn parse_len(value: Option<String>) -> Option<u64> {
    value?.trim().parse::<u64>().ok().filter(|len| *len > 0)
}

fn clean_mime(mime: Option<&str>) -> Option<String> {
    let mime = mime?.split(';').next()?.trim().to_ascii_lowercase();
    (!mime.is_empty()).then_some(mime)
}

const MEDIA_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "m4b", "aac", "ogg", "oga", "opus", "flac", "wav", "mp4", "m4v", "mov", "webm",
    "mka",
];

fn has_media_extension(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let file = path.rsplit('/').next().unwrap_or_default();
    file.rsplit_once('.').is_some_and(|(_, ext)| {
        MEDIA_EXTENSIONS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(ext))
    })
}

/// Whether an enclosure/media element points at something playable.
/// `lenient_untyped`: accept a URL without type information (RSS
/// enclosures); media:content without type must at least look like audio.
fn is_playable(url: &str, mime: Option<&str>, medium: Option<&str>, lenient_untyped: bool) -> bool {
    let url = url.trim();
    if url.is_empty() {
        return false;
    }
    if let Some(mime) = clean_mime(mime) {
        return mime.starts_with("audio/")
            || mime.starts_with("video/")
            || matches!(
                mime.as_str(),
                "application/octet-stream"
                    | "binary/octet-stream"
                    | "application/ogg"
                    | "application/mp4"
                    | "application/x-mpegurl"
                    | "application/vnd.apple.mpegurl"
            )
            || (mime == "application/x-unknown" && has_media_extension(url));
    }
    if let Some(medium) = medium.map(|m| m.trim().to_ascii_lowercase())
        && !medium.is_empty()
    {
        return medium == "audio" || medium == "video";
    }
    lenient_untyped || has_media_extension(url)
}

/// An absolute `http`/`https` URL for a media or image reference: absolute
/// http(s) URLs are kept as written, relative ones (including
/// protocol-relative `//host/...`) are resolved against `base`, and anything
/// else (`file:`, `ftp:`, unresolvable relative paths) yields `None`.
fn resolve_http_url(raw: &str, base: Option<&Url>) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let has_host = |url: &Url| url.host_str().is_some_and(|host| !host.is_empty());
    if super::is_remote_url(raw) {
        return Url::parse(raw)
            .ok()
            .filter(has_host)
            .map(|_| raw.to_string());
    }
    let resolved = match base {
        Some(base) => base.join(raw).ok()?,
        None if raw.starts_with("//") => Url::parse(&format!("https:{raw}")).ok()?,
        None => return None,
    };
    (matches!(resolved.scheme(), "http" | "https") && has_host(&resolved))
        .then(|| resolved.to_string())
}

// ---------------------------------------------------------------------------
// Text cleanup

fn clean_short(text: &str) -> String {
    html_to_text(text, MAX_SHORT_CHARS)
}

fn clean_long(text: &str) -> String {
    html_to_text(text, MAX_DESCRIPTION_CHARS)
}

const BLOCK_TAGS: &[&str] = &[
    "br",
    "p",
    "div",
    "li",
    "ul",
    "ol",
    "tr",
    "td",
    "th",
    "table",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "blockquote",
    "hr",
    "pre",
    "section",
    "article",
    "header",
    "footer",
    "dd",
    "dt",
];

/// Converts (possibly HTML) text to plain text: strips tags (dropping
/// `<script>`/`<style>` bodies), decodes entities, collapses whitespace and
/// truncates to `max_chars` characters.
pub(super) fn html_to_text(input: &str, max_chars: usize) -> String {
    let mut stripped = String::with_capacity(input.len().min(64 * 1024));
    let mut rest = input;
    while let Some(lt) = rest.find('<') {
        stripped.push_str(&rest[..lt]);
        let after = &rest[lt + 1..];
        let looks_like_tag = after
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?'));
        let Some(gt) = after.find('>').filter(|_| looks_like_tag) else {
            stripped.push('<');
            rest = after;
            continue;
        };
        let tag = &after[..gt];
        rest = &after[gt + 1..];
        if let Some(comment) = tag.strip_prefix("!--")
            && !comment.ends_with("--")
        {
            // Comment containing '>': skip to the real terminator.
            rest = rest.find("-->").map_or("", |end| &rest[end + 3..]);
            continue;
        }
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if !tag.starts_with('/') && (name == "script" || name == "style") {
            let close = format!("</{name}");
            rest = match find_ascii_ci(rest, &close) {
                Some(end) => rest[end..].find('>').map_or("", |gt| &rest[end + gt + 1..]),
                None => "",
            };
            stripped.push(' ');
            continue;
        }
        if BLOCK_TAGS.contains(&name.as_str()) {
            stripped.push(' ');
        }
    }
    stripped.push_str(rest);
    let decoded = decode_entities(&stripped);
    collapse_and_truncate(&decoded, max_chars)
}

fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let hay = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

fn collapse_and_truncate(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_chars * 4));
    let mut chars = 0usize;
    let mut truncated = false;
    for word in text.split(|c: char| c.is_whitespace() || c.is_control()) {
        if word.is_empty() {
            continue;
        }
        let word = word.trim_matches(|c: char| c == '\u{ad}' || c == '\u{200b}');
        if word.is_empty() {
            continue;
        }
        let needed = word.chars().count() + usize::from(!out.is_empty());
        if chars + needed > max_chars {
            truncated = true;
            if out.is_empty() {
                out.extend(word.chars().take(max_chars.saturating_sub(1)));
            }
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        chars += needed;
    }
    if truncated {
        while out.chars().count() + 1 > max_chars {
            out.pop();
        }
        out.push('…');
    }
    out
}

/// Decodes XML/HTML entities and numeric character references; unknown or
/// malformed references are kept verbatim.
pub(super) fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let end = after
            .char_indices()
            .take(32)
            .find(|(_, c)| *c == ';' || *c == '&' || c.is_whitespace() || *c == '<')
            .filter(|(_, c)| *c == ';');
        match end.and_then(|(end, _)| resolve_entity(&after[..end]).map(|ch| (end, ch))) {
            Some((end, ch)) => {
                out.push(ch);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Resolves an entity name (without `&`/`;`) to a character.
fn resolve_entity(name: &str) -> Option<char> {
    if let Some(number) = name.strip_prefix('#') {
        let code = if let Some(hex) = number.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            number.parse::<u32>().ok()?
        };
        return char::from_u32(code).filter(|ch| *ch != '\0');
    }
    let ch = match name {
        "amp" | "AMP" => '&',
        "lt" | "LT" => '<',
        "gt" | "GT" => '>',
        "quot" | "QUOT" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "ensp" => '\u{2002}',
        "emsp" => '\u{2003}',
        "thinsp" => '\u{2009}',
        "shy" => '\u{ad}',
        "zwnj" => '\u{200c}',
        "zwj" => '\u{200d}',
        "laquo" => '«',
        "raquo" => '»',
        "lsaquo" => '‹',
        "rsaquo" => '›',
        "ldquo" => '“',
        "rdquo" => '”',
        "bdquo" => '„',
        "lsquo" => '‘',
        "rsquo" => '’',
        "sbquo" => '‚',
        "mdash" => '—',
        "ndash" => '–',
        "minus" => '−',
        "hellip" => '…',
        "bull" => '•',
        "middot" => '·',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "sect" => '§',
        "para" => '¶',
        "times" => '×',
        "divide" => '÷',
        "plusmn" => '±',
        "frac12" => '½',
        "frac14" => '¼',
        "frac34" => '¾',
        "iexcl" => '¡',
        "iquest" => '¿',
        "numero" => '№',
        "larr" => '←',
        "rarr" => '→',
        "uarr" => '↑',
        "darr" => '↓',
        "hearts" => '♥',
        "star" => '☆',
        "acute" => '´',
        "aacute" => 'á',
        "Aacute" => 'Á',
        "agrave" => 'à',
        "Agrave" => 'À',
        "acirc" => 'â',
        "Acirc" => 'Â',
        "auml" => 'ä',
        "Auml" => 'Ä',
        "atilde" => 'ã',
        "aring" => 'å',
        "aelig" => 'æ',
        "ccedil" => 'ç',
        "Ccedil" => 'Ç',
        "eacute" => 'é',
        "Eacute" => 'É',
        "egrave" => 'è',
        "Egrave" => 'È',
        "ecirc" => 'ê',
        "euml" => 'ë',
        "iacute" => 'í',
        "igrave" => 'ì',
        "icirc" => 'î',
        "iuml" => 'ï',
        "ntilde" => 'ñ',
        "Ntilde" => 'Ñ',
        "oacute" => 'ó',
        "Oacute" => 'Ó',
        "ograve" => 'ò',
        "ocirc" => 'ô',
        "ouml" => 'ö',
        "Ouml" => 'Ö',
        "otilde" => 'õ',
        "oslash" => 'ø',
        "uacute" => 'ú',
        "ugrave" => 'ù',
        "ucirc" => 'û',
        "uuml" => 'ü',
        "Uuml" => 'Ü',
        "yacute" => 'ý',
        "szlig" => 'ß',
        _ => return None,
    };
    Some(ch)
}

// ---------------------------------------------------------------------------
// Durations

/// Parses `HH:MM:SS`, `MM:SS`, `SSSS` and fractional seconds. Invalid or zero
/// durations yield `None`.
pub(super) fn parse_duration(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let parts: Vec<&str> = text.split(':').map(str::trim).collect();
    if parts.len() > 3 {
        return None;
    }
    let mut total = 0.0f64;
    let last = parts.len() - 1;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            return None;
        }
        let value = if index == last {
            if !part.chars().all(|c| c.is_ascii_digit() || c == '.') || part.starts_with('.') {
                return None;
            }
            part.parse::<f64>().ok()?
        } else {
            if !part.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            part.parse::<u64>().ok()? as f64
        };
        total = total * 60.0 + value;
    }
    (total.is_finite() && total > 0.0).then_some(total)
}

// ---------------------------------------------------------------------------
// Dates

/// Parses RFC 2822 (leniently) or RFC 3339 dates into unix seconds.
pub(super) fn parse_date(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let bytes = text.as_bytes();
    if bytes.len() >= 10 && bytes[..4].iter().all(u8::is_ascii_digit) && bytes[4] == b'-' {
        return parse_rfc3339(text);
    }
    parse_rfc2822(text)
}

fn parse_rfc3339(text: &str) -> Option<i64> {
    let year: i64 = text.get(0..4)?.parse().ok()?;
    if text.get(4..5)? != "-" || text.get(7..8)? != "-" {
        return None;
    }
    let month: u32 = digits(text.get(5..7)?)?;
    let day: u32 = digits(text.get(8..10)?)?;
    let rest = &text[10..];
    let (mut hour, mut minute, mut second, mut offset) = (0, 0, 0, 0i64);
    if !rest.is_empty() {
        let rest = rest.strip_prefix(['T', 't', ' '])?.trim_start();
        let zone_at = rest
            .char_indices()
            .find(|(_, c)| matches!(c, 'Z' | 'z' | '+' | '-'))
            .map_or(rest.len(), |(at, _)| at);
        let (time, zone) = rest.split_at(zone_at);
        (hour, minute, second) = parse_hms(time.trim())?;
        let zone = zone.trim();
        if !zone.is_empty() {
            offset = if zone.eq_ignore_ascii_case("z") {
                0
            } else {
                parse_numeric_zone(zone)?
            };
        }
    }
    to_unix(year, month, day, hour, minute, second, offset)
}

fn parse_rfc2822(text: &str) -> Option<i64> {
    let cleaned = text.replace(',', " ");
    let mut day: Option<u32> = None;
    let mut month: Option<u32> = None;
    let mut year: Option<i64> = None;
    let mut time: Option<(u32, u32, u32)> = None;
    let mut offset = 0i64;
    let mut in_comment = false;
    for token in cleaned.split_whitespace() {
        if in_comment || token.starts_with('(') {
            in_comment = !token.ends_with(')');
            continue;
        }
        if month.is_none()
            && let Some(value) = month_from_name(token)
        {
            month = Some(value);
            continue;
        }
        if time.is_none() && token.contains(':') {
            let zone_at = token
                .char_indices()
                .skip(1)
                .find(|(_, c)| matches!(c, '+' | '-') || c.is_ascii_alphabetic())
                .map_or(token.len(), |(at, _)| at);
            let (clock, zone) = token.split_at(zone_at);
            time = Some(parse_hms(clock)?);
            if !zone.is_empty() {
                offset = parse_zone(zone)?;
            }
            continue;
        }
        if token.starts_with(['+', '-']) && token.len() > 1 {
            offset = parse_numeric_zone(token)?;
            continue;
        }
        if token.chars().all(|c| c.is_ascii_digit()) {
            if day.is_none() && token.len() <= 2 {
                day = Some(token.parse().ok()?);
            } else if year.is_none() {
                let value: i64 = token.parse().ok()?;
                year = Some(match token.len() {
                    1 | 2 if value < 50 => 2000 + value,
                    1 | 2 => 1900 + value,
                    3 => 1900 + value,
                    _ => value,
                });
            } else {
                return None;
            }
            continue;
        }
        if let Some(zone) = named_zone(token) {
            offset = zone;
        }
        // Weekday names and unknown words are ignored.
    }
    let (hour, minute, second) = time.unwrap_or((0, 0, 0));
    to_unix(year?, month?, day?, hour, minute, second, offset)
}

fn digits<T: std::str::FromStr>(text: &str) -> Option<T> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn parse_hms(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split(':');
    let hour: u32 = digits(parts.next()?)?;
    let minute: u32 = digits(parts.next()?)?;
    let second: u32 = match parts.next() {
        Some(seconds) => {
            let whole = seconds.split(['.', ',']).next()?;
            digits(whole)?
        }
        None => 0,
    };
    if parts.next().is_some() || hour > 24 || minute > 59 || second > 60 {
        return None;
    }
    if hour == 24 && (minute > 0 || second > 0) {
        return None;
    }
    Some((hour, minute, second.min(59)))
}

fn parse_numeric_zone(text: &str) -> Option<i64> {
    let sign = match text.chars().next()? {
        '+' => 1,
        '-' => -1,
        _ => return None,
    };
    let body: String = text[1..].chars().filter(|c| *c != ':').collect();
    // Byte slicing below requires ASCII; anything else is not a zone.
    if !body.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (hours, minutes): (i64, i64) = match body.len() {
        1 | 2 => (digits(&body)?, 0),
        3 => (digits(&body[..1])?, digits(&body[1..])?),
        4 => (digits(&body[..2])?, digits(&body[2..])?),
        _ => return None,
    };
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

fn parse_zone(text: &str) -> Option<i64> {
    if text.starts_with(['+', '-']) {
        parse_numeric_zone(text)
    } else {
        Some(named_zone(text).unwrap_or(0))
    }
}

fn named_zone(text: &str) -> Option<i64> {
    let hours = match text.to_ascii_uppercase().as_str() {
        "GMT" | "UT" | "UTC" | "Z" | "WET" => 0,
        "EST" => -5,
        "EDT" => -4,
        "CST" => -6,
        "CDT" => -5,
        "MST" => -7,
        "MDT" => -6,
        "PST" => -8,
        "PDT" => -7,
        "AKST" => -9,
        "AKDT" => -8,
        "HST" => -10,
        "BST" | "CET" | "WEST" => 1,
        "CEST" | "EET" => 2,
        "EEST" | "MSK" => 3,
        "JST" => 9,
        "AEST" => 10,
        "AEDT" => 11,
        _ => return None,
    };
    Some(hours * 3600)
}

fn month_from_name(token: &str) -> Option<u32> {
    let lower = token.to_lowercase();
    if !lower.chars().all(char::is_alphabetic) {
        return None;
    }
    let prefix: String = lower.chars().take(3).collect();
    if prefix.chars().count() < 3 {
        return None;
    }
    let month = match prefix.as_str() {
        "jan" | "янв" => 1,
        "feb" | "фев" => 2,
        "mar" | "мар" => 3,
        "apr" | "апр" => 4,
        "may" | "мая" | "май" => 5,
        "jun" | "июн" => 6,
        "jul" | "июл" => 7,
        "aug" | "авг" => 8,
        "sep" | "сен" => 9,
        "oct" | "окт" => 10,
        "nov" | "ноя" => 11,
        "dec" | "дек" => 12,
        _ => return None,
    };
    Some(month)
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn to_unix(
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    offset: i64,
) -> Option<i64> {
    if !(1900..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(
        days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second)
            - offset,
    )
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// UTC calendar year of a unix timestamp.
pub(super) fn year_from_unix(timestamp: i64) -> i32 {
    let days = timestamp.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    i32::try_from(year).unwrap_or(if year < 0 { i32::MIN } else { i32::MAX })
}
