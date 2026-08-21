// Minimal line-oriented rewriting of the host's tui.toml and config.toml for
// --install / --uninstall / --on / --off. This is NOT a general TOML parser:
// it locates the [status_line] section (or the managed hook block) and adds,
// replaces or removes only its own lines, preserving everything else
// (including `items` and unrelated hooks) byte-for-byte modulo CRLF
// normalization.

pub const EXE_BASENAME: &str = "kimi-code-hud-rs";

const HOOKS_START: &str = "# --- kimi-code-hud-rs hooks START (managed, do not edit) ---";
const HOOKS_END: &str = "# --- kimi-code-hud-rs hooks END ---";

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum CommandValue {
    Absent,
    Parsed(String),
    Unknown,
}

fn normalize(content: &str) -> Vec<String> {
    content.replace("\r\n", "\n").split('\n').map(str::to_string).collect()
}

fn find_section(lines: &[String], name: &str) -> Option<(usize, usize)> {
    let header = format!("[{}]", name);
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == header {
            let mut end = lines.len();
            for (j, l) in lines.iter().enumerate().skip(i + 1) {
                if l.trim_start().starts_with('[') {
                    end = j;
                    break;
                }
            }
            return Some((i, end));
        }
    }
    None
}

/// The raw text after `command =` on a line, if the line is a command key.
fn rhs_of_command(line: &str) -> Option<String> {
    let t = line.trim_start();
    let rest = t.strip_prefix("command")?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?;
    Some(rest.to_string())
}

fn is_command_line(line: &str) -> bool {
    rhs_of_command(line).is_some()
}

fn parse_basic_string(rhs: &str) -> Option<String> {
    if !rhs.starts_with('"') {
        return None;
    }
    let chars: Vec<char> = rhs.chars().collect();
    let mut token = String::from("\"");
    let mut i = 1;
    while i < chars.len() {
        token.push(chars[i]);
        if chars[i] == '\\' && i + 1 < chars.len() {
            i += 1;
            token.push(chars[i]);
        } else if chars[i] == '"' {
            let rest: String = chars[i + 1..].iter().collect();
            let rest = rest.trim();
            if rest.is_empty() || rest.starts_with('#') {
                return serde_json::from_str::<String>(&token).ok();
            }
            return None;
        }
        i += 1;
    }
    None
}

fn parse_literal_string(rhs: &str) -> Option<String> {
    let rest = rhs.strip_prefix('\'')?;
    let end = rest.find('\'')?;
    let value = &rest[..end];
    let tail = rest[end + 1..].trim();
    if tail.is_empty() || tail.starts_with('#') {
        Some(value.to_string())
    } else {
        None
    }
}

fn command_value_from_line(line: &str) -> CommandValue {
    let Some(rhs) = rhs_of_command(line) else {
        return CommandValue::Absent;
    };
    let rhs = rhs.trim();
    if let Some(parsed) = parse_basic_string(rhs) {
        return CommandValue::Parsed(parsed);
    }
    if let Some(literal) = parse_literal_string(rhs) {
        return CommandValue::Parsed(literal);
    }
    CommandValue::Unknown
}

/// Inspect [status_line].command. Unknown syntax is intentionally
/// fail-closed so the self-heal hook never overwrites user configuration.
pub fn inspect_status_line_command(content: &str) -> CommandValue {
    let lines = normalize(content);
    let Some((start, end)) = find_section(&lines, "status_line") else {
        return CommandValue::Absent;
    };
    for line in &lines[start + 1..end] {
        if is_command_line(line) {
            return command_value_from_line(line);
        }
    }
    CommandValue::Absent
}

#[allow(dead_code)] // exercised by tests
pub fn get_status_line_command(content: &str) -> Option<String> {
    match inspect_status_line_command(content) {
        CommandValue::Parsed(v) => Some(v),
        _ => None,
    }
}

pub fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// Split a shell-ish command into words, honoring single/double quotes and
/// backslash escapes inside double quotes. None when the string carries shell
/// metacharacters or an unterminated quote — such commands are never ours.
pub fn safe_command_words(command: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = command.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else if ch == '\\' && q == '"' && i + 1 < chars.len() {
                i += 1;
                word.push(chars[i]);
            } else {
                word.push(ch);
            }
            started = true;
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            started = true;
            i += 1;
            continue;
        }
        if ch.is_whitespace() {
            if started {
                words.push(std::mem::take(&mut word));
                started = false;
            }
            i += 1;
            continue;
        }
        if matches!(ch, ';' | '&' | '|' | '<' | '>' | '(' | ')' | '`') {
            return None;
        }
        if ch == '$' && chars.get(i + 1) == Some(&'(') {
            return None;
        }
        word.push(ch);
        started = true;
        i += 1;
    }
    if quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

