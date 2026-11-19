//! The public podcast card opened from the search results. In Russian, like
//! the share confirmation: «Подписаться» / «Уже подписан» are the actions
//! the requirements name.
//!
//! Everything but the origin feed URL is a publisher's claim and is shown as
//! such; the episode preview comes from the feed itself.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use super::podcasts::{format_date, format_duration_ms};
use super::popup::centered;
use super::theme;
use crate::app::state::{AppState, Loadable};
use crate::podcasts::discovery::{FeedPreview, PodcastHit};

pub(super) fn draw(
    frame: &mut Frame,
    state: &AppState,
    hit: &PodcastHit,
    preview: Option<&Loadable<FeedPreview>>,
    scroll: u16,
) {
    let subscribed =
        crate::app::update::podcast_discovery::subscribed_id(state, hit, preview).is_some();
    let subscribing = state.podcasts.subscribing;
    let lines = card_lines(state, hit, preview, subscribed, subscribing);

    let width = 92.min(frame.area().width.saturating_sub(4)).max(30);
    let inner_width = width.saturating_sub(2).max(1);
    let body_height = lines
        .iter()
        .map(|line| (line.width().max(1) as u16).div_ceil(inner_width))
        .sum::<u16>();
    let height = (body_height + 4).min(frame.area().height.saturating_sub(2));
    let area = centered(frame.area(), width, height);
    let block = Block::bordered()
        .title(" Подкаст · публичная карточка ")
        .title_style(theme::header_for(state))
        .border_style(theme::strong_border_for(state));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let [body, footer_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let max_scroll = body_height.saturating_sub(body.height);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll.min(max_scroll), 0)),
        body,
    );
    let footer = if subscribed {
        "enter открыть · e выпуски · ↑↓ прокрутка · esc закрыть"
    } else {
        "enter подписаться · e выпуски · ↑↓ прокрутка · esc закрыть"
    };
    frame.render_widget(
        Paragraph::new(Line::styled(footer, theme::dim())).alignment(Alignment::Center),
        footer_area,
    );
}

fn card_lines(
    state: &AppState,
    hit: &PodcastHit,
    preview: Option<&Loadable<FeedPreview>>,
    subscribed: bool,
    subscribing: bool,
) -> Vec<Line<'static>> {
    let card = &hit.hit.card;
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("[podcast] ", theme::accent_for(state)),
            Span::styled(card.title.clone(), bold),
        ]),
        Line::styled(
            "Сведения ниже указали издатели карточки; они не проверены.",
            theme::dim(),
        ),
        Line::raw(""),
    ];
    if let Some(author) = &card.author {
        lines.push(field("Автор", author.clone()));
    }
    if !card.categories.is_empty() {
        lines.push(field("Категории", card.categories.join(", ")));
    }
    if let Some(language) = &card.language {
        lines.push(field("Язык", language.clone()));
    }
    let mut episodes = match card.episode_count {
        Some(count) => format!("{count}"),
        None => "не указано".to_string(),
    };
    if let Some(latest) = card.latest_episode_at_ms {
        episodes.push_str(&format!(
            " · последний {}",
            format_date(latest.div_euclid(1000))
        ));
    }
    lines.push(field("Выпусков", episodes));
    let publishers = hit.hit.publishers.len();
    lines.push(field(
        "Опубликовали",
        format!(
            "{publishers} {} (не число подписчиков и не знак доверия)",
            plural_ru(publishers, "устройство", "устройства", "устройств")
        ),
    ));
    lines.push(field("Лента", hit.hit.feed_url.as_str().to_string()));
    for claim in &hit.hit.claimed_feed_urls {
        lines.push(Line::styled(
            format!(
                "Заявленный новый адрес (не проверен, не используется): {}",
                claim.feed_url.as_str()
            ),
            theme::dim(),
        ));
    }
    if let Some(link) = &card.link {
        lines.push(field("Сайт", link.clone()));
    }
    if let Some(description) = &card.description {
        lines.push(Line::raw(""));
        lines.extend(
            description
                .lines()
                .map(|line| Line::raw(line.replace('\t', "    "))),
        );
    }

    lines.push(Line::raw(""));
    if subscribed {
        lines.push(Line::from(vec![
            Span::styled(
                "✓ Уже подписан",
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  · enter откроет подкаст во вкладке Podcasts",
                theme::dim(),
            ),
        ]));
    } else if subscribing {
        lines.push(Line::styled(
            "Подписка… читаем ленту",
            theme::accent_for(state),
        ));
    } else {
        lines.push(Line::from(vec![
            Span::styled(
                "[ Подписаться ]",
                theme::accent_for(state).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  лента будет прочитана с её сервера; подписка личная и не публикуется",
                theme::dim(),
            ),
        ]));
    }

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Сезоны и выпуски (из RSS-ленты)",
        theme::header_for(state),
    ));
    match preview {
        None => lines.push(Line::styled(
            "e — прочитать ленту без подписки (запрос к серверу ленты)",
            theme::dim(),
        )),
        Some(Loadable::Loading) => {
            lines.push(Line::styled("читаем ленту…", theme::accent_for(state)))
        }
        Some(Loadable::Failed(err)) => lines.push(Line::styled(
            format!("не удалось прочитать ленту: {err}"),
            Style::new().fg(Color::Red),
        )),
        Some(Loadable::Ready(feed)) => preview_lines(&mut lines, card.title.as_str(), feed),
    }
    lines
}

