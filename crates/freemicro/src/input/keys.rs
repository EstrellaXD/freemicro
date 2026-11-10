//! Key-combo names to macOS virtual keycodes and modifier flags. Pure. Port
//! of `input/keys.py`.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyNameError(pub String);

impl std::fmt::Display for KeyNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KeyNameError {}

/// In canonical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Modifier {
    Command,
    Control,
    Option,
    Shift,
    Fn,
}

impl Modifier {
    /// AppleScript's word for it (`"command"`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            Modifier::Command => "command",
            Modifier::Control => "control",
            Modifier::Option => "option",
            Modifier::Shift => "shift",
            Modifier::Fn => "fn",
        }
    }
}

/// Modifier spellings we accept.
pub const MODIFIER_ALIASES: [(&str, Modifier); 13] = [
    ("cmd", Modifier::Command),
    ("command", Modifier::Command),
    ("meta", Modifier::Command),
    ("super", Modifier::Command),
    ("win", Modifier::Command),
    ("ctrl", Modifier::Control),
    ("control", Modifier::Control),
    ("alt", Modifier::Option),
    ("opt", Modifier::Option),
    ("option", Modifier::Option),
    ("shift", Modifier::Shift),
    ("fn", Modifier::Fn),
    ("function", Modifier::Fn),
];

/// Virtual key codes for keys with no printable character.
pub const KEY_CODES: [(&str, u16); 30] = [
    ("return", 36),
    ("enter", 76),
    ("tab", 48),
    ("space", 49),
    ("delete", 51),
    ("backspace", 51),
    ("forward-delete", 117),
    ("escape", 53),
    ("esc", 53),
    ("help", 114),
    ("home", 115),
    ("end", 119),
    ("page-up", 116),
    ("page-down", 121),
    ("left", 123),
    ("right", 124),
    ("down", 125),
    ("up", 126),
    ("f1", 122),
    ("f2", 120),
    ("f3", 99),
    ("f4", 118),
    ("f5", 96),
    ("f6", 97),
    ("f7", 98),
    ("f8", 100),
    ("f9", 101),
    ("f10", 109),
    ("f11", 103),
    ("f12", 111),
];

/// Names for printable keys that would collide with the `-`/`+` separators.
pub const LITERAL_ALIASES: [(&str, char); 12] = [
    ("minus", '-'),
    ("hyphen", '-'),
    ("plus", '+'),
    ("equals", '='),
    ("comma", ','),
    ("period", '.'),
    ("slash", '/'),
    ("backslash", '\\'),
    ("semicolon", ';'),
    ("quote", '\''),
    ("grave", '`'),
    ("backtick", '`'),
];

/// US-layout virtual key codes for printable characters (CGEvent only).
pub const CHAR_KEY_CODES: [(char, u16); 47] = [
    ('a', 0),
    ('s', 1),
    ('d', 2),
    ('f', 3),
    ('h', 4),
    ('g', 5),
    ('z', 6),
    ('x', 7),
    ('c', 8),
    ('v', 9),
    ('b', 11),
    ('q', 12),
    ('w', 13),
    ('e', 14),
    ('r', 15),
    ('y', 16),
    ('t', 17),
    ('1', 18),
    ('2', 19),
    ('3', 20),
    ('4', 21),
    ('6', 22),
    ('5', 23),
    ('=', 24),
    ('9', 25),
    ('7', 26),
    ('-', 27),
    ('8', 28),
    ('0', 29),
    (']', 30),
    ('o', 31),
    ('u', 32),
    ('[', 33),
    ('i', 34),
    ('p', 35),
    ('l', 37),
    ('j', 38),
    ('\'', 39),
    ('k', 40),
    (';', 41),
    ('\\', 42),
    (',', 43),
    ('/', 44),
    ('n', 45),
    ('m', 46),
    ('.', 47),
    ('`', 50),
];

fn key_code(name: &str) -> Option<u16> {
    KEY_CODES.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

fn literal(name: &str) -> Option<char> {
    LITERAL_ALIASES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c)
}

fn is_key_name(name: &str) -> bool {
    key_code(name).is_some() || literal(name).is_some()
}

