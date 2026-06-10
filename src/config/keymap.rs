use std::{fs, path::PathBuf, str::FromStr};

use anyhow::{Context as _, Result, bail};
use crokey::{KeyCombination, KeyCombinationFormat, key};
use crossterm::event::{KeyCode, KeyModifiers};
use serde::Deserialize;

use crate::app::action::Action;

const DEFAULT_KEYMAP: &str = include_str!("default_keymap.toml");

/// Input context a binding applies to. `Global` bindings work everywhere;
/// view-specific bindings shadow global ones for the same key sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyContext {
    #[default]
    Global,
    Library,
    Search,
    Playlists,
    Queue,
    Devices,
}

impl KeyContext {
    pub fn label(self) -> &'static str {
        match self {
            KeyContext::Global => "global",
            KeyContext::Library => "library",
            KeyContext::Search => "search",
            KeyContext::Playlists => "playlists",
            KeyContext::Queue => "queue",
            KeyContext::Devices => "devices",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub keys: Vec<KeyCombination>,
    pub action: Action,
    pub context: KeyContext,
}

#[derive(Debug, Deserialize)]
struct RawBinding {
    key_sequence: String,
    command: Action,
    #[serde(default)]
    context: KeyContext,
}

#[derive(Debug, Default, Deserialize)]
struct KeymapFile {
    #[serde(default)]
    keymaps: Vec<RawBinding>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum KeyResolution {
    Action(Action),
    /// The pressed keys are a prefix of a longer sequence; the formatted
    /// pending chord is returned for display in the status bar.
    Pending(String),
    Unmatched,
}

pub struct Keymap {
    bindings: Vec<Binding>,
    pending: Vec<KeyCombination>,
    format: KeyCombinationFormat,
}

impl Keymap {
    /// Load defaults merged with the user's keymap.toml. A broken user file
    /// must not brick the app: it is ignored and reported as a warning.
    pub fn load() -> (Self, Option<String>) {
        let mut bindings =
            parse_bindings(DEFAULT_KEYMAP).expect("embedded default keymap must parse");
        let mut warning = None;
        if let Some(path) = user_keymap_path() {
            if path.exists() {
                match fs::read_to_string(&path)
                    .map_err(anyhow::Error::from)
                    .and_then(|text| parse_bindings(&text))
                {
                    Ok(user) => merge(&mut bindings, user),
                    Err(err) => {
                        warning = Some(format!(
                            "{} ignored: {err:#}; using default keybindings",
                            path.display()
                        ));
                    }
                }
            }
        }
        let keymap = Self {
            bindings,
            pending: Vec::new(),
            format: KeyCombinationFormat::default(),
        };
        (keymap, warning)
    }

    /// Feed one key combination; returns an action, a pending-chord state, or
    /// nothing. Esc clears a pending chord instead of resolving.
    pub fn resolve(&mut self, key: KeyCombination, context: KeyContext) -> KeyResolution {
        let key = self.localize(normalize(key));
        if key == key!(esc) && !self.pending.is_empty() {
            self.pending.clear();
            return KeyResolution::Unmatched;
        }
        self.pending.push(key);
        match self.lookup(context) {
            Lookup::Exact(action) => {
                self.pending.clear();
                KeyResolution::Action(action)
            }
            Lookup::Prefix => KeyResolution::Pending(self.format_pending()),
            Lookup::Nothing => {
                let retry = self.pending.len() > 1;
                self.pending.clear();
                if retry {
                    // The aborted chord's last key may start a new sequence.
                    self.resolve(key, context)
                } else {
                    KeyResolution::Unmatched
                }
            }
        }
    }

    /// All bindings as (keys, description, context) for the help view.
    pub fn help_entries(&self) -> Vec<(String, String, KeyContext)> {
        self.bindings
            .iter()
            .map(|b| (self.format_keys(&b.keys), b.action.describe(), b.context))
            .collect()
    }

    fn lookup(&self, context: KeyContext) -> Lookup {
        let mut exact_ctx: Option<&Binding> = None;
        let mut exact_global: Option<&Binding> = None;
        let mut has_prefix = false;
        for b in &self.bindings {
            if b.context != KeyContext::Global && b.context != context {
                continue;
            }
            if b.keys.len() < self.pending.len() || b.keys[..self.pending.len()] != self.pending {
                continue;
            }
            if b.keys.len() == self.pending.len() {
                if b.context == KeyContext::Global {
                    exact_global.get_or_insert(b);
                } else {
                    exact_ctx.get_or_insert(b);
                }
            } else {
                has_prefix = true;
            }
        }
        // An exact match fires immediately even if a longer sequence shares
        // the prefix — don't bind both "g" and "g g".
        if let Some(b) = exact_ctx.or(exact_global) {
            Lookup::Exact(b.action.clone())
        } else if has_prefix {
            Lookup::Prefix
        } else {
            Lookup::Nothing
        }
    }

