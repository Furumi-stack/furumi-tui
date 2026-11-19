mod additional_settings;
pub mod art;
mod federation;
pub(crate) mod global;
mod logs;
mod playlists;
mod podcast_card;
mod podcasts;
mod popup;
#[cfg(test)]
mod status_tests;
pub mod theme;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Tabs};

use crate::app::input::LineEdit;
use crate::app::state::{AppState, GlobalView, Loadable, Tab, TrackSelectionScope};
use crate::config::keymap::Keymap;
use crate::library::models::Availability;

pub(crate) fn availability_marker(
    availability: Availability,
    selected_style: Option<Style>,
) -> (&'static str, Style) {
    let (label, style) = match availability {
        Availability::Local => ("●", Style::new().fg(Color::Green)),
        Availability::Mixed => ("◐", Style::new().fg(Color::Yellow)),
        Availability::Remote => ("⇅", theme::accent()),
    };
    (label, selected_style.unwrap_or(style))
}

pub(crate) fn availability_prefix(availability: Availability) -> Span<'static> {
    let (label, style) = availability_marker(availability, None);
    Span::styled(format!("{label} "), style)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_availability_marker_uses_the_supplied_mode_style() {
        let client_style = Style::new().fg(Color::Black).bg(Color::Yellow);

        let (label, style) = availability_marker(Availability::Local, Some(client_style));

        assert_eq!(label, "●");
        assert_eq!(style, client_style);
    }

    fn render(state: &AppState, width: u16, height: u16) -> String {
        let keymap = Keymap::defaults();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, state, &keymap)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tab_bar_numbers_tabs_in_the_new_order() {
        let screen = render(&AppState::default(), 120, 10);
        let bar = screen.lines().next().unwrap();
        let positions: Vec<usize> = [
            "1 Library",
            "2 Playlists",
            "3 Queue",
            "4 Podcasts",
            "5 Settings",
            "6 Logs",
        ]
        .iter()
        .map(|title| {
            bar.find(title)
                .unwrap_or_else(|| panic!("{title} in {bar:?}"))
        })
        .collect();
        assert!(positions.is_sorted(), "{bar:?}");
    }

    /// Each tab's number appears once, in order, and the active tab's label
    /// is fully on screen.
    fn assert_tab_bar_fits(width: u16, active: Tab, active_label: &str) {
        let mut state = AppState::default();
        state.active_tab = active;
        let screen = render(&state, width, 10);
        let bar = screen.lines().next().unwrap();
        let digits: String = bar.chars().filter(char::is_ascii_digit).collect();
        assert_eq!(digits, "123456", "width {width}: {bar:?}");
        assert!(
            bar.contains(active_label),
            "width {width}: {active_label} missing in {bar:?}"
        );
    }

    #[test]
    fn tab_bar_keeps_every_tab_visible_on_narrow_terminals() {
        for width in [80, 77, 76, 70, 65, 60, 48, 40, 30] {
            assert_tab_bar_fits(width, Tab::Logs, "6 Logs");
            assert_tab_bar_fits(width, Tab::Global, "1 Lib");
        }
        // Full titles while they fit.
        assert_tab_bar_fits(80, Tab::Global, "5 Settings");
        assert_tab_bar_fits(65, Tab::Global, "5 Settings");
        // Numbers only, but the active tab keeps its full title.
        assert_tab_bar_fits(30, Tab::Federation, "5 Settings");
        assert_tab_bar_fits(30, Tab::Playlists, "2 Playlists");
    }

    #[test]
    fn tab_bar_highlights_active_tab_when_compact() {
        let mut state = AppState::default();
        state.active_tab = Tab::Logs;
        let keymap = Keymap::defaults();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 10)).unwrap();
        terminal.draw(|frame| draw(frame, &state, &keymap)).unwrap();
        let buffer = terminal.backend().buffer();
        let bar: String = (0..60).map(|x| buffer[(x, 0)].symbol()).collect();
        let x = bar.find("6 Logs").expect("active tab on screen") as u16;
        let highlight = theme::tab_active_for(&state);
        let cell = buffer[(x, 0)].style();
        assert_eq!((cell.fg, cell.bg), (highlight.fg, highlight.bg));
    }

    use crate::app::podcast_fixtures;

    #[test]
    fn podcasts_tab_reports_opening_and_failed_store() {
        let mut state = AppState::default();
        state.active_tab = Tab::Podcasts;
        let screen = render(&state, 100, 12);
        assert!(screen.contains("opening the podcast database"), "{screen}");

        state.podcasts.store = crate::app::state::PodcastStoreStatus::Failed(
            "podcast database schema v9 is newer than this Furumi supports".into(),
        );
        for width in [100, 50] {
            let screen = render(&state, width, 16);
            let words: Vec<&str> = screen
                .split(|c: char| c.is_whitespace() || c == '│')
                .filter(|word| !word.is_empty())
                .collect();
            for word in ["unavailable", "v9", "newer", "unchanged."] {
                assert!(words.contains(&word), "{word} at {width}:\n{screen}");
            }
        }
    }

    #[test]
    fn podcasts_list_shows_actions_counts_and_errors() {
        let state = podcast_fixtures::ready_state(Vec::new());
        let screen = render(&state, 100, 14);
        assert!(screen.contains("+ Add podcast (RSS feed URL)"), "{screen}");
        assert!(screen.contains("Check all podcasts"), "{screen}");
        assert!(screen.contains("No subscriptions yet"), "{screen}");

        let mut row = podcast_fixtures::row("Test Show", 2, 3, 0);
        row.fetch.last_error = Some("HTTP 503 Service Unavailable".into());
        row.fetch.consecutive_failures = 2;
        let mut state = podcast_fixtures::ready_state(vec![row]);
        state.podcasts.selected = crate::app::state::PODCAST_ACTION_ROWS;
        // Wide: side panel with details; narrow: status below the list.
        for width in [110, 60] {
            let screen = render(&state, width, 30);
            assert!(screen.contains("! Test Show"), "{screen}");
            assert!(screen.contains("2 unplayed · 3 ep"), "{screen}");
            assert!(screen.contains("last check failed"), "{screen}");
            assert!(screen.contains("503"), "{screen}");
        }
    }

    #[test]
    fn podcast_share_state_is_shown_in_the_list_and_details() {
        use crate::podcasts::publication::{Discovery, Publication};
        let mut state =
            podcast_fixtures::ready_state(vec![podcast_fixtures::row("Test Show", 0, 3, 0)]);
        state.podcasts.selected = crate::app::state::PODCAST_ACTION_ROWS;
        let screen = render(&state, 110, 30);
        assert!(screen.contains("private (not shared"), "{screen}");
        assert!(!screen.contains("shared · 3 ep"), "{screen}");

        let mut row = podcast_fixtures::row("Test Show", 0, 3, 0);
        row.summary.shared = true;
        let mut state = podcast_fixtures::ready_state(vec![row]);
        state.podcasts.selected = crate::app::state::PODCAST_ACTION_ROWS;
        state.podcasts.publication.discovery = Discovery::Running { peers: 2 };
        state
            .podcasts
            .publication
            .podcasts
            .insert(podcast_fixtures::podcast_id(), Publication::Published);
        let screen = render(&state, 110, 30);
        assert!(screen.contains("shared · 3 ep"), "{screen}");
        assert!(screen.contains("shared · published from"), "{screen}");

        state.podcasts.publication.podcasts.insert(
            podcast_fixtures::podcast_id(),
            Publication::HeldBack(Vec::new()),
        );
        let screen = render(&state, 110, 30);
        assert!(screen.contains("held back"), "{screen}");

        state.podcasts.publication.discovery = Discovery::Off("federation is off".into());
        let screen = render(&state, 110, 30);
        assert!(screen.contains("not published:"), "{screen}");

        // Sharing turned off here: until a pass revoked the card, the panel
        // does not claim the podcast is simply private.
        state.podcasts.publication.discovery = Discovery::Running { peers: 2 };
        state
            .podcasts
            .publication
            .podcasts
            .insert(podcast_fixtures::podcast_id(), Publication::Published);
        state
            .podcasts
            .publication
            .note_revocation(&podcast_fixtures::podcast_id(), true);
        let screen = render(&state, 110, 30);
        assert!(screen.contains("private · revoking the"), "{screen}");
        if let Some(crate::app::state::Loadable::Ready(rows)) = &mut state.podcasts.list {
            rows[0].summary.shared = false;
        }
        let screen = render(&state, 110, 30);
        assert!(screen.contains("private · revoking the"), "{screen}");
        assert!(!screen.contains("private (not shared"), "{screen}");
        state.podcasts.publication.discovery = Discovery::Off("federation is off".into());
        let screen = render(&state, 110, 30);
        assert!(screen.contains("may not be revoked yet:"), "{screen}");
        assert!(screen.contains("federation is off"), "{screen}");
        state.podcasts.publication.unrevoked.clear();
        let screen = render(&state, 110, 30);
        assert!(screen.contains("private (not shared"), "{screen}");
    }

    #[test]
    fn the_podcast_card_scrolls_exactly_to_its_last_line() {
        use crate::app::state::Popup;
        use crate::app::update::podcast_discovery::card_key;
        use crossterm::event::KeyCode;
        let mut state = search_with_podcasts();
        let mut hit = state.search.fed_podcasts[1].clone();
        // Long words word-wrap into far more rows than their character
        // count suggests (one 50-character word per 90-column row).
        let word = "x".repeat(50);
        hit.hit.card.description = Some(vec![word.as_str(); 30].join(" "));
        state.popup = Some(Popup::PodcastCard {
            hit: Box::new(hit),
            preview: None,
            scroll: 0,
            max_scroll: std::cell::Cell::new(u16::MAX),
        });
        let last_line = "e — прочитать ленту";
        let screen = render(&state, 110, 30);
        assert!(!screen.contains(last_line), "below the fold:\n{screen}");

        // Paging far past the end stops at the last line ...
        for _ in 0..20 {
            let popup = state.popup.take().unwrap();
            assert_eq!(card_key(&mut state, popup, KeyCode::PageDown), None);
        }
        let screen = render(&state, 110, 30);
        assert!(screen.contains(last_line), "{screen}");
        let Some(Popup::PodcastCard {
            scroll, max_scroll, ..
        }) = &state.popup
        else {
            panic!("the card is open");
        };
        assert_eq!(*scroll, max_scroll.get());
        let end = *scroll;
        // ... and one step back moves the view at once.
        let popup = state.popup.take().unwrap();
        card_key(&mut state, popup, KeyCode::Up);
        assert!(matches!(
            &state.popup,
            Some(Popup::PodcastCard { scroll, .. }) if *scroll == end - 1
        ));
    }

    /// Search view with a federated artist and two public podcast cards;
    /// the first podcast is subscribed on this device.
    fn search_with_podcasts() -> AppState {
        use crate::podcasts::discovery::{PodcastHit, test_hit};
        let mut state =
            podcast_fixtures::ready_state(vec![podcast_fixtures::row("Test Show", 0, 3, 0)]);
        state.active_tab = Tab::Global;
        state.search.query = "show".into();
        state.search.results = Some(Default::default());
        state.search.fed_artists = vec![crate::federation::FedArtistHit {
            name: "Show Band".into(),
            peers: 2,
        }];
        let mut other = test_hit("https://feeds.example.test/other.xml", "Other Show");
        other.card.author = None;
        state.search.fed_podcasts = vec![
            PodcastHit::new(test_hit("https://example.test/feed.xml", "Test Show")),
            PodcastHit::new(other),
        ];
        state.global.stack.push(GlobalView::Search { cursor: 1 });
        state
    }

    #[test]
    fn search_marks_podcast_hits_apart_from_music() {
        let screen = render(&search_with_podcasts(), 120, 20);
        assert!(screen.contains("Federation"), "{screen}");
        assert!(
            screen.contains("Show Band  artist · open the card"),
            "{screen}"
        );
        assert!(screen.contains("Podcasts · public cards"), "{screen}");
        assert!(
            screen.contains("[podcast] Test Show  Ferris Crab · open the card"),
            "{screen}"
        );
        assert!(
            screen.contains("[podcast] Other Show  open the card"),
            "{screen}"
        );
        assert!(screen.contains("✓ subscribed"), "{screen}");
        assert!(screen.contains("12 ep · 1 publisher"), "{screen}");
        // The podcasts come after the music rows.
        let artist = screen.find("Show Band").unwrap();
        let podcast = screen.find("[podcast]").unwrap();
        assert!(artist < podcast, "{screen}");
    }

    #[test]
    fn podcast_card_offers_subscribe_or_shows_already_subscribed() {
        use crate::app::state::{Loadable, Popup};
        use crate::podcasts::discovery::{FeedPreview, PreviewEpisode};
        let mut state = search_with_podcasts();
        let mut hit = state.search.fed_podcasts[1].clone();
        hit.hit.claimed_feed_urls = vec![music_dht::podcast_dht::ClaimedFeedUrl {
            feed_url: music_dht::podcast::FeedUrl::parse("https://elsewhere.test/claimed.xml")
                .unwrap(),
            publishers: hit.hit.publishers.clone(),
        }];
        state.popup = Some(Popup::PodcastCard {
            hit: Box::new(hit.clone()),
            preview: None,
            scroll: 0,
            max_scroll: std::cell::Cell::new(u16::MAX),
        });
        let screen = render(&state, 110, 40);
        assert!(screen.contains("[podcast] Other Show"), "{screen}");
        assert!(screen.contains("они не проверены"), "{screen}");
        assert!(screen.contains("Категории: Technology"), "{screen}");
        assert!(
            screen.contains("Выпусков: 12 · последний 2024-06-15"),
            "{screen}"
        );
        assert!(
            screen.contains("Опубликовали: 1 устройство (не число подписчиков"),
            "{screen}"
        );
        assert!(
            screen.contains("Лента: https://feeds.example.test/other.xml"),
            "{screen}"
        );
        assert!(
            screen.contains("(не проверен, не используется): https://elsewhere.test/claimed.xml"),
            "{screen}"
        );
        assert!(screen.contains("[ Подписаться ]"), "{screen}");
        assert!(
            screen.contains("e — прочитать ленту без подписки"),
            "{screen}"
        );
        assert!(screen.contains("enter подписаться"), "{screen}");
        assert!(!screen.contains("Уже подписан"), "{screen}");

        state.popup = Some(Popup::PodcastCard {
            hit: Box::new(hit),
            preview: Some(Loadable::Ready(FeedPreview {
                title: "Other Show (feed)".into(),
                episode_count: 3,
                seasons: vec![(Some(1), 2), (None, 1)],
                episodes: vec![PreviewEpisode {
                    title: "Pilot".into(),
                    season: Some(1),
                    episode_number: Some(1),
                    published_at_ms: Some(1_718_445_600_000),
                    duration_ms: Some(2_700_000),
                }],
                moved_to: None,
                subscribed_as: None,
            })),
            scroll: 0,
            max_scroll: std::cell::Cell::new(u16::MAX),
        });
        let screen = render(&state, 110, 40);
        assert!(
            screen.contains("Название в ленте: Other Show (feed)"),
            "{screen}"
        );
        assert!(
            screen.contains("В ленте: 3 выпуска · сезон 1: 2, без сезона: 1"),
            "{screen}"
        );
        assert!(
            screen.contains("• Pilot  S1E1 · 2024-06-15 · 45:00"),
            "{screen}"
        );
        assert!(screen.contains("… и ещё 2"), "{screen}");

        // The subscribed podcast: «Уже подписан», Enter opens it.
        state.popup = Some(Popup::PodcastCard {
            hit: Box::new(state.search.fed_podcasts[0].clone()),
            preview: None,
            scroll: 0,
            max_scroll: std::cell::Cell::new(u16::MAX),
        });
        let screen = render(&state, 110, 40);
        assert!(screen.contains("✓ Уже подписан"), "{screen}");
        assert!(screen.contains("enter открыть"), "{screen}");
        assert!(!screen.contains("[ Подписаться ]"), "{screen}");
    }

    #[test]
    fn podcast_share_confirmation_lists_the_warnings_in_russian() {
        let mut state =
            podcast_fixtures::ready_state(vec![podcast_fixtures::row("Test Show", 0, 1, 0)]);
        state.popup = Some(crate::app::state::Popup::ConfirmPodcastShare {
            podcast_id: podcast_fixtures::podcast_id(),
            title: "Test Show".into(),
            urls: crate::podcasts::publication::ConfirmedUrls {
                feed_url: music_dht::podcast::FeedUrl::parse(
                    "https://example.test/feed.xml?token=abc",
                )
                .unwrap(),
                id_feed_url: None,
            },
            warnings: vec!["в адресе ленты есть параметр «token», похожий на ключ доступа".into()],
            already_shared: false,
        });
        let screen = render(&state, 100, 30);
        assert!(
            screen.contains("Поделиться подкастом «Test Show»"),
            "{screen}"
        );
        assert!(screen.contains("Адрес похож на личный"), "{screen}");
        assert!(screen.contains("параметр «token»"), "{screen}");
        assert!(screen.contains("feed.xml?token=abc"), "{screen}");
        assert!(
            screen.contains("enter/y опубликовать · esc/n отмена"),
            "{screen}"
        );
        assert!(!screen.contains("x не делиться"), "{screen}");
    }

    #[test]
    fn podcast_seasons_and_episodes_render_progress() {
        use music_dht::podcast::ProgressStatus;

        let mut state =
            podcast_fixtures::ready_state(vec![podcast_fixtures::row("Test Show", 1, 5, 1)]);
        state.podcasts.opened = Some(crate::app::state::OpenedPodcast {
            podcast_id: podcast_fixtures::podcast_id(),
            season_cursor: Some(0),
            season: None,
            episode_cursor: 0,
        });
        state.podcasts.seasons = Some((
            podcast_fixtures::podcast_id(),
            Loadable::Ready(podcast_fixtures::seasons()),
        ));
        let screen = render(&state, 100, 14);
        assert!(screen.contains("All episodes"), "{screen}");
        assert!(screen.contains("1 unplayed · 5 ep"), "{screen}");
        assert!(screen.contains("Season 1"), "{screen}");
        assert!(screen.contains("2021–2022"), "{screen}");
        assert!(screen.contains("Without a season"), "{screen}");

        let state = podcast_fixtures::with_episodes(
            podcast_fixtures::ready_state(vec![podcast_fixtures::row("Test Show", 1, 3, 0)]),
            vec![
                podcast_fixtures::episode(1, "Pilot", Some((ProgressStatus::Completed, 0))),
                podcast_fixtures::episode(2, "Second", Some((ProgressStatus::InProgress, 754_000))),
                podcast_fixtures::episode(3, "Third", None),
            ],
        );
        let screen = render(&state, 110, 30);
        assert!(
            screen.contains("3 episodes · all episodes · newest first"),
            "{screen}"
        );
        assert!(screen.contains("✓ 2024-06-15  S1E1 Pilot"), "{screen}");
        assert!(screen.contains("played · 45:00"), "{screen}");
        assert!(screen.contains("◐ 2024-06-15  S1E2 Second"), "{screen}");
        assert!(screen.contains("12:34 / 45:00"), "{screen}");
        assert!(screen.contains("● 2024-06-15  S1E3 Third"), "{screen}");
        assert!(screen.contains("Notes for Pilot."), "{screen}");

        // The episode playing here shows its live position.
        let mut state = state;
        let mut playing = podcast_fixtures::episode_track(3);
        playing.title = "Third".into();
        state.player.queue = vec![playing.clone()];
        state.player.current = Some(playing);
        state.player.playing = true;
        state.player.position_secs = 61.0;
        let screen = render(&state, 110, 30);
        assert!(screen.contains("▶ 2024-06-15  S1E3 Third"), "{screen}");
        assert!(screen.contains("1:01 / 45:00"), "{screen}");
        assert!(screen.contains("◐ 2024-06-15  S1E2 Second"), "{screen}");
    }

    #[test]
    fn podcast_unsubscribe_confirmation_says_progress_is_kept() {
        let mut state =
            podcast_fixtures::ready_state(vec![podcast_fixtures::row("Test Show", 0, 1, 0)]);
        state.popup = Some(crate::app::state::Popup::ConfirmPodcastUnsubscribe {
            podcast_id: podcast_fixtures::podcast_id(),
            title: "Test Show".into(),
        });
        let screen = render(&state, 100, 24);
        assert!(
            screen.contains("Unsubscribe from \"Test Show\"?"),
            "{screen}"
        );
        assert!(screen.contains("progress and history are kept"), "{screen}");
    }

    #[test]
    fn help_lists_tab_names_for_digit_shortcuts() {
        let mut state = AppState::default();
        state.help_visible = true;
        let screen = render(&state, 160, 80);
        for title in ["Go to Podcasts", "Go to Settings", "Go to Logs"] {
            assert!(screen.contains(title), "{title} missing:\n{screen}");
        }
        assert!(!screen.contains("Go to tab"), "{screen}");
    }
}