fn is_flag(word: &str) -> bool {
    let Some(rest) = word.strip_prefix("--") else {
        return false;
    };
    let mut chars = rest.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    let body: String = chars.collect();
    if let Some((name, value)) = body.split_once('=') {
        !value.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | ':' | '-'))
    } else {
        body.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    }
}

fn is_plain_value(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | ':' | '-'))
}

/// True only for the command this HUD generates: the kimi-code-hud-rs
/// executable, optionally followed by simple flag arguments. Anything else —
/// stray words, odd flag shapes, shell metacharacters — stays foreign so
/// unknown commands are never rewritten.
pub fn is_own_command(command: &str) -> bool {
    let Some(words) = safe_command_words(command) else {
        return false;
    };
    if words.is_empty() {
        return false;
    }
    let exe = words[0].replace('\\', "/");
    let exe = exe.rsplit('/').next().unwrap_or("").to_lowercase();
    let stem = exe.strip_suffix(".exe").unwrap_or(&exe).to_string();
    if stem != EXE_BASENAME {
        return false;
    }
    let mut expects_value = false;
    for word in &words[1..] {
        if is_flag(word) {
            expects_value = !word.contains('=');
            continue;
        }
        if expects_value && is_plain_value(word) {
            expects_value = false;
            continue;
        }
        return false;
    }
    true
}

/// tui.toml content with the status-line command installed. Idempotent;
/// preserves an existing [status_line] section (e.g. its `items`).
pub fn set_status_line_command(content: &str, command: &str) -> String {
    let line = format!("command = \"{}\"", toml_escape(command));
    let mut lines = normalize(content);
    while matches!(lines.last(), Some(l) if l.trim().is_empty()) {
        lines.pop();
    }
    match find_section(&lines, "status_line") {
        None => {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push("[status_line]".to_string());
            lines.push(line);
            format!("{}\n", lines.join("\n"))
        }
        Some((start, end)) => {
            for i in start + 1..end {
                if is_command_line(&lines[i]) {
                    if lines[i] == line {
                        return if content.ends_with('\n') {
                            content.to_string()
                        } else {
                            format!("{}\n", content)
                        };
                    }
                    lines[i] = line;
                    return format!("{}\n", lines.join("\n"));
                }
            }
            lines.insert(start + 1, line);
            format!("{}\n", lines.join("\n"))
        }
    }
}

/// tui.toml content with our status-line command removed. Only command lines
/// inside [status_line] that are ours (or equal the given command) are
/// dropped; a section left with nothing but blanks is removed entirely.
pub fn remove_status_line_command(content: &str, command: &str) -> String {
    let lines = normalize(content);
    let Some((start, end)) = find_section(&lines, "status_line") else {
        return content.to_string();
    };
    let mut kept: Vec<String> = Vec::new();
    for line in &lines[start + 1..end] {
        if let CommandValue::Parsed(value) = command_value_from_line(line) {
            if value == command || is_own_command(&value) {
                continue;
            }
        }
        kept.push(line.clone());
    }
    if kept.iter().all(|l| l.trim().is_empty()) {
        let mut out: Vec<String> = lines[..start].to_vec();
        out.extend(lines[end..].iter().cloned());
        while matches!(out.last(), Some(l) if l.trim().is_empty()) {
            out.pop();
        }
        return if out.is_empty() {
            String::new()
        } else {
            format!("{}\n", out.join("\n"))
        };
    }
    let mut out: Vec<String> = lines[..=start].to_vec();
    out.extend(kept);
    out.extend(lines[end..].iter().cloned());
    out.join("\n")
}

fn hook_block_lines(hook_command: &str) -> Vec<String> {
    vec![
        HOOKS_START.to_string(),
        "[[hooks]]".to_string(),
        "event = \"SessionStart\"".to_string(),
        format!("command = \"{}\"", toml_escape(hook_command)),
        "timeout = 5".to_string(),
        HOOKS_END.to_string(),
    ]
}

/// (start, Some(end)) of the managed marker pair; a dangling START marker has
/// no END and must leave the file untouched.
fn find_block(lines: &[String]) -> Option<(usize, Option<usize>)> {
    let start = lines.iter().position(|l| l.trim() == HOOKS_START)?;
    for (i, l) in lines.iter().enumerate().skip(start + 1) {
        if l.trim() == HOOKS_END {
            return Some((start, Some(i)));
        }
    }
    Some((start, None))
}