    /// Layout fallback (vim langmap style): a Cyrillic key that no binding
    /// uses directly is translated to the Latin key in the same physical
    /// position (ЙЦУКЕН ↔ QWERTY), so bindings work in the Russian layout.
    /// Text input is unaffected — this runs only inside keymap resolution.
    fn localize(&self, key: KeyCombination) -> KeyCombination {
        let crokey::OneToThree::One(KeyCode::Char(c)) = key.codes else {
            return key;
        };
        let lower = c.to_lowercase().next().unwrap_or(c);
        let Some(latin) = qwerty_equivalent(lower) else {
            return key;
        };
        // A binding that mentions the Cyrillic key directly wins.
        if self.bindings.iter().any(|b| b.keys.contains(&key)) {
            return key;
        }
        let mapped = if c.is_uppercase() || key.modifiers.contains(KeyModifiers::SHIFT) {
            KeyCode::Char(latin.to_ascii_uppercase())
        } else {
            KeyCode::Char(latin)
        };
        normalize(KeyCombination::new(mapped, key.modifiers))
    }

    fn format_keys(&self, keys: &[KeyCombination]) -> String {
        keys.iter()
            .map(|k| self.format.to_string(*k))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn format_pending(&self) -> String {
        self.format_keys(&self.pending)
    }
}

enum Lookup {
    Exact(Action),
    Prefix,
    Nothing,
}

/// Terminals report SHIFT alongside symbol keys ('?', '+', ...) inconsistently.
/// Letters (of any alphabet) keep SHIFT (that is how "shift-g" works);
/// symbols drop it so a "?" binding matches everywhere.
fn normalize(key: KeyCombination) -> KeyCombination {
    if let crokey::OneToThree::One(KeyCode::Char(c)) = key.codes {
        if !c.is_alphabetic() && key.modifiers.contains(KeyModifiers::SHIFT) {
            return KeyCombination::new(KeyCode::Char(c), key.modifiers - KeyModifiers::SHIFT);
        }
    }
    key
}

/// The Latin character on the same physical key in the standard ЙЦУКЕН
/// layout (lowercase in, lowercase out).
fn qwerty_equivalent(c: char) -> Option<char> {
    Some(match c {
        'й' => 'q', 'ц' => 'w', 'у' => 'e', 'к' => 'r', 'е' => 't',
        'н' => 'y', 'г' => 'u', 'ш' => 'i', 'щ' => 'o', 'з' => 'p',
        'х' => '[', 'ъ' => ']',
        'ф' => 'a', 'ы' => 's', 'в' => 'd', 'а' => 'f', 'п' => 'g',
        'р' => 'h', 'о' => 'j', 'л' => 'k', 'д' => 'l', 'ж' => ';',
        'э' => '\'',
        'я' => 'z', 'ч' => 'x', 'с' => 'c', 'м' => 'v', 'и' => 'b',
        'т' => 'n', 'ь' => 'm', 'б' => ',', 'ю' => '.',
        'ё' => '`',
        _ => return None,
    })
}

pub fn user_keymap_path() -> Option<PathBuf> {
    crate::config::project_dirs().map(|dirs| dirs.config_dir().join("keymap.toml"))
}

fn parse_bindings(text: &str) -> Result<Vec<Binding>> {
    let file: KeymapFile = toml::from_str(text).context("invalid TOML")?;
    file.keymaps
        .into_iter()
        .map(|raw| {
            let keys = parse_sequence(&raw.key_sequence)
                .with_context(|| format!("bad key_sequence {:?}", raw.key_sequence))?;
            Ok(Binding {
                keys,
                action: raw.command,
                context: raw.context,
            })
        })
        .collect()
}

fn parse_sequence(s: &str) -> Result<Vec<KeyCombination>> {
    let keys: Vec<KeyCombination> = s
        .split_whitespace()
        .map(parse_chord)
        .collect::<Result<_>>()?;
    if keys.is_empty() {
        bail!("empty key sequence");
    }
    Ok(keys)
}

fn parse_chord(chord: &str) -> Result<KeyCombination> {
    // crokey only parses single-byte characters; non-ASCII keys (Cyrillic
    // bindings) are built directly.
    let mut chars = chord.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if !c.is_ascii() {
            let modifiers = if c.is_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            return Ok(KeyCombination::new(KeyCode::Char(c), modifiers));
        }
    }
    KeyCombination::from_str(chord)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .map(normalize)
}

fn merge(bindings: &mut Vec<Binding>, user: Vec<Binding>) {
    for b in user {
        bindings.retain(|d| !(d.keys == b.keys && d.context == b.context));
        bindings.push(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crokey::key;

    fn keymap_from(toml: &str) -> Keymap {
        Keymap {
            bindings: parse_bindings(toml).unwrap(),
            pending: Vec::new(),
            format: KeyCombinationFormat::default(),
        }
    }

    #[test]
    fn default_keymap_parses() {
        let bindings = parse_bindings(DEFAULT_KEYMAP).unwrap();
        assert!(bindings.len() > 20);
    }

    #[test]
    fn single_key_resolves() {
        let mut km = keymap_from(DEFAULT_KEYMAP);
        assert_eq!(
            km.resolve(key!(q), KeyContext::Library),
            KeyResolution::Action(Action::Quit)
        );
    }

    #[test]
    fn chord_sequence_resolves() {
        let mut km = keymap_from(DEFAULT_KEYMAP);
        assert!(matches!(
            km.resolve(key!(g), KeyContext::Library),
            KeyResolution::Pending(_)
        ));
        assert_eq!(
            km.resolve(key!(g), KeyContext::Library),
            KeyResolution::Action(Action::SelectFirst)
        );
    }

    #[test]
    fn aborted_chord_retries_last_key() {
        let mut km = keymap_from(DEFAULT_KEYMAP);
        km.resolve(key!(g), KeyContext::Library);
        assert_eq!(
            km.resolve(key!(q), KeyContext::Library),
            KeyResolution::Action(Action::Quit)
        );
    }

    #[test]
    fn esc_clears_pending_chord() {
        let mut km = keymap_from(DEFAULT_KEYMAP);
        km.resolve(key!(g), KeyContext::Library);
        assert_eq!(
            km.resolve(key!(esc), KeyContext::Library),
            KeyResolution::Unmatched
        );
        // Esc with no pending chord is a normal binding (Back).
        assert_eq!(
            km.resolve(key!(esc), KeyContext::Library),
            KeyResolution::Action(Action::Back)
        );
    }

    #[test]
    fn context_binding_shadows_global() {
        let mut km = keymap_from(
            r#"
            [[keymaps]]
            key_sequence = "n"
            command = "NextTrack"

            [[keymaps]]
            key_sequence = "n"
            command = "MoveDown"
            context = "search"
            "#,
        );
        assert_eq!(
            km.resolve(key!(n), KeyContext::Search),
            KeyResolution::Action(Action::MoveDown)
        );
        assert_eq!(
            km.resolve(key!(n), KeyContext::Library),
            KeyResolution::Action(Action::NextTrack)
        );
    }

    #[test]
    fn user_binding_overrides_default() {
        let mut bindings = parse_bindings(DEFAULT_KEYMAP).unwrap();
        let user = parse_bindings(
            r#"
            [[keymaps]]
            key_sequence = "q"
            command = "Back"
            "#,
        )
        .unwrap();
        merge(&mut bindings, user);
        let mut km = Keymap {
            bindings,
            pending: Vec::new(),
            format: KeyCombinationFormat::default(),
        };
        assert_eq!(
            km.resolve(key!(q), KeyContext::Library),
            KeyResolution::Action(Action::Back)
        );
    }

    #[test]
    fn shift_symbol_normalizes() {
        let mut km = keymap_from(DEFAULT_KEYMAP);
        let question_with_shift =
            KeyCombination::new(KeyCode::Char('?'), KeyModifiers::SHIFT);
        assert_eq!(
            km.resolve(question_with_shift, KeyContext::Library),
            KeyResolution::Action(Action::ToggleHelp)
        );
    }

    #[test]
    fn russian_layout_maps_to_physical_keys() {
        let mut km = keymap_from(DEFAULT_KEYMAP);
        // physical J → 'о' in ЙЦУКЕН
        let o = KeyCombination::new(KeyCode::Char('о'), KeyModifiers::NONE);
        assert_eq!(
            km.resolve(o, KeyContext::Library),
            KeyResolution::Action(Action::MoveDown)
        );
        // physical Shift+G → 'П'
        let cap_pe = KeyCombination::new(KeyCode::Char('П'), KeyModifiers::SHIFT);
        assert_eq!(
            km.resolve(cap_pe, KeyContext::Library),
            KeyResolution::Action(Action::SelectLast)
        );
        // chord: 'п п' = physical "g g"
        let pe = KeyCombination::new(KeyCode::Char('п'), KeyModifiers::NONE);
        assert!(matches!(
            km.resolve(pe, KeyContext::Library),
            KeyResolution::Pending(_)
        ));
        assert_eq!(
            km.resolve(pe, KeyContext::Library),
            KeyResolution::Action(Action::SelectFirst)
        );
        // punctuation positions: 'ю' sits on the '.' key (SeekForward)
        let yu = KeyCombination::new(KeyCode::Char('ю'), KeyModifiers::NONE);
        assert_eq!(
            km.resolve(yu, KeyContext::Library),
            KeyResolution::Action(Action::SeekForward { seconds: 10 })
        );
    }

    #[test]
    fn explicit_cyrillic_binding_wins_over_layout_fallback() {
        let mut km = keymap_from(
            r#"
            [[keymaps]]
            key_sequence = "о"
            command = "Quit"
            "#,
        );
        let o = KeyCombination::new(KeyCode::Char('о'), KeyModifiers::NONE);
        assert_eq!(
            km.resolve(o, KeyContext::Library),
            KeyResolution::Action(Action::Quit)
        );
    }

    #[test]
    fn parameterized_command_parses() {
        let mut km = keymap_from(DEFAULT_KEYMAP);
        let dot: KeyCombination = ".".parse().unwrap();
        assert_eq!(
            km.resolve(dot, KeyContext::Library),
            KeyResolution::Action(Action::SeekForward { seconds: 10 })
        );
    }
}