pub fn draw(frame: &mut Frame, state: &AppState, keymap: &Keymap) {
    if state.visualizer.active {
        crate::visualizer::draw(frame, state);
        if state.shutting_down {
            draw_shutdown(frame, state);
        }
        return;
    }

    let [tabs_area, main_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(frame.area());

    draw_tabs(frame, tabs_area, state);
    match state.active_tab {
        Tab::Global => global::draw(frame, main_area, state),
        Tab::Playlists => playlists::draw(frame, main_area, state),
        Tab::Queue => draw_queue(frame, main_area, state),
        Tab::Podcasts => podcasts::draw(frame, main_area, state),
        Tab::Federation => federation::draw(frame, main_area, state),
        Tab::Logs => logs::draw(frame, main_area, state),
    }
    draw_status(frame, status_area, state);

    if state.additional_settings_open {
        additional_settings::draw(frame, state);
    }

    if state.help_visible {
        draw_help(frame, keymap, state);
    }
    popup::draw(frame, state);
    if state.shutting_down {
        draw_shutdown(frame, state);
    }
}

fn draw_shutdown(frame: &mut Frame, state: &AppState) {
    let area = centered(frame.area(), 28, 5);
    frame.render_widget(Clear, area);
    let block = Block::bordered().border_style(theme::strong_border_for(state));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Line::styled("Shutting down...", theme::header_for(state)))
            .alignment(Alignment::Center),
        Rect {
            y: inner.y + inner.height / 2,
            height: 1,
            ..inner
        },
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

pub(crate) fn loading_line(state: &AppState, text: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{} ", state.spinner()), theme::accent_for(state)),
        Span::styled(text.into(), theme::dim()),
    ])
}

