//! Hotkey parsing and resolution.
//!
//! A hotkey is written as a plus separated list of modifier names followed by
//! exactly one key, for example "alt+1", "alt+backspace" or "ctrl+alt+h".
//! The backtick key may be written as a literal backtick, as "grave" or as
//! "backtick".

use std::collections::HashMap;
use std::fmt;

use crate::config::Config;

/// The non modifier part of a hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A printable ASCII character, lower cased.
    Char(char),
    Backspace,
    Tab,
    Enter,
    Escape,
    Space,
    Delete,
    Insert,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

impl Key {
    /// Canonical name used in error messages and the settings UI.
    pub fn name(self) -> String {
        match self {
            Key::Char(c) => c.to_string(),
            Key::Backspace => "backspace".to_string(),
            Key::Tab => "tab".to_string(),
            Key::Enter => "enter".to_string(),
            Key::Escape => "escape".to_string(),
            Key::Space => "space".to_string(),
            Key::Delete => "delete".to_string(),
            Key::Insert => "insert".to_string(),
            Key::Left => "left".to_string(),
            Key::Right => "right".to_string(),
            Key::Up => "up".to_string(),
            Key::Down => "down".to_string(),
            Key::Home => "home".to_string(),
            Key::End => "end".to_string(),
            Key::PageUp => "pageup".to_string(),
            Key::PageDown => "pagedown".to_string(),
            Key::F(number) => format!("f{number}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    pub win: bool,
}

/// A fully specified hotkey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hotkey {
    pub mods: Modifiers,
    pub key: Key,
}

impl Hotkey {
    pub fn new(mods: Modifiers, key: Key) -> Self {
        Self { mods, key }
    }

    pub fn alt(key: Key) -> Self {
        Self {
            mods: Modifiers {
                alt: true,
                ..Modifiers::default()
            },
            key,
        }
    }

    /// Human readable form, e.g. "Alt + Backspace".
    pub fn display(&self) -> String {
        let mut parts = Vec::new();
        if self.mods.ctrl {
            parts.push("Ctrl".to_string());
        }
        if self.mods.alt {
            parts.push("Alt".to_string());
        }
        if self.mods.shift {
            parts.push("Shift".to_string());
        }
        if self.mods.win {
            parts.push("Win".to_string());
        }
        parts.push(display_key(self.key));
        parts.join(" + ")
    }
}

fn display_key(key: Key) -> String {
    match key {
        Key::Char(' ') => "Space".to_string(),
        Key::Char(c) => c.to_string(),
        Key::F(number) => format!("F{number}"),
        other => {
            let name = other.name();
            let mut chars = name.chars();
            let first = chars.next().unwrap_or('?').to_ascii_uppercase();
            format!("{first}{}", chars.as_str())
        }
    }
}

/// The slot keys that are bound to Alt by default: Alt+1 .. Alt+0, Alt+-, Alt+=.
pub const SLOT_KEYS: [&str; 12] = [
    "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "-", "=",
];

pub fn is_slot_key(key: &str) -> bool {
    SLOT_KEYS.contains(&key)
}

/// Alt + slot key, or None when the key is not a slot key.
pub fn slot_hotkey(key: &str) -> Option<Hotkey> {
    if is_slot_key(key) {
        key.chars().next().map(|c| Hotkey::alt(Key::Char(c)))
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("hotkey is empty")]
    Empty,
    #[error("hotkey '{0}' must contain at least one modifier such as alt or ctrl")]
    NoModifier(String),
    #[error("unknown key '{0}'")]
    UnknownKey(String),
    #[error("hotkey '{0}' is bound more than once")]
    Duplicate(String),
}

/// Parse "alt+1", "alt+backspace", "ctrl+shift+h", ...
pub fn parse_hotkey(input: &str) -> Result<Hotkey, KeyError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(KeyError::Empty);
    }
    let parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
    if parts.len() < 2 {
        return Err(KeyError::NoModifier(trimmed.to_string()));
    }
    let mut mods = Modifiers::default();
    for part in &parts[..parts.len() - 1] {
        match part.to_ascii_lowercase().as_str() {
            "alt" | "option" | "opt" => mods.alt = true,
            "ctrl" | "control" => mods.ctrl = true,
            "shift" => mods.shift = true,
            "win" | "super" | "meta" | "windows" => mods.win = true,
            other => return Err(KeyError::UnknownKey(other.to_string())),
        }
    }
    if !(mods.alt || mods.ctrl || mods.shift || mods.win) {
        return Err(KeyError::NoModifier(trimmed.to_string()));
    }
    let key = parse_key(parts[parts.len() - 1])?;
    Ok(Hotkey { mods, key })
}

