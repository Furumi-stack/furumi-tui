//! Command line parsing. `/query` is the live search; word commands run on
//! Enter.
//!
//! To add a command:
//! 1. Add a `Command` variant and a match arm in `parse()`.
//! 2. Execute it in `cmdline::execute()`.
//!
//! Live commands (re-evaluated on every keystroke) also need handling in
//! `cmdline::apply_live`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepeatArg {
    Off,
    One,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `/query` — realtime search over artists, releases and tracks.
    Search(String),
    /// `:q` / `:quit` — exit immediately (explicit enough to skip the
    /// double-press confirmation).
    Quit,
    /// `:import <path>` — import an audio file or a directory into the
    /// library.
    Import(String),
    /// `:open frid://...` — open a shared federation content link.
    Open(String),
    /// `:connect frid://i/...` — pair this client with a trusted device.
    ConnectInvite(String),
    /// `:volume 40` (also `:vol`) — set the volume precisely.
    Volume(u8),
    /// `:seek +30` / `:seek -10` — relative seek in seconds.
    Seek(i64),
    /// `:seek 90` / `:seek 1:30` — absolute position.
    SeekTo(u64),
    /// `:shuffle` — toggle shuffle.
    Shuffle,
    /// `:repeat [off|one|all]` — set or cycle the repeat mode.
    Repeat(Option<RepeatArg>),
    /// `:clear` — clear the play queue.
    ClearQueue,
    /// `:next` / `:prev` — queue navigation.
    Next,
    Prev,
    /// `:pause` / `:play` — toggle playback.
    PlayPause,
    /// `:help` — open the keybinding help.
    Help,
    /// `:logs [error|warn|info|debug|trace]` — jump to the Logs tab,
    /// optionally setting the severity filter.
    Logs(Option<usize>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// Nothing typed yet.
    Empty,
    Command(Command),
    /// Recognized command with bad arguments; the message explains usage.
    Invalid(String),
    Unknown(String),
}

pub fn parse(input: &str) -> Parsed {
    if input.is_empty() {
        return Parsed::Empty;
    }
    if let Some(query) = input.strip_prefix('/') {
        return Parsed::Command(Command::Search(query.trim().to_string()));
    }
    let mut parts = input.split_whitespace();
    let Some(name) = parts.next() else {
        return Parsed::Empty;
    };
    let arg = parts.next();
    match name {
        "q" | "quit" => Parsed::Command(Command::Quit),
        "import" | "add" => {
            let path = input.trim_start().split_once(char::is_whitespace);
            match path.map(|(_, rest)| rest.trim()) {
                Some(path) if !path.is_empty() => {
                    Parsed::Command(Command::Import(path.to_string()))
                }
                _ => Parsed::Invalid("usage: :import <file or directory>".to_string()),
            }
        }
        "open" => {
            let value = input.trim_start().split_once(char::is_whitespace);
            match value.map(|(_, rest)| rest.trim()) {
                Some(value) if !value.is_empty() => Parsed::Command(Command::Open(value.into())),
                _ => Parsed::Invalid("usage: :open frid://<content_id>".to_string()),
            }
        }
        "connect" => {
            let value = input.trim_start().split_once(char::is_whitespace);
            match value.map(|(_, rest)| rest.trim()) {
                Some(value) if !value.is_empty() => {
                    Parsed::Command(Command::ConnectInvite(value.into()))
                }
                _ => Parsed::Invalid("usage: :connect frid://i/<invite>".to_string()),
            }
        }
        "volume" | "vol" => match arg.and_then(|a| a.parse::<u8>().ok()) {
            Some(value) if value <= 100 => Parsed::Command(Command::Volume(value)),
            _ => Parsed::Invalid("usage: :volume 0-100".to_string()),
        },
        "seek" => match arg.map(parse_seek_arg) {
            Some(Some(command)) => Parsed::Command(command),
            _ => Parsed::Invalid("usage: :seek +30 | -10 | 90 | 1:30".to_string()),
        },
        "shuffle" => Parsed::Command(Command::Shuffle),
        "repeat" => match arg {
            None => Parsed::Command(Command::Repeat(None)),
            Some("off") => Parsed::Command(Command::Repeat(Some(RepeatArg::Off))),
            Some("one") => Parsed::Command(Command::Repeat(Some(RepeatArg::One))),
            Some("all") => Parsed::Command(Command::Repeat(Some(RepeatArg::All))),
            Some(_) => Parsed::Invalid("usage: :repeat [off|one|all]".to_string()),
        },
        "clear" => Parsed::Command(Command::ClearQueue),
        "next" => Parsed::Command(Command::Next),
        "prev" => Parsed::Command(Command::Prev),
        "pause" | "play" => Parsed::Command(Command::PlayPause),
        "help" => Parsed::Command(Command::Help),
        "logs" => match arg {
            None => Parsed::Command(Command::Logs(None)),
            Some(level) => match ["error", "warn", "info", "debug", "trace"]
                .iter()
                .position(|l| *l == level)
            {
                Some(index) => Parsed::Command(Command::Logs(Some(index))),
                None => Parsed::Invalid("usage: :logs [error|warn|info|debug|trace]".to_string()),
            },
        },
        _ => Parsed::Unknown(name.to_string()),
    }
}