fn is_waiting_message(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("loading")
        || normalized.contains("searching")
        || normalized.contains("fetching")
        || normalized.contains("downloading")
        || normalized.contains("locating")
        || normalized.contains("importing")
        || normalized.contains("waiting")
        || normalized.contains("assembling")
        || normalized.contains("resolving")
}

fn draw_tabs(frame: &mut Frame, area: Rect, state: &AppState) {
    frame.render_widget(tab_bar(state, area.width), area);
}

/// The widest tab bar that fits `width`. Narrow terminals drop the outer
/// padding, then abbreviate titles, then show only numbers (keeping the
/// active tab's full title), so all six shortcuts and the highlight stay
/// visible instead of the last tabs being cut off.
fn tab_bar(state: &AppState, width: u16) -> Tabs<'static> {
    use unicode_width::UnicodeWidthStr as _;

    let active = state.active_tab;
    let build = |label: &dyn Fn(Tab) -> String, padded: bool| {
        let tabs = Tabs::new(Tab::ALL.iter().map(|&tab| label(tab)))
            .select(active.index())
            .style(theme::dim())
            .highlight_style(theme::tab_active_for(state))
            .divider("");
        if padded { tabs } else { tabs.padding("", "") }
    };
    let full = |tab: Tab| format!(" {} {} ", tab.index() + 1, tab.title());
    let short = |tab: Tab| format!(" {} {} ", tab.index() + 1, tab.short_title());
    let numbers_with_active = |tab: Tab| {
        if tab == active {
            full(tab)
        } else {
            format!(" {} ", tab.index() + 1)
        }
    };
    let numbers = |tab: Tab| format!(" {} ", tab.index() + 1);
    let candidates: [(&dyn Fn(Tab) -> String, bool); 4] = [
        (&full, true),
        (&full, false),
        (&short, false),
        (&numbers_with_active, false),
    ];
    for (label, padded) in candidates {
        let tabs = build(label, padded);
        if tabs.width() <= usize::from(width) {
            return tabs;
        }
    }
    build(&numbers, false)
}