fn preview_lines(lines: &mut Vec<Line<'static>>, card_title: &str, feed: &FeedPreview) {
    if feed.title != card_title {
        lines.push(field("Название в ленте", feed.title.clone()));
    }
    if let Some(moved) = &feed.moved_to {
        lines.push(field("Лента переехала", moved.as_str().to_string()));
    }
    let seasons: Vec<String> = feed
        .seasons
        .iter()
        .map(|(season, count)| match season {
            Some(season) => format!("сезон {season}: {count}"),
            None => format!("без сезона: {count}"),
        })
        .collect();
    lines.push(field(
        "В ленте",
        format!(
            "{} {}{}",
            feed.episode_count,
            plural_ru(feed.episode_count, "выпуск", "выпуска", "выпусков"),
            if seasons.is_empty() {
                String::new()
            } else {
                format!(" · {}", seasons.join(", "))
            }
        ),
    ));
    for episode in &feed.episodes {
        let mut meta = Vec::new();
        match (episode.season, episode.episode_number) {
            (Some(season), Some(number)) => meta.push(format!("S{season}E{number}")),
            (Some(season), None) => meta.push(format!("S{season}")),
            (None, Some(number)) => meta.push(format!("#{number}")),
            (None, None) => {}
        }
        if let Some(published) = episode.published_at_ms {
            meta.push(format_date(published.div_euclid(1000)));
        }
        if let Some(duration) = episode.duration_ms {
            meta.push(format_duration_ms(duration));
        }
        lines.push(Line::from(vec![
            Span::raw(format!("• {}", episode.title)),
            Span::styled(
                if meta.is_empty() {
                    String::new()
                } else {
                    format!("  {}", meta.join(" · "))
                },
                theme::dim(),
            ),
        ]));
    }
    if feed.episodes.len() < feed.episode_count {
        lines.push(Line::styled(
            format!("… и ещё {}", feed.episode_count - feed.episodes.len()),
            theme::dim(),
        ));
    }
}

fn field(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), theme::dim()),
        Span::raw(value),
    ])
}

/// Russian plural form for `n`.
fn plural_ru(n: usize, one: &'static str, few: &'static str, many: &'static str) -> &'static str {
    let (tens, units) = (n % 100, n % 10);
    if (11..=14).contains(&tens) {
        many
    } else if units == 1 {
        one
    } else if (2..=4).contains(&units) {
        few
    } else {
        many
    }
}

#[cfg(test)]
mod tests {
    use super::plural_ru;

    #[test]
    fn russian_plurals() {
        let forms = |n| plural_ru(n, "устройство", "устройства", "устройств");
        assert_eq!(forms(1), "устройство");
        assert_eq!(forms(3), "устройства");
        assert_eq!(forms(5), "устройств");
        assert_eq!(forms(11), "устройств");
        assert_eq!(forms(21), "устройство");
        assert_eq!(forms(0), "устройств");
    }
}