pub fn known_key_names() -> Vec<String> {
    let mut names: Vec<String> = KEY_CODES
        .iter()
        .map(|(n, _)| n.to_string())
        .chain(LITERAL_ALIASES.iter().map(|(n, _)| n.to_string()))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Split `cmd+shift-k` on either separator; a hyphenated key name
/// (`page-up`) wins over splitting, whole or as the tail.
fn split(combo: &str) -> Vec<String> {
    let trimmed = combo.trim();
    let whole = trimmed.to_lowercase();
    if is_key_name(&whole) {
        return vec![whole];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut token = String::new();
    for ch in trimmed.chars() {
        if (ch == '+' || ch == '-') && !token.is_empty() {
            parts.push(std::mem::take(&mut token));
        } else {
            token.push(ch);
        }
    }
    if !token.is_empty() {
        parts.push(token);
    }
    if parts.len() >= 2 {
        for start in (0..=parts.len() - 2).rev() {
            let tail = parts[start..].join("-").to_lowercase();
            if is_key_name(&tail) {
                let mut out = parts[..start].to_vec();
                out.push(tail);
                return out;
            }
        }
    }
    parts
}

/// `"cmd+shift+k"` -> (sorted modifiers, key name). Aliases as Python.
pub fn parse_combo(combo: &str) -> Result<(Vec<Modifier>, String), KeyNameError> {
    if combo.trim().is_empty() {
        return Err(KeyNameError(
            "key must be a non-empty string, e.g. 'escape'".to_string(),
        ));
    }
    let mut parts = split(combo);
    let Some(base) = parts.pop() else {
        return Err(KeyNameError(format!("could not parse key combo '{combo}'")));
    };
    let mut modifiers = Vec::new();
    for raw in &parts {
        let lowered = raw.to_lowercase();
        let Some((_, modifier)) = MODIFIER_ALIASES.iter().find(|(n, _)| *n == lowered) else {
            let mut names: Vec<&str> = MODIFIER_ALIASES.iter().map(|(n, _)| *n).collect();
            names.sort();
            return Err(KeyNameError(format!(
                "unknown modifier '{raw}' in '{combo}'; expected one of {}",
                names.join(", ")
            )));
        };
        if !modifiers.contains(modifier) {
            modifiers.push(*modifier);
        }
    }
    let base = base.to_lowercase();
    if !is_key_name(&base) && base.chars().count() != 1 {
        return Err(KeyNameError(format!(
            "unknown key '{base}' in '{combo}'; use a single character or one of: {}",
            known_key_names().join(", ")
        )));
    }
    modifiers.sort();
    Ok((modifiers, base))
}

/// `combo` -> (virtual keycode, modifiers) for CGEvent.
pub fn cgevent_spec(combo: &str) -> Result<(u16, Vec<Modifier>), KeyNameError> {
    let (modifiers, base) = parse_combo(combo)?;
    if let Some(code) = key_code(&base) {
        return Ok((code, modifiers));
    }
    let ch = literal(&base).unwrap_or_else(|| base.chars().next().unwrap_or(' '));
    let lowered = ch.to_lowercase().next().unwrap_or(ch);
    match CHAR_KEY_CODES.iter().find(|(c, _)| *c == lowered) {
        Some((_, code)) => Ok((*code, modifiers)),
        None => Err(KeyNameError(format!(
            "no virtual key code for '{ch}' in '{combo}'; use a named key or type it \
             with a 'text' action instead"
        ))),
    }
}

pub fn escape_applescript(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `tell application "System Events" to key code ... using {...}`.
pub fn applescript_for(combo: &str) -> Result<String, KeyNameError> {
    let (modifiers, base) = parse_combo(combo)?;
    if modifiers.contains(&Modifier::Fn) {
        return Err(KeyNameError(format!(
            "'{combo}' uses fn, which AppleScript cannot express - macOS handles fn below \
             the System Events API. Use the CGEvent backend (the default when available) \
             for fn bindings."
        )));
    }
    let mut target = match key_code(&base) {
        Some(code) => format!("key code {code}"),
        None => {
            let ch = literal(&base).map(String::from).unwrap_or(base);
            format!("keystroke \"{}\"", escape_applescript(&ch))
        }
    };
    if !modifiers.is_empty() {
        let using: Vec<String> = modifiers
            .iter()
            .map(|m| format!("{} down", m.as_str()))
            .collect();
        target = format!("{target} using {{{}}}", using.join(", "));
    }
    Ok(format!("tell application \"System Events\" to {target}"))
}

pub fn applescript_for_text(text: &str) -> String {
    format!(
        "tell application \"System Events\" to keystroke \"{}\"",
        escape_applescript(text)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_combo_accepts_aliases_and_both_separators() {
        let cases: [(&str, &[Modifier], &str); 7] = [
            ("fn+f5", &[Modifier::Fn], "f5"),
            ("escape", &[], "escape"),
            ("ctrl-r", &[Modifier::Control], "r"),
            ("shift+cmd+K", &[Modifier::Command, Modifier::Shift], "k"),
            ("page-up", &[], "page-up"),
            ("cmd+page-up", &[Modifier::Command], "page-up"),
            ("ctrl+ctrl+a", &[Modifier::Control], "a"),
        ];
        for (combo, mods, base) in cases {
            let (m, b) = parse_combo(combo).unwrap_or_else(|e| panic!("{combo}: {e}"));
            assert_eq!((m.as_slice(), b.as_str()), (mods, base), "{combo}");
        }
    }

    #[test]
    fn parse_combo_rejects_bad_names() {
        for combo in ["", "  ", "fn", "hyper+a", "cmd+escapee"] {
            assert!(parse_combo(combo).is_err(), "{combo}");
        }
    }

    #[test]
    fn cgevent_spec_resolves_named_and_printable_keys() {
        assert_eq!(cgevent_spec("return").unwrap(), (36, vec![]));
        assert_eq!(
            cgevent_spec("ctrl+cmd+o").unwrap(),
            (31, vec![Modifier::Command, Modifier::Control])
        );
        assert_eq!(cgevent_spec("cmd+minus").unwrap().0, 27);
        assert!(cgevent_spec("é").is_err());
    }

    #[test]
    fn applescript_for_renders_key_code_or_keystroke() {
        assert_eq!(
            applescript_for("shift-tab").unwrap(),
            "tell application \"System Events\" to key code 48 using {shift down}"
        );
        assert_eq!(
            applescript_for("cmd+quote").unwrap(),
            "tell application \"System Events\" to keystroke \"'\" using {command down}"
        );
        assert!(applescript_for("fn+f5").is_err());
        assert_eq!(
            applescript_for_text("say \"hi\""),
            "tell application \"System Events\" to keystroke \"say \\\"hi\\\"\""
        );
    }
}