/// One track row used by every track list: ♥ marker for liked tracks, the
/// title and artists on the left, tech info and duration on the right.
pub(crate) fn track_row(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    track: &crate::library::models::TrackItem,
    index_label: String,
    selected: bool,
    visual_selected: bool,
) {
    track_row_with_like_marker(
        frame,
        area,
        state,
        track,
        index_label,
        selected,
        visual_selected,
        true,
    );
}

pub(crate) fn track_row_with_like_marker(
    frame: &mut Frame,
    area: Rect,
    state: &AppState,
    track: &crate::library::models::TrackItem,
    index_label: String,
    selected: bool,
    visual_selected: bool,
    show_like_marker: bool,
) {
    let heart = if !show_like_marker {
        Span::raw("")
    } else if state.track_liked(track) {
        Span::styled("♥ ", theme::accent_for(state))
    } else {
        Span::raw("  ")
    };
    let fed_marker = if track.fed.is_some() {
        let availability = if state.track_content_local(track) {
            Availability::Local
        } else {
            Availability::Remote
        };
        availability_prefix(availability)
    } else {
        Span::raw("")
    };
    let line = Line::from(vec![
        Span::styled(format!("{index_label:>3} "), theme::dim()),
        heart,
        fed_marker,
        Span::raw(track.title.clone()),
        Span::styled(format!("  {}", track.artist_line()), theme::dim()),
    ]);
    frame.render_widget(Paragraph::new(line), area);

    let right = track_meta_suffix(track, area.width >= 60);
    frame.render_widget(
        Paragraph::new(Line::styled(right, theme::dim())).alignment(Alignment::Right),
        area,
    );
    if visual_selected {
        frame
            .buffer_mut()
            .set_style(area, theme::selection_for(state));
    }
    if selected {
        frame
            .buffer_mut()
            .set_style(area, theme::tab_active_for(state));
    }
}

