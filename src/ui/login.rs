use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use super::theme;
use crate::app::state::{LoginField, LoginForm, LoginMode};

pub fn draw(frame: &mut Frame, form: &LoginForm) {
    match form.mode {
        LoginMode::Form => draw_form(frame, form),
        LoginMode::SsoPending => draw_sso_pending(frame, form),
    }
}

fn draw_form(frame: &mut Frame, form: &LoginForm) {
    let area = centered(frame.area(), 52, 19);
    let block = Block::bordered()
        .title(" Sign in to furumi ")
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // SSO is the primary path: server URL + SSO button up top, the rarely
    // used password fallback below a separator.
    let [server, sso_button, separator, username, password, signin_button, message, hint] =
        Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(inner);

    draw_field(frame, server, "Server URL", &form.server_url, false,
        form.focus == LoginField::ServerUrl);
    draw_button(frame, sso_button, "[ Continue with SSO ]",
        form.focus == LoginField::SsoButton);
    frame.render_widget(
        Paragraph::new(Line::styled("── or sign in with password ──", theme::dim()))
            .alignment(Alignment::Center),
        separator,
    );
    draw_field(frame, username, "Username", &form.username, false,
        form.focus == LoginField::Username);
    draw_field(frame, password, "Password", &form.password, true,
        form.focus == LoginField::Password);
    draw_button(frame, signin_button, "[ Sign in ]",
        form.focus == LoginField::SignInButton);

    draw_message(frame, message, form);
    frame.render_widget(
        Paragraph::new(Line::styled(
            "tab/↑↓ move · enter submit · ctrl-c quit",
            theme::dim(),
        ))
        .alignment(Alignment::Center),
        hint,
    );
}

fn draw_sso_pending(frame: &mut Frame, form: &LoginForm) {
    // The URL stays on ONE line (wrapping breaks copy-paste); the dialog is
    // as wide as the terminal allows and ctrl-l copies the full link.
    let width = (form.sso_url.len() as u16 + 4)
        .clamp(48, frame.area().width.saturating_sub(2).max(40));
    let area = centered(frame.area(), width, 14.min(frame.area().height));

    let block = Block::bordered()
        .title(" Continue with SSO ")
        .title_style(theme::header())
        .border_style(theme::accent());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [steps, url_label, url, paste, message, hint] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(inner);

    let lines = if let Some(port) = form.sso_port {
        vec![
            Line::raw("1. Finish signing in, in the browser window."),
            Line::from(vec![
                Span::raw("2. Sign-in completes here automatically "),
                Span::styled(format!("(waiting on 127.0.0.1:{port})"), theme::dim()),
            ]),
            Line::raw("3. If it doesn't, paste the code from the page below."),
        ]
    } else {
        vec![
            Line::raw("1. Finish signing in, in the browser window."),
            Line::raw("2. Copy the code shown on the final page."),
            Line::raw("3. Paste it below and press Enter."),
        ]
    };
    frame.render_widget(Paragraph::new(lines), steps);

    frame.render_widget(
        Paragraph::new(Line::styled(
            "If the browser didn't open — ctrl-l copies this link, ctrl-o retries:",
            theme::dim(),
        )),
        url_label,
    );
    // One line, never wrapped: a wrapped URL copies with a line break and
    // stops working. If it doesn't fit, ctrl-l still copies it whole.
    frame.render_widget(
        Paragraph::new(Line::styled(form.sso_url.clone(), theme::accent())),
        url,
    );

    draw_field(frame, paste, "Link or code", &form.sso_paste, false, true);
    draw_message(frame, message, form);
    frame.render_widget(
        Paragraph::new(Line::styled(
            "enter submit · ctrl-l copy link · esc back · ctrl-c quit",
            theme::dim(),
        ))
        .alignment(Alignment::Center),
        hint,
    );
}

fn draw_field(frame: &mut Frame, area: Rect, label: &str, value: &str, mask: bool, focused: bool) {
    let border = if focused { theme::accent() } else { theme::dim() };
    let block = Block::bordered().title(label).border_style(border);
    let shown = if mask {
        "•".repeat(value.chars().count())
    } else {
        value.to_string()
    };
    // Keep the tail visible when the value overflows the field.
    let width = block.inner(area).width.saturating_sub(1) as usize;
    let mut text: String = shown
        .chars()
        .skip(shown.chars().count().saturating_sub(width))
        .collect();
    if focused {
        text.push('█');
    }
    frame.render_widget(Paragraph::new(text).block(block), area);
}

fn draw_button(frame: &mut Frame, area: Rect, label: &str, focused: bool) {
    let style = if focused {
        theme::tab_active()
    } else {
        theme::dim()
    };
    frame.render_widget(
        Paragraph::new(Line::styled(label, style)).alignment(Alignment::Center),
        area,
    );
}

fn draw_message(frame: &mut Frame, area: Rect, form: &LoginForm) {
    let line = if form.busy {
        Line::styled("signing in…", theme::accent())
    } else if let Some(error) = &form.error {
        Line::styled(error.clone(), Style::new().fg(Color::Red))
    } else {
        Line::default()
    };
    frame.render_widget(
        Paragraph::new(line)
            .wrap(Wrap { trim: true })
            .alignment(Alignment::Center),
        area,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [rect] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    let [rect] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(rect);
    rect
}
