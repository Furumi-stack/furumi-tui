//! Command line (`:`) command parsing.
//!
//! To add a command:
//! 1. Add a `Command` variant.
//! 2. Recognize it in `parse()` below.
//! 3. Handle it in `app::cmdline` — live commands (re-evaluated on every
//!    keystroke, like search) in `apply_live`, one-shot commands in `commit`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `:/query` — realtime search over artists, releases and tracks.
    Search(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Nothing typed yet.
    Empty,
    Command(Command),
    Unknown(String),
}

pub fn parse(input: &str) -> Parsed {
    if input.is_empty() {
        return Parsed::Empty;
    }
    if let Some(query) = input.strip_prefix('/') {
        return Parsed::Command(Command::Search(query.trim().to_string()));
    }
    // Future word commands parse here, e.g. "volume 50" / "seek +30".
    let name = input.split_whitespace().next().unwrap_or(input);
    Parsed::Unknown(name.to_string())
}

/// Live commands take effect while typing; one-shot commands run on Enter.
pub fn is_live(command: &Command) -> bool {
    matches!(command, Command::Search(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search() {
        assert_eq!(
            parse("/daft punk"),
            Parsed::Command(Command::Search("daft punk".to_string()))
        );
        assert_eq!(parse("/"), Parsed::Command(Command::Search(String::new())));
    }

    #[test]
    fn unknown_and_empty() {
        assert_eq!(parse(""), Parsed::Empty);
        assert_eq!(parse("volume 50"), Parsed::Unknown("volume".to_string()));
    }

    #[test]
    fn search_is_live() {
        assert!(is_live(&Command::Search("x".into())));
    }
}