pub(crate) fn track_meta_suffix(
    track: &crate::library::models::TrackItem,
    include_tech: bool,
) -> String {
    let has_tech = track.audio_format.is_some()
        || track.audio_bitrate.is_some()
        || track.file_size_bytes.is_some();
    if !include_tech || !has_tech {
        return track.duration_label();
    }

    let format = track
        .audio_format
        .as_deref()
        .map(|value| value.to_ascii_uppercase())
        .unwrap_or_default();
    let format: String = format.chars().take(4).collect();
    let bitrate = track
        .audio_bitrate
        .map(|value| format!("{value}k"))
        .unwrap_or_default();
    let size = track
        .file_size_bytes
        .map(|bytes| format!("{:.1}MB", bytes as f64 / 1_048_576.0))
        .unwrap_or_default();

    format!(
        "{format:<4} {bitrate:>5} {size:>8} · {:>5}",
        track.duration_label()
    )
}

/// Interactive queue: its own cursor, enter plays the selected track and
/// already-played tracks stay listed, greyed out.
fn draw_queue(frame: &mut Frame, area: Rect, state: &AppState) {
    let player = &state.player;
    let block = Block::bordered()
        .title(format!(
            " Queue — {} tracks · Mode: {} · enter: play · alt-j/k: move · d: remove · shift-v: select · :clear ",
            player.queue.len(),
            state.global.filters.source_mode.label()
        ))
        .title_style(theme::header_for(state))
        .border_style(theme::border_for(state));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if player.queue.is_empty() {
        let middle = Rect {
            y: inner.y + inner.height / 2,
            height: 1,
            ..inner
        };
        frame.render_widget(
            Paragraph::new(Line::styled(
                "queue is empty — open a track and press enter",
                theme::dim(),
            ))
            .alignment(Alignment::Center),
            middle,
        );
        return;
    }

    let cursor = state.queue_tab.cursor.min(player.queue.len() - 1);
    let visible = usize::from(inner.height.max(1));
    let first = cursor
        .saturating_sub(visible / 2)
        .min(player.queue.len().saturating_sub(visible));
    let played_style = Style::new().fg(Color::DarkGray).bg(Color::Rgb(28, 28, 32));
    for (index, track) in player.queue.iter().enumerate().skip(first).take(visible) {
        let row = Rect {
            x: inner.x,
            y: inner.y + (index - first) as u16,
            width: inner.width,
            height: 1,
        };
        let label = if index == player.queue_pos && player.playing {
            "▶".to_string()
        } else {
            (index + 1).to_string()
        };
        let visual_selected = state
            .track_selection
            .contains(&TrackSelectionScope::Queue, index);
        track_row(
            frame,
            row,
            state,
            track,
            label,
            index == cursor,
            visual_selected,
        );
        // Tracks before the playing one are history: greyed out unless the
        // cursor is on them.
        if index < player.queue_pos && index != cursor && !visual_selected {
            frame.buffer_mut().set_style(row, played_style);
        }
    }
}