/// `+30`/`-10` → relative; `90` or `1:30` → absolute.
fn parse_seek_arg(arg: &str) -> Option<Command> {
    if let Some(rest) = arg.strip_prefix('+') {
        return rest.parse::<i64>().ok().map(Command::Seek);
    }
    if let Some(rest) = arg.strip_prefix('-') {
        return rest.parse::<i64>().ok().map(|s| Command::Seek(-s));
    }
    if let Some((minutes, seconds)) = arg.split_once(':') {
        let minutes = minutes.parse::<u64>().ok()?;
        let seconds = seconds.parse::<u64>().ok()?;
        if seconds >= 60 {
            return None;
        }
        return Some(Command::SeekTo(minutes * 60 + seconds));
    }
    arg.parse::<u64>().ok().map(Command::SeekTo)
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
    fn parses_word_commands() {
        assert_eq!(parse("q"), Parsed::Command(Command::Quit));
        assert_eq!(parse("quit"), Parsed::Command(Command::Quit));
        assert_eq!(
            parse("import ~/Music/My Album"),
            Parsed::Command(Command::Import("~/Music/My Album".to_string()))
        );
        assert_eq!(
            parse(
                "open frid://b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef?t=A-B"
            ),
            Parsed::Command(Command::Open(
                "frid://b3:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef?t=A-B"
                    .to_string()
            ))
        );
        assert!(matches!(parse("import"), Parsed::Invalid(_)));
        assert!(matches!(parse("open"), Parsed::Invalid(_)));
        assert_eq!(
            parse("connect frid://i/abcd"),
            Parsed::Command(Command::ConnectInvite("frid://i/abcd".to_string()))
        );
        assert_eq!(parse("volume 40"), Parsed::Command(Command::Volume(40)));
        assert_eq!(parse("vol 0"), Parsed::Command(Command::Volume(0)));
        assert_eq!(parse("shuffle"), Parsed::Command(Command::Shuffle));
        assert_eq!(parse("repeat"), Parsed::Command(Command::Repeat(None)));
        assert_eq!(
            parse("repeat all"),
            Parsed::Command(Command::Repeat(Some(RepeatArg::All)))
        );
        assert_eq!(parse("clear"), Parsed::Command(Command::ClearQueue));
        assert_eq!(parse("logs debug"), Parsed::Command(Command::Logs(Some(3))));
    }

    #[test]
    fn parses_seek_forms() {
        assert_eq!(parse("seek +30"), Parsed::Command(Command::Seek(30)));
        assert_eq!(parse("seek -10"), Parsed::Command(Command::Seek(-10)));
        assert_eq!(parse("seek 90"), Parsed::Command(Command::SeekTo(90)));
        assert_eq!(parse("seek 1:30"), Parsed::Command(Command::SeekTo(90)));
        assert!(matches!(parse("seek"), Parsed::Invalid(_)));
        assert!(matches!(parse("seek 1:75"), Parsed::Invalid(_)));
    }

    #[test]
    fn invalid_and_unknown() {
        assert_eq!(parse(""), Parsed::Empty);
        assert!(matches!(parse("volume 150"), Parsed::Invalid(_)));
        assert!(matches!(parse("volume"), Parsed::Invalid(_)));
        assert_eq!(
            parse("frobnicate"),
            Parsed::Unknown("frobnicate".to_string())
        );
    }

    #[test]
    fn search_is_live() {
        assert!(is_live(&Command::Search("x".into())));
        assert!(!is_live(&Command::Quit));
    }
}