fn hook_path_from_command(hook_command: &str) -> Option<String> {
    let t = hook_command.trim();
    let stripped = t.strip_prefix("node").filter(|r| r.starts_with(char::is_whitespace));
    let path = stripped.map(str::trim).unwrap_or(t);
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

/// Bare [[hooks]] blocks (written before the marker convention) registering
/// our SessionStart hook. A block spans its [[hooks]] header to the next
/// table header; it matches when it sets event = "SessionStart" and a command
/// containing the hook path.
fn find_bare_blocks(
    lines: &[String],
    hook_path: Option<&str>,
    exclude: Option<(usize, usize)>,
) -> Vec<(usize, usize)> {
    let Some(hook_path) = hook_path else {
        return Vec::new();
    };
    let escaped_path = hook_path.replace('\\', "\\\\");
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some((ex_start, ex_end)) = exclude {
            if i >= ex_start && i <= ex_end {
                i = ex_end + 1;
                continue;
            }
        }
        if lines[i].trim() != "[[hooks]]" {
            i += 1;
            continue;
        }
        let mut end = lines.len();
        for (j, l) in lines.iter().enumerate().skip(i + 1) {
            if let Some((ex_start, _)) = exclude {
                if j == ex_start {
                    end = j;
                    break;
                }
            }
            if l.trim().starts_with('[') {
                end = j;
                break;
            }
        }
        let mut has_event = false;
        let mut has_command = false;
        for l in &lines[i + 1..end] {
            let t = l.trim();
            if t == "event = \"SessionStart\"" {
                has_event = true;
            }
            if t.starts_with("command = \"")
                && (t.contains(hook_path) || t.contains(&escaped_path))
            {
                has_command = true;
            }
        }
        if has_event && has_command {
            let mut last = end.saturating_sub(1);
            while last > i && lines[last].trim().is_empty() {
                last -= 1;
            }
            ranges.push((i, last));
        }
        i = end.max(i + 1);
    }
    ranges
}

fn collapse_blanks_at(lines: &mut Vec<String>, i: usize) {
    let mut lo = i.min(lines.len());
    while lo > 0 && lines[lo - 1].trim().is_empty() {
        lo -= 1;
    }
    let mut hi = i.min(lines.len());
    while hi < lines.len() && lines[hi].trim().is_empty() {
        hi += 1;
    }
    let count = hi.saturating_sub(lo);
    if lo == 0 {
        lines.drain(0..count);
    } else if count > 1 {
        lines.drain(lo + 1..hi);
    }
}

struct RangeEdit {
    start: usize,
    end: usize,
    replacement: Option<Vec<String>>,
}

fn apply_ranges(lines: &mut Vec<String>, ranges: Vec<RangeEdit>) {
    let mut sorted = ranges;
    sorted.sort_by(|a, b| b.start.cmp(&a.start));
    for range in sorted {
        let is_delete = range.replacement.is_none();
        let replacement = range.replacement.unwrap_or_default();
        let at = range.start.min(lines.len());
        let end = range.end.min(lines.len());
        lines.splice(at..=end, replacement);
        if is_delete {
            collapse_blanks_at(lines, at);
        }
    }
}

/// config.toml content with our SessionStart hook block present. Idempotent;
/// refreshes the block in place when the hook path moved. Legacy unmarked
/// [[hooks]] blocks are adopted (first upgraded, the rest removed).
pub fn ensure_hooks_block(content: &str, hook_command: &str) -> String {
    let mut lines = normalize(content);
    while matches!(lines.last(), Some(l) if l.trim().is_empty()) {
        lines.pop();
    }
    let block = hook_block_lines(hook_command);
    let found = find_block(&lines);
    if let Some((_, None)) = found {
        return content.to_string();
    }
    let bare = find_bare_blocks(
        &lines,
        hook_path_from_command(hook_command).as_deref(),
        found.map(|(s, e)| (s, e.unwrap())),
    );
    let mut ranges: Vec<RangeEdit> = Vec::new();
    match found {
        Some((start, Some(end))) => {
            ranges.push(RangeEdit { start, end, replacement: Some(block.clone()) });
            for (s, e) in bare {
                ranges.push(RangeEdit { start: s, end: e, replacement: None });
            }
        }
        None => {
            if let Some((first, rest)) = bare.split_first() {
                ranges.push(RangeEdit {
                    start: first.0,
                    end: first.1,
                    replacement: Some(block.clone()),
                });
                for (s, e) in rest {
                    ranges.push(RangeEdit { start: *s, end: *e, replacement: None });
                }
            } else {
                if !lines.is_empty() {
                    lines.push(String::new());
                }
                lines.extend(block);
            }
        }
        Some((_, None)) => unreachable!(),
    }
    apply_ranges(&mut lines, ranges);
    while matches!(lines.last(), Some(l) if l.trim().is_empty()) {
        lines.pop();
    }
    let out = format!("{}\n", lines.join("\n"));
    let normalized = content.replace("\r\n", "\n");
    if out == normalized {
        content.to_string()
    } else {
        out
    }
}