fn format_secs(secs: f64) -> String {
    let total = secs.max(0.0).round() as i64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// Playback time, progress bar, queue position, volume and mode flags.
/// Wider consoles get a longer bar and full flags; narrow ones drop pieces.
fn player_right_line(state: &AppState, width: u16) -> (Line<'static>, u16) {
    let player = &state.player;
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(track) = &player.current
        && player.playing
    {
        let bar_width: usize = match width {
            0..=59 => 0,
            60..=79 => 8,
            80..=109 => 14,
            _ => 22,
        };
        spans.push(Span::raw(format!("{} ", format_secs(player.position_secs))));
        if bar_width > 0 && track.duration_seconds > 0.0 {
            let ratio = (player.position_secs / track.duration_seconds).clamp(0.0, 1.0);
            let filled = (ratio * bar_width as f64).round() as usize;
            spans.push(Span::styled("━".repeat(filled), theme::accent_for(state)));
            spans.push(Span::styled("─".repeat(bar_width - filled), theme::dim()));
            spans.push(Span::raw(" "));
        } else {
            spans.push(Span::styled("/ ", theme::dim()));
        }
        spans.push(Span::raw(track.duration_label()));
        if !player.queue.is_empty() && width >= 70 {
            spans.push(Span::styled(
                format!(" [{}/{}]", player.queue_pos + 1, player.queue.len()),
                theme::dim(),
            ));
        }
    }
    let volume_offset = spans.iter().map(Span::width).sum::<usize>() + 2;
    if width >= 80 {
        let volume_cells = usize::from(player.volume / 10);
        spans.extend([
            Span::styled("  vol ", theme::dim()),
            Span::styled("█".repeat(volume_cells), theme::accent_for(state)),
            Span::styled("░".repeat(10 - volume_cells), theme::dim()),
            Span::raw(format!(" {:3}%", player.volume)),
            Span::raw("  "),
        ]);
        // Enabled modes light up as filled chips; disabled stay dim text.
        if player.shuffle {
            spans.push(Span::styled(" shuffle ", theme::tab_active_for(state)));
        } else {
            spans.push(Span::styled("shuffle off", theme::dim()));
        }
        spans.push(Span::raw("  "));
        if player.repeat == crate::app::state::RepeatMode::Off {
            spans.push(Span::styled("repeat off", theme::dim()));
        } else {
            spans.push(Span::styled(
                format!(" repeat {} ", player.repeat.label()),
                theme::tab_active_for(state),
            ));
        }
    } else {
        spans.push(Span::styled(format!("  {}%", player.volume), theme::dim()));
    }
    if width >= 70 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!(" {} ", state.device_playback.role.label()),
            theme::role_pill(state.device_playback.role),
        ));
        if width >= 160 {
            spans.push(Span::styled(
                format!(" · online {}", state.device_playback.online_devices.max(1)),
                theme::dim(),
            ));
        }
    }
    // Keep a gap between the flags and the username block to the right.
    spans.push(Span::raw("  "));
    (
        Line::from(spans),
        volume_offset.min(usize::from(u16::MAX)) as u16,
    )
}