fn parse_key(token: &str) -> Result<Key, KeyError> {
    let token = token.trim();
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_graphic() || c == ' ' {
            return Ok(Key::Char(c.to_ascii_lowercase()));
        }
    }
    match token.to_ascii_lowercase().as_str() {
        "backspace" | "bksp" | "bs" => Ok(Key::Backspace),
        "tab" => Ok(Key::Tab),
        "enter" | "return" => Ok(Key::Enter),
        "esc" | "escape" => Ok(Key::Escape),
        "space" | "spacebar" => Ok(Key::Space),
        "delete" | "del" => Ok(Key::Delete),
        "insert" | "ins" => Ok(Key::Insert),
        "left" | "arrowleft" => Ok(Key::Left),
        "right" | "arrowright" => Ok(Key::Right),
        "up" | "arrowup" => Ok(Key::Up),
        "down" | "arrowdown" => Ok(Key::Down),
        "home" => Ok(Key::Home),
        "end" => Ok(Key::End),
        "pageup" | "pgup" => Ok(Key::PageUp),
        "pagedown" | "pgdn" => Ok(Key::PageDown),
        "grave" | "backtick" | "tilde" => Ok(Key::Char('\u{60}')),
        other => {
            if let Some(number) = other.strip_prefix('f') {
                if let Ok(number) = number.parse::<u8>() {
                    if (1..=24).contains(&number) {
                        return Ok(Key::F(number));
                    }
                }
            }
            Err(KeyError::UnknownKey(token.to_string()))
        }
    }
}

/// What a hotkey does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// Open the selectable snippet list.
    List,
    /// Open the configuration menu.
    Config,
    /// Insert the snippet at this index in Config::snippet.
    Snippet(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapError {
    pub context: String,
    pub error: KeyError,
}

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.context, self.error)
    }
}

/// Immutable mapping of hotkeys to actions.
#[derive(Debug, Clone)]
pub struct Keymap {
    list: Hotkey,
    config: Hotkey,
    slots: Vec<(Hotkey, usize)>,
    by_key: HashMap<Hotkey, Binding>,
}

impl Keymap {
    pub fn build(config: &Config) -> Result<Self, Vec<KeymapError>> {
        let mut errors = Vec::new();

        let list = parse_hotkey(&config.hotkeys.list).map_err(|error| KeymapError {
            context: "hotkeys.list".to_string(),
            error,
        });
        let config_hotkey = parse_hotkey(&config.hotkeys.config).map_err(|error| KeymapError {
            context: "hotkeys.config".to_string(),
            error,
        });

        let list = match list {
            Ok(hotkey) => hotkey,
            Err(error) => {
                errors.push(error);
                Hotkey::alt(Key::Char('\u{60}'))
            }
        };
        let config_hotkey = match config_hotkey {
            Ok(hotkey) => hotkey,
            Err(error) => {
                errors.push(error);
                Hotkey::alt(Key::Backspace)
            }
        };

        let mut slots: Vec<(Hotkey, usize)> = Vec::new();
        let mut seen: HashMap<Hotkey, String> = HashMap::new();
        seen.insert(list, "hotkeys.list".to_string());
        seen.insert(config_hotkey, "hotkeys.config".to_string());

        for (index, snippet) in config.snippet.iter().enumerate() {
            if !snippet.enabled || snippet.key.is_empty() {
                continue;
            }
            if !is_slot_key(&snippet.key) {
                errors.push(KeymapError {
                    context: format!("snippet #{}", index + 1),
                    error: KeyError::UnknownKey(snippet.key.clone()),
                });
                continue;
            }
            let Some(hotkey) = slot_hotkey(&snippet.key) else {
                continue;
            };
            let label = format!("snippet #{} ({})", index + 1, snippet.key);
            if let Some(existing) = seen.insert(hotkey, label.clone()) {
                errors.push(KeymapError {
                    context: label,
                    error: KeyError::Duplicate(format!(
                        "{} conflicts with {}",
                        hotkey.display(),
                        existing
                    )),
                });
                continue;
            }
            slots.push((hotkey, index));
        }

        if !errors.is_empty() {
            return Err(errors);
        }

        let mut by_key = HashMap::new();
        by_key.insert(list, Binding::List);
        by_key.insert(config_hotkey, Binding::Config);
        for (hotkey, index) in &slots {
            by_key.insert(*hotkey, Binding::Snippet(*index));
        }

        Ok(Self {
            list,
            config: config_hotkey,
            slots,
            by_key,
        })
    }

