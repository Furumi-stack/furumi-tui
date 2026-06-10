use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::api::{auth, client};
use crate::app::Runtime;
use crate::app::event::AppEvent;
use crate::app::sso;
use crate::app::state::{AppState, LoginField, LoginForm, LoginMode};

pub fn handle_key(state: &mut AppState, runtime: &mut Runtime, key: KeyEvent) {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        state.should_quit = true;
        return;
    }
    let form = &mut state.login;
    if form.busy {
        return;
    }
    match form.mode {
        LoginMode::Form => handle_form_key(form, runtime, key),
        LoginMode::SsoPending => handle_sso_key(form, runtime, key),
    }
}

/// Bracketed paste goes into whichever text field is focused.
pub fn handle_paste(state: &mut AppState, pasted: &str) {
    let form = &mut state.login;
    if form.busy {
        return;
    }
    let cleaned: String = pasted.chars().filter(|c| !c.is_control()).collect();
    if let Some(field) = focused_text(form) {
        field.push_str(&cleaned);
    }
}

fn handle_form_key(form: &mut LoginForm, runtime: &mut Runtime, key: KeyEvent) {
    match key.code {
        KeyCode::Tab | KeyCode::Down => form.focus = form.focus.next(),
        KeyCode::BackTab | KeyCode::Up => form.focus = form.focus.prev(),
        KeyCode::Backspace => {
            if let Some(field) = focused_text(form) {
                field.pop();
            }
        }
        KeyCode::Enter => match form.focus {
            LoginField::ServerUrl | LoginField::Username => form.focus = form.focus.next(),
            LoginField::Password | LoginField::SignInButton => {
                submit_password(form, runtime);
            }
            LoginField::SsoButton => start_sso(form, runtime),
        },
        KeyCode::Char(c) if is_typing(key) => {
            if let Some(field) = focused_text(form) {
                field.push(c);
            }
        }
        _ => {}
    }
}

fn handle_sso_key(form: &mut LoginForm, runtime: &mut Runtime, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            if let Some(listener) = runtime.sso.take() {
                listener.abort();
            }
            form.mode = LoginMode::Form;
            form.sso_paste.clear();
            form.error = None;
        }
        KeyCode::Backspace => {
            form.sso_paste.pop();
        }
        KeyCode::Enter => submit_sso_code(form, runtime),
        KeyCode::Char(c) if is_typing(key) => form.sso_paste.push(c),
        _ => {}
    }
}

fn is_typing(key: KeyEvent) -> bool {
    key.modifiers
        .difference(KeyModifiers::SHIFT)
        .is_empty()
}

fn focused_text(form: &mut LoginForm) -> Option<&mut String> {
    if form.mode == LoginMode::SsoPending {
        return Some(&mut form.sso_paste);
    }
    match form.focus {
        LoginField::ServerUrl => Some(&mut form.server_url),
        LoginField::Username => Some(&mut form.username),
        LoginField::Password => Some(&mut form.password),
        LoginField::SignInButton | LoginField::SsoButton => None,
    }
}

fn submit_password(form: &mut LoginForm, runtime: &Runtime) {
    form.error = None;
    let base_url = match auth::normalize_base_url(&form.server_url) {
        Ok(url) => url,
        Err(err) => return form.error = Some(err.to_string()),
    };
    let username = form.username.trim().to_string();
    if username.is_empty() {
        return form.error = Some("enter a username".to_string());
    }
    if form.password.is_empty() {
        return form.error = Some("enter a password".to_string());
    }
    form.server_url = base_url.clone();
    form.busy = true;

    let password = form.password.clone();
    let http = runtime.http.clone();
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let result = client::login_password(&http, &base_url, &username, &password).await;
        let _ = tx.send(login_event(result));
    });
}

fn start_sso(form: &mut LoginForm, runtime: &mut Runtime) {
    form.error = None;
    let base_url = match auth::normalize_base_url(&form.server_url) {
        Ok(url) => url,
        Err(err) => return form.error = Some(err.to_string()),
    };
    form.server_url = base_url.clone();
    form.sso_paste.clear();

    // Preferred flow: loopback listener, the browser redirect finishes the
    // login hands-free. Fallback: furumi:// deep link + manual code paste.
    if let Some(listener) = runtime.sso.take() {
        listener.abort();
    }
    match sso::start(runtime.event_tx.clone()) {
        Ok(listener) => {
            let redirect = format!("http://127.0.0.1:{}/callback", listener.port);
            form.sso_url = client::sso_start_url(&base_url, &redirect);
            form.sso_port = Some(listener.port);
            runtime.sso = Some(listener);
        }
        Err(err) => {
            tracing::warn!(%err, "loopback listener unavailable, falling back to manual paste");
            form.sso_url = client::sso_start_url(&base_url, "furumi://auth/callback");
            form.sso_port = None;
        }
    }

    form.mode = LoginMode::SsoPending;
    if let Err(err) = open::that_detached(&form.sso_url) {
        tracing::warn!(%err, "failed to open browser for SSO");
        form.error = Some("couldn't open a browser — use the URL below".to_string());
    }
}

fn submit_sso_code(form: &mut LoginForm, runtime: &Runtime) {
    form.error = None;
    let code = match auth::extract_sso_code(&form.sso_paste) {
        Ok(code) => code,
        Err(err) => return form.error = Some(err.to_string()),
    };
    spawn_sso_exchange(form, runtime, code);
}

/// Used by both the manual paste path and the loopback callback event.
pub fn spawn_sso_exchange(form: &mut LoginForm, runtime: &Runtime, code: String) {
    let base_url = form.server_url.clone();
    form.busy = true;

    let http = runtime.http.clone();
    let tx = runtime.event_tx.clone();
    tokio::spawn(async move {
        let result = client::login_sso_exchange(&http, &base_url, &code).await;
        let _ = tx.send(login_event(result));
    });
}

fn login_event(result: Result<auth::AuthSession, client::ApiError>) -> AppEvent {
    match result {
        Ok(session) => {
            tracing::info!(user = %session.user.name, server = %session.server_base_url, "signed in");
            if let Err(err) = auth::save_session(&session) {
                tracing::warn!(%err, "failed to persist credentials");
            }
            AppEvent::LoginSucceeded(Box::new(session))
        }
        Err(err) => {
            tracing::warn!(%err, "login failed");
            AppEvent::LoginFailed(err.to_string())
        }
    }
}