/// Reserve the source-mode badge first so playback details cannot clip it.
fn player_row_areas(row: Rect, state: &AppState) -> [Rect; 3] {
    let badge_width = (format!(" federation: {} ", state.global.filters.source_mode.label()).len()
        as u16)
        .min(row.width);
    let content = Rect::new(
        row.x,
        row.y,
        row.width.saturating_sub(badge_width),
        row.height,
    );
    let badge = Rect::new(content.right(), row.y, badge_width, row.height);
    let (line, _) = player_right_line(state, content.width);
    let right_width = line.width().min(usize::from(content.width)) as u16;
    let [title, right] = Layout::horizontal([
        Constraint::Min(8.min(content.width)),
        Constraint::Length(right_width),
    ])
    .areas(content);
    [title, right, badge]
}

/// Shared geometry for rendering and background visualization requests.
fn footer_areas(area: Rect, state: &AppState) -> [Rect; 3] {
    let [player_row, message_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let [_, right, badge] = player_row_areas(player_row, state);
    let (_, volume_offset) = player_right_line(state, area.width.saturating_sub(badge.width));
    let version_width = (env!("CARGO_PKG_VERSION").len() as u16 + 1).min(message_row.width);
    let version = Rect::new(
        message_row.x + message_row.width.saturating_sub(version_width) / 2,
        message_row.y,
        version_width,
        message_row.height,
    );
    let message = Rect::new(
        message_row.x,
        message_row.y,
        version.x.saturating_sub(message_row.x).saturating_sub(1),
        message_row.height,
    );
    // On wide terminals align exactly with "vol". On narrow ones keep the
    // centered version readable, shrinking the histogram into the remainder.
    let start = right
        .x
        .saturating_add(volume_offset)
        .max(version.right().saturating_add(1))
        .min(message_row.right());
    let visual = Rect::new(
        start,
        message_row.y,
        message_row.right().saturating_sub(start),
        message_row.height,
    );
    [message, version, visual]
}

pub(crate) fn status_visualizer_area(screen: Rect, state: &AppState) -> Rect {
    let [_, _, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(screen);
    footer_areas(status, state)[2]
}

fn draw_status(frame: &mut Frame, area: Rect, state: &AppState) {
    let [player_row, _] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    let [message_row, version_area, visual_area] = footer_areas(area, state);

    let player = &state.player;
    // Layout: track title left, time/progress/flags on the right. The
    // right block is built first and gets a fixed width; the title
    // truncates into whatever is left.
    let [title_area, right_area, mode_area] = player_row_areas(player_row, state);
    let (center, _) = player_right_line(state, area.width.saturating_sub(mode_area.width));

    let mut spans = Vec::new();
    match &player.current {
        Some(track) if player.playing => {
            if player.paused {
                spans.push(Span::styled("⏸ ", theme::dim()));
            } else {
                spans.push(Span::styled("▶ ", theme::accent_for(state)));
            }
            if state.track_liked(track) {
                spans.push(Span::styled("♥ ", theme::accent_for(state)));
            }
            spans.push(Span::raw(track.title.clone()));
            spans.push(Span::styled(
                format!(" — {}", track.artist_line()),
                theme::dim(),
            ));
        }
        _ => {
            spans.push(Span::styled("■ stopped", theme::dim()));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), title_area);
    frame.render_widget(Paragraph::new(center), right_area);
    let mode = state.global.filters.source_mode;
    frame.render_widget(
        Paragraph::new(format!(" federation: {} ", mode.label()))
            .style(theme::source_mode_pill(mode)),
        mode_area,
    );
    draw_version(frame, version_area);
    if crate::visualizer::status::visible(state) {
        state.status_visualizer.draw(frame, visual_area);
    }

    if state.cmdline.active {
        // Vim-style command line takes over the message row.
        let mut spans = vec![Span::styled(":", theme::header_for(state))];
        spans.extend(line_edit_spans(
            &state.cmdline.input,
            usize::from(message_row.width.saturating_sub(2)),
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), message_row);
        return;
    }

    let message = match &state.status_message {
        Some(message) if is_waiting_message(message) => Line::from(vec![
            Span::styled(format!("{} ", state.spinner()), theme::accent_for(state)),
            Span::styled(message.clone(), theme::accent_for(state)),
        ]),
        Some(message) => Line::styled(message.clone(), theme::accent_for(state)),
        None => match active_artist_peer_search(state) {
            Some(message) => loading_line(state, message),
            None => match &state.player.current {
                // Idle line doubles as the current track's tech data display.
                Some(track) if state.player.playing && !track.tech_label_full().is_empty() => {
                    Line::styled(track.tech_label_full(), theme::dim())
                }
                _ => Line::styled("press ? for keybindings", theme::dim()),
            },
        },
    };
    frame.render_widget(Paragraph::new(message), message_row);

    if let Some(pending) = &state.pending_keys {
        let pending = Paragraph::new(Line::styled(
            format!("{pending} …"),
            theme::header_for(state),
        ))
        .alignment(Alignment::Right);
        frame.render_widget(pending, message_row);
    }
}

fn active_artist_peer_search(state: &AppState) -> Option<String> {
    if state.active_tab != Tab::Global {
        return None;
    }
    let Some(GlobalView::Artist { id, .. }) = state.global.stack.last() else {
        return None;
    };
    if !state.artist_federation_loading(*id) {
        return None;
    }
    let name = match state.artist_views.get(id) {
        Some(Loadable::Ready(detail)) => detail.name.as_str(),
        _ => "artist",
    };
    Some(format!("searching peers for \"{name}\"…"))
}

fn draw_version(frame: &mut Frame, area: Rect) {
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    frame.render_widget(
        Paragraph::new(Line::styled(version, theme::dim())).alignment(Alignment::Center),
        area,
    );
}

/// Help window: bindings merged per action (j / down on one row), grouped
/// into titled sections and laid out in two balanced columns.
fn draw_help(frame: &mut Frame, keymap: &Keymap, state: &AppState) {
    use crate::app::action::{Action, Category};
    use crate::config::keymap::KeyContext;

    struct MergedRow {
        keys: Vec<String>,
        action: Action,
        context: KeyContext,
    }
    let mut merged: Vec<MergedRow> = Vec::new();
    for (keys, action, context) in keymap.help_entries() {
        match merged
            .iter_mut()
            .find(|row| row.action == action && row.context == context)
        {
            Some(row) => row.keys.push(keys),
            None => merged.push(MergedRow {
                keys: vec![keys],
                action,
                context,
            }),
        }
    }

    // One block of lines per category: section header + its rows.
    let mut blocks: Vec<Vec<Line>> = Vec::new();
    for category in Category::ALL {
        let rows: Vec<&MergedRow> = merged
            .iter()
            .filter(|row| row.action.category() == category)
            .collect();
        if rows.is_empty() {
            continue;
        }
        let mut lines = vec![Line::styled(category.title(), theme::header_for(state))];
        for row in rows {
            let keys = row.keys.join(" / ");
            let context = if row.context == KeyContext::Global {
                String::new()
            } else {
                format!(" [{}]", row.context.label())
            };
            let command = row.action.command_hint().unwrap_or("");
            lines.push(Line::from(vec![
                Span::styled(format!("{keys:<13}"), theme::accent_for(state)),
                Span::raw(format!(
                    "{:<24}",
                    format!("{}{context}", row.action.describe())
                )),
                Span::styled(command.to_string(), theme::accent_for(state)),
            ]));
        }
        lines.push(Line::default());
        blocks.push(lines);
    }
    blocks.push(vec![
        Line::styled("Status icons", theme::header_for(state)),
        Line::from(vec![
            Span::styled("●", Style::new().fg(Color::Green)),
            Span::raw("  Local on this device"),
        ]),
        Line::from(vec![
            Span::styled("◐", Style::new().fg(Color::Yellow)),
            Span::raw("  Local + peer sources"),
        ]),
        Line::from(vec![
            Span::styled("⇅", theme::accent_for(state)),
            Span::raw("  Network only"),
        ]),
        Line::default(),
    ]);

    // Balance the blocks across two columns.
    let total: usize = blocks.iter().map(Vec::len).sum();
    let mut left: Vec<Line> = Vec::new();
    let mut right: Vec<Line> = Vec::new();
    for block in blocks {
        if left.len() < total.div_ceil(2) {
            left.extend(block);
        } else {
            right.extend(block);
        }
    }

    let column_height = left.len().max(right.len()) as u16;
    let width = 110.min(frame.area().width.saturating_sub(2));
    let height = (column_height + 4).min(frame.area().height.saturating_sub(2));
    let area = centered_rect(frame.area(), width, height);

    let block = Block::bordered()
        .title(" Keybindings & commands ")
        .title_style(theme::header_for(state))
        .border_style(theme::strong_border_for(state));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);

    let [columns_area, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let [left_area, _, right_area] = Layout::horizontal([
        Constraint::Percentage(50),
        Constraint::Length(2),
        Constraint::Percentage(50),
    ])
    .areas(columns_area);
    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(Paragraph::new(right), right_area);
    frame.render_widget(
        Paragraph::new(Line::styled(
            ": opens the command line · full forms: :seek +30|1:30 · :volume 0-100 · :repeat off|one|all · :logs [level]",
            theme::dim(),
        ))
        .alignment(Alignment::Center),
        footer,
    );
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let [rect] = Layout::horizontal([Constraint::Length(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [rect] = Layout::vertical([Constraint::Length(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(rect);
    rect
}

/// Renders a [`LineEdit`] as spans with a visible cursor, windowed so the
/// cursor always stays on screen when the value is wider than `width`.
pub(crate) fn line_edit_spans(edit: &LineEdit, width: usize) -> Vec<Span<'static>> {
    let width = width.max(2);
    let chars: Vec<char> = edit.as_str().chars().collect();
    let cursor = edit.cursor().min(chars.len());
    // Window start: keep the cursor within the visible slice (one cell is
    // reserved for the cursor block itself when it sits at the end).
    let start = (cursor + 1).saturating_sub(width);
    let end = (start + width.saturating_sub(1)).min(chars.len());
    let before: String = chars[start..cursor].iter().collect();
    let (under, after): (String, String) = if cursor < chars.len() {
        (
            chars[cursor].to_string(),
            chars[cursor + 1..end.max(cursor + 1)].iter().collect(),
        )
    } else {
        ("█".to_string(), String::new())
    };
    let cursor_style = if cursor < chars.len() {
        ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::REVERSED)
    } else {
        theme::accent()
    };
    vec![
        Span::raw(before),
        Span::styled(under, cursor_style),
        Span::raw(after),
    ]
}