    pub fn resolve(&self, hotkey: &Hotkey) -> Option<Binding> {
        self.by_key.get(hotkey).copied()
    }

    pub fn list_hotkey(&self) -> Hotkey {
        self.list
    }

    pub fn config_hotkey(&self) -> Hotkey {
        self.config
    }

    pub fn snippet_bindings(&self) -> &[(Hotkey, usize)] {
        &self.slots
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Snippet;

    fn alt(c: char) -> Hotkey {
        Hotkey::alt(Key::Char(c))
    }

    #[test]
    fn parses_common_hotkeys() {
        assert_eq!(parse_hotkey("alt+1").unwrap(), alt('1'));
        assert_eq!(parse_hotkey("ALT+1").unwrap(), alt('1'));
        assert_eq!(
            parse_hotkey("alt+backspace").unwrap(),
            Hotkey::alt(Key::Backspace)
        );
        assert_eq!(
            parse_hotkey("ctrl + shift + h").unwrap(),
            Hotkey {
                mods: Modifiers {
                    alt: false,
                    ctrl: true,
                    shift: true,
                    win: false
                },
                key: Key::Char('h')
            }
        );
        assert_eq!(
            parse_hotkey("alt+grave").unwrap(),
            Hotkey::alt(Key::Char('\u{60}'))
        );
        assert_eq!(parse_hotkey("alt+=").unwrap(), alt('='));
        assert_eq!(parse_hotkey("alt+-").unwrap(), alt('-'));
        assert_eq!(parse_hotkey("alt+f5").unwrap(), Hotkey::alt(Key::F(5)));
    }

    #[test]
    fn rejects_bad_hotkeys() {
        assert!(matches!(parse_hotkey(""), Err(KeyError::Empty)));
        assert!(matches!(parse_hotkey("1"), Err(KeyError::NoModifier(_))));
        assert!(matches!(
            parse_hotkey("hyper+1"),
            Err(KeyError::UnknownKey(_))
        ));
        assert!(matches!(
            parse_hotkey("alt+nope"),
            Err(KeyError::UnknownKey(_))
        ));
        assert!(matches!(parse_hotkey("alt+f99"), Err(KeyError::UnknownKey(_))));
    }

    #[test]
    fn builds_default_keymap() {
        let keymap = Keymap::build(&Config::example()).unwrap();
        assert_eq!(
            keymap.resolve(&Hotkey::alt(Key::Char('\u{60}'))),
            Some(Binding::List)
        );
        assert_eq!(
            keymap.resolve(&Hotkey::alt(Key::Backspace)),
            Some(Binding::Config)
        );
        assert_eq!(
            keymap.resolve(&Hotkey::alt(Key::Char('1'))),
            Some(Binding::Snippet(0))
        );
        assert_eq!(
            keymap.resolve(&Hotkey::alt(Key::Char('2'))),
            Some(Binding::Snippet(1))
        );
        assert_eq!(keymap.resolve(&Hotkey::alt(Key::Char('9'))), None);
    }

    #[test]
    fn slot_keys_cover_alt_row() {
        for key in SLOT_KEYS {
            assert!(slot_hotkey(key).is_some(), "missing slot {key}");
            assert!(slot_hotkey(key).unwrap().mods.alt);
        }
        assert!(slot_hotkey("x").is_none());
    }

    #[test]
    fn duplicate_slots_are_rejected() {
        let mut config = Config::default();
        config.snippet = vec![Snippet::new("1", "a", "a"), Snippet::new("1", "b", "b")];
        let errors = Keymap::build(&config).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| matches!(e.error, KeyError::Duplicate(_))));
    }

    #[test]
    fn hotkey_colliding_with_menu_is_rejected() {
        let mut config = Config::default();
        config.snippet = vec![Snippet::new("1", "a", "a")];
        config.hotkeys.list = "alt+1".to_string();
        let errors = Keymap::build(&config).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| matches!(e.error, KeyError::Duplicate(_))));
    }
}
