//! Host-side smart actions the pad asks for: `kb.sa.openapp {path}`,
//! `openurl {url}`, `inserttext {text}`; `exec {cmd}` stays off. Port of
//! `input/smartactions.py`.
//!
//! The payload comes from the pad, a trust boundary: every argument goes to
//! `open` as one argv element, never through a shell.

use std::process::{Command, Stdio};

use serde_json::Value;

use super::actions::repr;

pub const PREFIX: &str = "kb.sa.";

/// The URL scheme as `urlsplit` sees it (`""` when there is none).
fn scheme(url: &str) -> String {
    let Some((head, _)) = url.split_once(':') else {
        return String::new();
    };
    let mut chars = head.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
    if valid {
        head.to_lowercase()
    } else {
        String::new()
    }
}

/// Run `message` if it is a smart action; return a log line, else `None`.
/// `opener` runs `open <arg>`.
pub fn handle_smart_action(
    message: &Value,
    type_text: &mut dyn FnMut(&str),
    opener: &mut dyn FnMut(&[String]),
) -> Option<String> {
    let method = message
        .get("m")
        .filter(|v| !v.is_null())
        .or_else(|| message.get("method"))
        .and_then(Value::as_str)?;
    let action = method.strip_prefix(PREFIX)?;
    let params = message
        .get("p")
        .filter(|v| !v.is_null())
        .or_else(|| message.get("params"));
    let Some(params) = params.and_then(Value::as_object) else {
        return Some(format!("smart action {method}: ignored (no parameters)"));
    };
    let field = match action {
        "openapp" => "path",
        "openurl" => "url",
        "inserttext" => "text",
        "exec" => "cmd",
        _ => "",
    };
    let value = match params.get(field) {
        Some(Value::String(v)) if !v.is_empty() => v.as_str(),
        _ => return Some(format!("smart action {method}: ignored (unknown or empty)")),
    };
    Some(match action {
        "openapp" => {
            opener(&["open".to_string(), value.to_string()]);
            format!("smart action: opened {value}")
        }
        "openurl" => {
            let url = if value.to_lowercase().starts_with("http") {
                value.to_string()
            } else {
                format!("https://{value}")
            };
            if !matches!(scheme(&url).as_str(), "http" | "https") {
                return Some(format!(
                    "smart action: refused non-http URL {}",
                    repr(value)
                ));
            }
            opener(&["open".to_string(), url.clone()]);
            format!("smart action: opened {url}")
        }
        "inserttext" => {
            type_text(value);
            format!("smart action: typed {} characters", value.chars().count())
        }
        _ => format!(
            "smart action: ignored exec {} (shell commands are off)",
            repr(value)
        ),
    })
}

/// Spawn `argv` detached, output discarded. A thread reaps the child.
pub fn open_detached(argv: &[String]) {
    let Some((program, args)) = argv.split_first() else {
        return;
    };
    if let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(message: Value) -> (Option<String>, Vec<String>, Vec<Vec<String>>) {
        let mut typed = Vec::new();
        let mut opened = Vec::new();
        let line = handle_smart_action(&message, &mut |t| typed.push(t.to_string()), &mut |argv| {
            opened.push(argv.to_vec())
        });
        (line, typed, opened)
    }

    #[test]
    fn smart_actions_open_type_and_refuse() {
        let open = |arg: &str| vec![vec!["open".to_string(), arg.to_string()]];
        let cases: Vec<(Value, Vec<String>, Vec<Vec<String>>)> = vec![
            (
                json!({"m": "kb.sa.openapp", "p": {"path": "/Applications/Mail.app"}}),
                vec![],
                open("/Applications/Mail.app"),
            ),
            (
                json!({"m": "kb.sa.openurl", "p": {"url": "example.com"}}),
                vec![],
                open("https://example.com"),
            ),
            (
                json!({"method": "kb.sa.openurl", "params": {"url": "HTTP://x.org"}}),
                vec![],
                open("HTTP://x.org"),
            ),
            (
                json!({"m": "kb.sa.openurl", "p": {"url": "httpfoo"}}),
                vec![],
                vec![],
            ),
            (
                json!({"m": "kb.sa.inserttext", "p": {"text": "hi"}}),
                vec!["hi".to_string()],
                vec![],
            ),
            (
                json!({"m": "kb.sa.exec", "p": {"cmd": "rm -rf /"}}),
                vec![],
                vec![],
            ),
            (json!({"m": "kb.sa.openapp", "p": {}}), vec![], vec![]),
            (json!({"m": "kb.sa.openapp"}), vec![], vec![]),
        ];
        for (message, typed, opened) in cases {
            let (line, t, o) = run(message.clone());
            assert!(line.is_some(), "{message}");
            assert_eq!((t, o), (typed, opened), "{message}");
        }
        assert_eq!(run(json!({"m": "v.oai.hid", "p": {}})).0, None);
    }
}
