// Readers for the host's config.toml model/provider/thinking tables. The
// host writes canonical TOML (quoted table names, one key per line), so a
// small line scanner is sufficient and keeps unrelated content untouched.

pub const MANAGED_KIMI_PROVIDER: &str = "managed:kimi-code";

pub struct Section {
    pub parts: Vec<String>,
    pub body: String,
}

/// Parse a `[dotted."quoted"]` table header into its parts; None for array
/// tables or malformed quoting.
fn parse_header_parts(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    let inner = t.strip_prefix('[')?.strip_suffix(']')?;
    if inner.starts_with('[') {
        return None; // [[array of tables]]
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut escaped = false;
    for ch in inner.chars() {
        if in_quotes {
            if escaped {
                cur.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_quotes = false;
            } else {
                cur.push(ch);
            }
        } else if ch == '"' {
            in_quotes = true;
        } else if ch == '.' {
            parts.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    if in_quotes {
        return None;
    }
    parts.push(cur.trim().to_string());
    Some(parts)
}

/// All sections of a TOML document with their raw body text.
pub fn sections(text: &str) -> Vec<Section> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim_start().starts_with('[') {
            if let Some(parts) = parse_header_parts(lines[i]) {
                let start = i + 1;
                let mut end = lines.len();
                for (j, l) in lines.iter().enumerate().skip(start) {
                    if l.trim_start().starts_with('[') {
                        end = j;
                        break;
                    }
                }
                out.push(Section { parts, body: lines[start..end].join("\n") });
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Raw text after `key =` for the first matching line in a section body.
pub fn key_rhs(section: &str, key: &str) -> Option<String> {
    for line in section.split('\n') {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix(key) {
            let rest = rest.trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Whether a section declares `key = ...` at all.
pub fn has_key(section: &str, key: &str) -> bool {
    key_rhs(section, key).is_some()
}

pub fn bool_value(section: &str, key: &str) -> Option<bool> {
    let rhs = key_rhs(section, key)?;
    let first = rhs.split_whitespace().next()?;
    match first {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Raw (undecoded) basic-string value, as the Node port's stringValue.
pub fn string_value(section: &str, key: &str) -> Option<String> {
    let rhs = key_rhs(section, key)?;
    if !rhs.starts_with('"') {
        return None;
    }
    let end = rhs[1..].find('"')? + 1;
    Some(rhs[1..end].to_string())
}

/// Basic-string value with escapes decoded (JSON-compatible escaping).
pub fn decoded_string_value(section: &str, key: &str) -> Option<String> {
    let rhs = key_rhs(section, key)?;
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
            return serde_json::from_str::<String>(&token).ok();
        }
        i += 1;
    }
    None
}

/// Inline string-array value: `capabilities = [ "thinking", ... ]`. None when
/// the key is absent.
pub fn string_array_value(section: &str, key: &str) -> Option<Vec<String>> {
    let rhs = key_rhs(section, key)?;
    let trimmed = rhs.trim_start();
    let inner = trimmed.strip_prefix('[')?;
    let end = inner.find(']')?;
    let mut items = Vec::new();
    let mut rest = &inner[..end];
    while let Some(pos) = rest.find('"') {
        let after = &rest[pos + 1..];
        let Some(close) = after.find('"') else { break };
        items.push(after[..close].to_string());
        rest = &after[close + 1..];
    }
    Some(items)
}

/// Body of the flat `[name]` table, or None.
pub fn table_text(text: &str, table: &str) -> Option<String> {
    sections(text)
        .into_iter()
        .find(|s| s.parts.len() == 1 && s.parts[0] == table)
        .map(|s| s.body)
}

/// The [models."<alias>"] table body whose alias, display_name, or model id
/// matches the given name.
pub fn find_model_table(text: &str, name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    for s in sections(text) {
        if s.parts.len() == 2 && s.parts[0] == "models" {
            let alias = &s.parts[1];
            if alias == name
                || string_value(&s.body, "display_name").as_deref() == Some(name)
                || string_value(&s.body, "model").as_deref() == Some(name)
            {
                return Some(s.body);
            }
        }
    }
    None
}

/// Body of [providers.<name>] / [providers."<name>"], matched exactly.
pub fn find_provider_table(text: &str, name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    sections(text)
        .into_iter()
        .find(|s| s.parts.len() == 2 && s.parts[0] == "providers" && s.parts[1] == name)
        .map(|s| s.body)
}

/// Body of the [providers."managed:kimi-code".oauth] sub-table, or None.
pub fn managed_oauth_table(text: &str) -> Option<String> {
    sections(text)
        .into_iter()
        .find(|s| s.parts.len() == 3 && s.parts[0] == "providers" && s.parts[1] == MANAGED_KIMI_PROVIDER && s.parts[2] == "oauth")
        .map(|s| s.body)
}

/// Resolve which provider serves the active model: the wire's modelAlias is
/// exact and preferred, the payload's display string is the fallback. None
/// when the model cannot be attributed (missing config, unknown model, no
/// provider key) — quota rendering fails closed in that case.
pub fn resolve_model_provider(
    model_alias: Option<&str>,
    model_display: Option<&str>,
    config_text: &str,
) -> Option<String> {
    for name in [model_alias, model_display].into_iter().flatten() {
        if name.is_empty() {
            continue;
        }
        if let Some(table) = find_model_table(config_text, name) {
            if let Some(provider) = string_value(&table, "provider") {
                return Some(provider);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
theme = "dark"

[thinking]
enabled = true
effort = "high"

[models."kimi-code/k3"]
model = "k3"
display_name = "K3"
provider = "managed:kimi-code"
capabilities = [ "thinking", "always_thinking" ]
support_efforts = [ "low", "high", "max" ]
default_effort = "high"

[models."openai/gpt"]
model = "gpt-5.6"
display_name = "GPT"
provider = "openai"

[providers."managed:kimi-code"]
base_url = "https://api.kimi.com/coding/v1"

[providers."managed:kimi-code".oauth]
key = "oauth/kimi-code"
oauth_host = "https://auth.kimi.com"
"#;

    #[test]
    fn finds_model_tables_by_all_names() {
        for name in ["kimi-code/k3", "K3", "k3"] {
            let body = find_model_table(CONFIG, name).expect(name);
            assert!(body.contains("managed:kimi-code"));
        }
        assert!(find_model_table(CONFIG, "nope").is_none());
    }

    #[test]
    fn resolves_provider() {
        assert_eq!(
            resolve_model_provider(Some("kimi-code/k3"), None, CONFIG).as_deref(),
            Some(MANAGED_KIMI_PROVIDER)
        );
        assert_eq!(
            resolve_model_provider(None, Some("GPT"), CONFIG).as_deref(),
            Some("openai")
        );
        assert_eq!(resolve_model_provider(None, Some("mystery"), CONFIG), None);
    }

    #[test]
    fn parses_values() {
        let thinking = table_text(CONFIG, "thinking").unwrap();
        assert_eq!(bool_value(&thinking, "enabled"), Some(true));
        assert_eq!(string_value(&thinking, "effort").as_deref(), Some("high"));

        let model = find_model_table(CONFIG, "k3").unwrap();
        assert_eq!(
            string_array_value(&model, "capabilities"),
            Some(vec!["thinking".to_string(), "always_thinking".to_string()])
        );
        assert!(has_key(&model, "support_efforts"));
        assert_eq!(string_value(&model, "default_effort").as_deref(), Some("high"));
    }

    #[test]
    fn finds_oauth_subtable() {
        let oauth = managed_oauth_table(CONFIG).unwrap();
        assert_eq!(
            decoded_string_value(&oauth, "key").as_deref(),
            Some("oauth/kimi-code")
        );
        assert!(find_provider_table(CONFIG, "managed:kimi-code").is_some());
    }
}
