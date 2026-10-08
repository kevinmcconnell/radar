//! `radar configure claude` and `radar remove claude`: point Claude Code's
//! status line at `radar-claude-statusline`, or take it away again, touching
//! nothing else in its settings file.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

const COMMAND: &str = "radar-claude-statusline";
const KEY: &str = "statusLine";

#[derive(Clone, Copy, PartialEq)]
pub enum Action {
    Configure,
    Remove,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    /// The new file contents.
    Write(String),
    /// Nothing to do: the file already says what was asked.
    Unchanged,
    /// Another status line is set; its command, as written.
    Conflict(String),
}

pub fn claude(action: Action) -> i32 {
    let path = settings_path();
    let existing = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            eprintln!("radar: cannot read {}: {e}", path.display());
            return 1;
        }
    };
    let planned = match action {
        Action::Configure => plan_configure(existing.as_deref(), &helper_command()),
        Action::Remove => plan_remove(existing.as_deref()),
    };
    let outcome = match planned {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("radar: {}: {e}; leaving it alone", path.display());
            return 1;
        }
    };
    let path_name = path.display();
    match (action, outcome) {
        (Action::Configure, Outcome::Unchanged) => {
            println!("radar: Claude Code status line already set in {path_name}");
            0
        }
        (Action::Remove, Outcome::Unchanged) => {
            println!("radar: no radar status line to remove in {path_name}");
            0
        }
        (Action::Configure, Outcome::Conflict(command)) => {
            eprintln!(
                "radar: warning: {path_name} already has a status line ({command:?}); leaving it alone.\n\
                 To keep it, have it pipe its input through {COMMAND} as well."
            );
            1
        }
        (Action::Remove, Outcome::Conflict(command)) => {
            eprintln!(
                "radar: warning: the status line in {path_name} ({command:?}) runs {COMMAND} \
                 alongside other things; leaving it alone. Remove {COMMAND} from it by hand."
            );
            1
        }
        (action, Outcome::Write(text)) => match write(&path, &text) {
            Ok(()) => {
                let done = match action {
                    Action::Configure => "set Claude Code status line in",
                    Action::Remove => "removed Claude Code status line from",
                };
                println!("radar: {done} {path_name}");
                0
            }
            Err(e) => {
                eprintln!("radar: cannot write {path_name}: {e}");
                1
            }
        },
    }
}

/// Claude Code reads `$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json`.
fn settings_path() -> PathBuf {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude")
        });
    dir.join("settings.json")
}

fn parse(text: &str) -> Result<serde_json::Map<String, Value>, String> {
    match serde_json::from_str(text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("not a JSON object".into()),
        Err(e) => Err(format!("not valid JSON ({e})")),
    }
}

fn command_of(status_line: &Value) -> String {
    match status_line.get("command").and_then(Value::as_str) {
        Some(c) => c.to_string(),
        None => status_line.to_string(),
    }
}

/// Whether `command` runs ours and nothing else: its name, or a path to it,
/// quoted or not.
fn is_ours(command: &str) -> bool {
    unquote(command.trim())
        .is_some_and(|path| path == COMMAND || path.ends_with(&format!("/{COMMAND}")))
}

/// The word the shell would read from `command`, if it is a single word made
/// of plain characters, single-quoted runs and backslash escapes.
fn unquote(command: &str) -> Option<String> {
    let mut word = String::new();
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => loop {
                match chars.next()? {
                    '\'' => break,
                    c => word.push(c),
                }
            },
            '\\' => word.push(chars.next()?),
            c if is_plain(c) => word.push(c),
            _ => return None,
        }
    }
    (!word.is_empty()).then_some(word)
}

/// Whether the shell takes `c` as it is, without quoting.
fn is_plain(c: char) -> bool {
    c.is_ascii_alphanumeric() || "/._-~".contains(c)
}

fn shell_quote(word: &str) -> String {
    if word.chars().all(is_plain) {
        word.into()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// How the status line should name the helper: by name when the helper beside
/// this binary is what `PATH` finds, else by its full path.
fn helper_command() -> String {
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.to_path_buf()))
    else {
        return COMMAND.into();
    };
    let helper = dir.join(COMMAND);
    let found = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join(COMMAND))
            .find(|h| h.is_file())
    });
    let on_path = found.is_some_and(|h| h == helper);
    match helper.to_str() {
        Some(path) if helper.exists() && !on_path => shell_quote(path),
        _ => COMMAND.into(),
    }
}