/// config.toml content with our hook block (and legacy bare blocks) removed.
pub fn remove_hooks_block(content: &str, hook_command: &str) -> String {
    let mut lines = normalize(content);
    let found = find_block(&lines);
    if let Some((_, None)) = found {
        return content.to_string();
    }
    let mut ranges: Vec<RangeEdit> = Vec::new();
    if let Some((start, Some(end))) = found {
        ranges.push(RangeEdit { start, end, replacement: None });
    }
    for (s, e) in find_bare_blocks(
        &lines,
        hook_path_from_command(hook_command).as_deref(),
        found.map(|(s, e)| (s, e.unwrap())),
    ) {
        ranges.push(RangeEdit { start: s, end: e, replacement: None });
    }
    if ranges.is_empty() {
        return content.to_string();
    }
    apply_ranges(&mut lines, ranges);
    while matches!(lines.last(), Some(l) if l.trim().is_empty()) {
        lines.pop();
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CMD: &str = "\"/opt/bin/kimi-code-hud-rs\"";

    #[test]
    fn set_and_get_roundtrip() {
        let content = "theme = \"dark\"\n\n[status_line]\nitems = [\"x\"]\n";
        let next = set_status_line_command(content, CMD);
        assert!(next.contains("items = [\"x\"]"));
        assert_eq!(get_status_line_command(&next).as_deref(), Some(CMD));
        // Idempotent.
        assert_eq!(set_status_line_command(&next, CMD), next);
    }

    #[test]
    fn set_appends_section_to_empty_file() {
        let next = set_status_line_command("", CMD);
        assert!(next.contains("[status_line]"));
        assert_eq!(get_status_line_command(&next).as_deref(), Some(CMD));
    }

    #[test]
    fn inspect_reports_unknown_syntax() {
        let content = "[status_line]\ncommand = 'not a string we write\n";
        assert_eq!(inspect_status_line_command(content), CommandValue::Unknown);
    }

    #[test]
    fn remove_only_our_command() {
        let foreign = "/usr/bin/my-own-status";
        let content = format!(
            "[status_line]\ncommand = \"{}\"\n",
            CMD.replace('"', "")
        );
        let next = set_status_line_command(&content, CMD);
        let next = set_status_line_command(&next, foreign);
        assert!(is_own_command(CMD));
        assert!(!is_own_command(foreign));
        let removed = remove_status_line_command(&next, CMD);
        // Our command line is gone, the foreign one survives.
        assert!(!removed.contains("kimi-code-hud-rs\" --sync"));
        assert!(removed.contains(foreign));
    }

    #[test]
    fn remove_drops_empty_section() {
        let next = set_status_line_command("", CMD);
        let removed = remove_status_line_command(&next, CMD);
        assert!(!removed.contains("[status_line]"));
    }

    #[test]
    fn safe_words_reject_metacharacters() {
        assert!(safe_command_words("a b c").is_some());
        assert_eq!(safe_command_words("\"a b\" c").unwrap(), vec!["a b", "c"]);
        assert!(safe_command_words("a;rm -rf").is_none());
        assert!(safe_command_words("$(x)").is_none());
        assert!(safe_command_words("\"unterminated").is_none());
    }

    #[test]
    fn is_own_command_accepts_only_our_exe() {
        assert!(is_own_command("/opt/bin/kimi-code-hud-rs"));
        assert!(is_own_command("\"C:\\\\tools\\\\kimi-code-hud-rs.exe\""));
        assert!(is_own_command("/opt/bin/kimi-code-hud-rs --flag=on"));
        assert!(!is_own_command("/opt/bin/kimi-code-hud-rs extra-positional"));
        assert!(!is_own_command("node /x/kimi-hud.mjs"));
        assert!(!is_own_command("/opt/bin/some-other-tool"));
    }

    #[test]
    fn hooks_block_roundtrip() {
        let hook = "\"/opt/bin/kimi-code-hud-rs\" --sync-status-line";
        let base = "model = \"k3\"\n\n[[hooks]]\nevent = \"UserPromptSubmit\"\ncommand = \"echo hi\"\n";
        let next = ensure_hooks_block(base, hook);
        assert!(next.contains(HOOKS_START));
        assert!(next.contains("event = \"SessionStart\""));
        assert!(next.contains("UserPromptSubmit"));
        assert_eq!(ensure_hooks_block(&next, hook), next);
        let removed = remove_hooks_block(&next, hook);
        assert!(!removed.contains(HOOKS_START));
        assert!(removed.contains("UserPromptSubmit"));
    }

    #[test]
    fn hooks_block_dangling_start_is_untouched() {
        let broken = format!("{}\n[[hooks]]\n", HOOKS_START);
        assert_eq!(ensure_hooks_block(&broken, "x"), broken);
        assert_eq!(remove_hooks_block(&broken, "x"), broken);
    }
}