fn plan_configure(existing: Option<&str>, command: &str) -> Result<Outcome, String> {
    let Some(text) = existing.filter(|t| !t.trim().is_empty()) else {
        return Ok(Outcome::Write(fresh(command)));
    };
    let map = parse(text)?;
    if let Some(status_line) = map.get(KEY) {
        let current = command_of(status_line);
        return Ok(if current.contains(COMMAND) {
            Outcome::Unchanged
        } else {
            Outcome::Conflict(current)
        });
    }
    if map.is_empty() {
        return Ok(Outcome::Write(fresh(command)));
    }

    // Splice the key in before the closing brace, so the rest of the file
    // stays byte for byte as it was.
    let close = text.rfind('}').ok_or("no closing brace")?;
    let body = text[..close].trim_end();
    let after = &text[close..];
    let json = serde_json::to_string(command).expect("strings serialize");
    let new = if body.contains('\n') {
        let indent = indent_of(body);
        format!("{body},\n{}{after}", entry(indent, &json))
    } else {
        format!(r#"{body},"{KEY}":{{"type":"command","command":{json}}}{after}"#)
    };

    let mut expected = map;
    expected.insert(KEY.into(), status_line(command));
    check(new, &expected, "add")
}

fn plan_remove(existing: Option<&str>) -> Result<Outcome, String> {
    let Some(text) = existing.filter(|t| !t.trim().is_empty()) else {
        return Ok(Outcome::Unchanged);
    };
    let mut map = parse(text)?;
    let Some(status_line) = map.get(KEY) else {
        return Ok(Outcome::Unchanged);
    };
    let command = command_of(status_line);
    if !is_ours(&command) {
        return Ok(if command.contains(COMMAND) {
            Outcome::Conflict(command)
        } else {
            Outcome::Unchanged
        });
    }

    // Cut out the key, its value and one comma beside it.
    let (start, end) = key_span(text, KEY).ok_or("could not find the status line")?;
    let before = text[..start].trim_end();
    let rest = text[end..].trim_start();
    let new = if let Some(before) = before.strip_suffix(',') {
        format!("{before}{}", &text[end..])
    } else if let Some(rest) = rest.strip_prefix(',') {
        format!("{}{}", &text[..start], rest.trim_start())
    } else {
        format!("{before}{rest}")
    };

    map.remove(KEY);
    check(new, &map, "remove")
}

/// Accept the edited text only if it parses to exactly what was intended.
fn check(
    new: String,
    expected: &serde_json::Map<String, Value>,
    verb: &str,
) -> Result<Outcome, String> {
    match parse(&new) {
        Ok(map) if &map == expected => Ok(Outcome::Write(new)),
        _ => Err(format!("could not {verb} the status line cleanly")),
    }
}

/// Where the top-level `key` starts, and where its value ends, in `text`,
/// which must be a valid JSON object.
fn key_span(text: &str, key: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut depth = 0;
    let mut expect_key = false;
    let mut start = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                let end = string_end(bytes, i)?;
                if depth == 1 && expect_key {
                    expect_key = false;
                    let name: Option<String> = serde_json::from_str(&text[i..=end]).ok();
                    if name.as_deref() == Some(key) {
                        start = Some(i);
                    }
                }
                i = end;
            }
            b'{' | b'[' => {
                depth += 1;
                expect_key = depth == 1;
            }
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return start.map(|s| (s, text[..i].trim_end().len()));
                }
            }
            b',' if depth == 1 => {
                if let Some(s) = start {
                    return Some((s, text[..i].trim_end().len()));
                }
                expect_key = true;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The index of the quote that closes the string opening at `open`.
fn string_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'"' => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn status_line(command: &str) -> Value {
    serde_json::json!({ "type": "command", "command": command })
}

fn fresh(command: &str) -> String {
    let json = serde_json::to_string(command).expect("strings serialize");
    format!("{{\n{}}}\n", entry("  ", &json))
}

/// The key and its value, with `command` already JSON-encoded.
fn entry(indent: &str, command: &str) -> String {
    format!(
        "{indent}\"{KEY}\": {{\n\
         {indent}{indent}\"type\": \"command\",\n\
         {indent}{indent}\"command\": {command}\n\
         {indent}}}\n"
    )
}

/// The indentation of the first key, so the new one lines up with it.
fn indent_of(body: &str) -> &str {
    body.split_once('{')
        .and_then(|(_, rest)| rest.split_once('\n'))
        .map(|(_, rest)| {
            let end = rest.find(|c| c != ' ' && c != '\t').unwrap_or(rest.len());
            &rest[..end]
        })
        .filter(|i| !i.is_empty())
        .unwrap_or("  ")
}

/// Follow `path` through any symlinks, even to a target that does not exist yet.
fn resolve(path: &Path) -> std::io::Result<PathBuf> {
    let mut path = path.to_path_buf();
    for _ in 0..40 {
        match fs::read_link(&path) {
            Ok(target) => path = path.parent().unwrap_or(Path::new("/")).join(target),
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(path),
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("too many levels of symbolic links"))
}

/// Replace the file atomically, following a symlink to the real file and
/// keeping its permissions.
fn write(path: &Path, text: &str) -> std::io::Result<()> {
    let target = resolve(path)?;
    let dir = target.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".settings.json.radar.{}", std::process::id()));
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str =
        r#"{"statusLine": {"type": "command", "command": "radar-claude-statusline"}}"#;

    fn configured(existing: Option<&str>) -> String {
        configured_with(existing, COMMAND)
    }

    fn configured_with(existing: Option<&str>, command: &str) -> String {
        match plan_configure(existing, command) {
            Ok(Outcome::Write(text)) => text,
            other => panic!("expected a write, got {other:?}"),
        }
    }

    fn removed(existing: &str) -> String {
        match plan_remove(Some(existing)) {
            Ok(Outcome::Write(text)) => text,
            other => panic!("expected a write, got {other:?}"),
        }
    }

    #[test]
    fn creates_settings_when_missing_or_empty() {
        let expected = "{\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"radar-claude-statusline\"\n  }\n}\n";
        assert_eq!(configured(None), expected);
        assert_eq!(configured(Some("")), expected);
        assert_eq!(configured(Some("{}\n")), expected);
    }

    #[test]
    fn adds_key_without_touching_the_rest() {
        let existing = "{\n  \"model\": \"opus\",\n  \"permissions\": {\n    \"allow\": [\"Bash(ls)\"]\n  }\n}\n";
        assert_eq!(
            configured(Some(existing)),
            "{\n  \"model\": \"opus\",\n  \"permissions\": {\n    \"allow\": [\"Bash(ls)\"]\n  },\n  \"statusLine\": {\n    \"type\": \"command\",\n    \"command\": \"radar-claude-statusline\"\n  }\n}\n"
        );
    }

    #[test]
    fn follows_existing_indentation() {
        let existing = "{\n\t\"model\": \"opus\"\n}";
        assert_eq!(
            configured(Some(existing)),
            "{\n\t\"model\": \"opus\",\n\t\"statusLine\": {\n\t\t\"type\": \"command\",\n\t\t\"command\": \"radar-claude-statusline\"\n\t}\n}"
        );
    }

    #[test]
    fn keeps_single_line_files_on_one_line() {
        assert_eq!(
            configured(Some(r#"{"model":"opus"}"#)),
            r#"{"model":"opus","statusLine":{"type":"command","command":"radar-claude-statusline"}}"#
        );
    }

    #[test]
    fn does_nothing_when_already_configured() {
        assert_eq!(plan_configure(Some(OURS), COMMAND), Ok(Outcome::Unchanged));
        let piped = r#"{"statusLine": {"type": "command", "command": "~/.local/bin/radar-claude-statusline"}}"#;
        assert_eq!(plan_configure(Some(piped), COMMAND), Ok(Outcome::Unchanged));
    }

    #[test]
    fn refuses_to_replace_another_status_line() {
        let existing = r#"{"statusLine": {"type": "command", "command": "starship-claude"}}"#;
        assert_eq!(
            plan_configure(Some(existing), COMMAND),
            Ok(Outcome::Conflict("starship-claude".into()))
        );
    }

    #[test]
    fn refuses_invalid_files() {
        for text in ["{\"model\": ", "[]"] {
            assert!(plan_configure(Some(text), COMMAND).is_err());
            assert!(plan_remove(Some(text)).is_err());
        }
    }

    #[test]
    fn remove_undoes_configure() {
        for existing in [
            "{\n  \"model\": \"opus\",\n  \"permissions\": {\n    \"allow\": [\"Bash(ls)\"]\n  }\n}\n",
            "{\n\t\"model\": \"opus\"\n}",
            r#"{"model":"opus"}"#,
        ] {
            assert_eq!(removed(&configured(Some(existing))), existing);
        }
        assert_eq!(removed(&configured(None)), "{}\n");
    }

    #[test]
    fn removes_key_from_anywhere() {
        let first = "{\n  \"statusLine\": {\"type\": \"command\", \"command\": \"radar-claude-statusline\"},\n  \"model\": \"opus\"\n}\n";
        assert_eq!(removed(first), "{\n  \"model\": \"opus\"\n}\n");
        let middle = r#"{"a": "}", "statusLine": {"command": "radar-claude-statusline"}, "b": [1, {"c": 2}]}"#;
        assert_eq!(removed(middle), r#"{"a": "}", "b": [1, {"c": 2}]}"#);
        let nested =
            r#"{"x": {"statusLine": 1}, "statusLine": {"command": "radar-claude-statusline"}}"#;
        assert_eq!(removed(nested), r#"{"x": {"statusLine": 1}}"#);
    }

    #[test]
    fn remove_does_nothing_without_our_status_line() {
        assert_eq!(plan_remove(None), Ok(Outcome::Unchanged));
        assert_eq!(plan_remove(Some("{}")), Ok(Outcome::Unchanged));
        let other = r#"{"statusLine": {"type": "command", "command": "starship-claude"}}"#;
        assert_eq!(plan_remove(Some(other)), Ok(Outcome::Unchanged));
    }

    #[test]
    fn remove_leaves_scripts_that_also_run_ours() {
        for command in [
            "tee >(radar-claude-statusline) | starship",
            "tee /tmp/status.json | ~/.local/bin/radar-claude-statusline",
            "true;~/.local/bin/radar-claude-statusline",
        ] {
            let settings = serde_json::json!({ "statusLine": { "command": command } }).to_string();
            assert_eq!(
                plan_remove(Some(&settings)),
                Ok(Outcome::Conflict(command.into()))
            );
        }
        let path = r#"{"statusLine": {"command": "~/.local/bin/radar-claude-statusline"}}"#;
        assert_eq!(removed(path), "{}");
    }

    #[test]
    fn writes_a_full_path_when_given_one() {
        let path = "/opt/radar/bin/radar-claude-statusline";
        let text = configured_with(None, path);
        assert!(text.contains(r#""command": "/opt/radar/bin/radar-claude-statusline""#));
        assert_eq!(plan_configure(Some(&text), COMMAND), Ok(Outcome::Unchanged));
        assert_eq!(removed(&text), "{}\n");
    }

    #[test]
    fn quotes_awkward_paths() {
        let path = "/home/me/My Tools/radar-claude-statusline";
        let command = shell_quote(path);
        assert_eq!(command, "'/home/me/My Tools/radar-claude-statusline'");
        for existing in [None, Some(r#"{"model":"opus"}"#)] {
            let text = configured_with(existing, &command);
            let settings: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(settings[KEY]["command"], command.as_str());
            assert!(is_ours(&command));
            assert_eq!(removed(&text), existing.unwrap_or("{}\n"));
        }
        let odd = shell_quote("/o'brien/radar-claude-statusline");
        assert_eq!(odd, r"'/o'\''brien/radar-claude-statusline'");
        assert!(is_ours(&odd));
        assert!(!is_ours("'/x/radar-claude-statusline' extra"));
        assert!(!is_ours("/x/radar-claude-statusline';'"));
    }

    #[test]
    fn writes_through_dangling_symlinks() {
        let dir = std::env::temp_dir().join(format!("radar-integrate-{}", std::process::id()));
        fs::create_dir_all(dir.join("dots")).unwrap();
        let link = dir.join("settings.json");
        std::os::unix::fs::symlink("dots/settings.json", &link).unwrap();
        write(&link, "{}\n").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
        assert_eq!(
            fs::read_to_string(dir.join("dots/settings.json")).unwrap(),
            "{}\n"
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
