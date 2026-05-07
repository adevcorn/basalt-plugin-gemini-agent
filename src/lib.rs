//! gemini-agent — Basalt plugin providing the Gemini CLI agent launcher.
//!
//! Provides: agent-launcher:gemini
//! Parses:   gemini --output-format stream-json   (NDJSON, one JSON object per line)

use basalt_plugin_sdk::prelude::*;

basalt_plugin_meta! {
    name:              "gemini-agent",
    version:           env!("CARGO_PKG_VERSION"),
    hook_flags:        CAP_AGENT_LAUNCHER,
    provides:          "agent-launcher:gemini",
    requires:          "",
    file_globs:        "",
    activates_on:      "",
    activation_events: "",
}

// ---------------------------------------------------------------------------
// agent_metadata
// ---------------------------------------------------------------------------

#[basalt_plugin]
fn agent_metadata() -> AgentMetadata {
    AgentMetadata {
        name: "Gemini CLI".into(),
        executable: "/opt/homebrew/bin/gemini".into(),
        args: vec!["-y".into(), "--output-format".into(), "stream-json".into()],
        // New session: pass the prompt via -p
        resume_new_args: vec![
            "-y".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--skip-trust".into(),
            "-p".into(),
            "{prompt}".into(),
        ],
        // Resume the recorded Gemini session and continue it with a positional prompt.
        resume_cont_args: vec![
            "-y".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--skip-trust".into(),
            "--resume".into(),
            "{session_id}".into(),
            "{prompt}".into(),
        ],
        execution_tier: AgentExecutionTier::StructuredDirect,
        workspace_capabilities: vec![
            "speculative-edits".into(),
            "approval-required".into(),
            "utf8-text".into(),
            "create".into(),
            "delete".into(),
            "rename".into(),
            "materialized-copy".into(),
        ],
    }
}

// ---------------------------------------------------------------------------
// Parser state
// ---------------------------------------------------------------------------

struct ParseState {
    open_message: bool,
    open_thought: bool,
}

impl ParseState {
    fn decode(state: &[u8]) -> Self {
        Self {
            open_message: state.first().copied().unwrap_or(0) != 0,
            open_thought: state.get(1).copied().unwrap_or(0) != 0,
        }
    }

    fn encode(&self) -> Vec<u8> {
        vec![self.open_message as u8, self.open_thought as u8]
    }
}

// ---------------------------------------------------------------------------
// agent_parse_line
// ---------------------------------------------------------------------------

#[basalt_plugin]
fn agent_parse_line(line: &[u8], state: &[u8]) -> (Vec<u8>, Vec<AgentEvent>) {
    let Ok(line_str) = std::str::from_utf8(line) else {
        return (state.to_vec(), vec![]);
    };
    let line_str = line_str.trim();
    if line_str.is_empty() {
        return (state.to_vec(), vec![]);
    }

    let mut parse_state = ParseState::decode(state);
    let events = parse_gemini_line(line_str, &mut parse_state);
    (parse_state.encode(), events)
}

fn parse_gemini_line(line: &str, parse_state: &mut ParseState) -> Vec<AgentEvent> {
    let type_val = match json_str(line, "type") {
        Some(t) => t,
        None => return vec![],
    };

    match type_val.as_str() {
        "tool_use" => {
            let tool_name = json_str(line, "tool_name").unwrap_or_default();
            let tool_id = json_str(line, "tool_id").unwrap_or_default();
            if tool_id.is_empty() {
                return vec![];
            }

            let params_raw = json_object_raw(line, "parameters").unwrap_or_default();
            let raw_cmd = human_raw_cmd(&tool_name, &params_raw);
            let tool_display = tool_display_name(&tool_name, &params_raw);
            let category = gemini_category(&tool_name);
            let file_paths = gemini_file_paths(&params_raw);

            vec![AgentEvent::NewEntry {
                vendor_id: tool_id,
                tool: tool_display,
                category,
                raw_cmd,
                file_paths,
            }]
        }

        "tool_result" => {
            let tool_id = match json_str(line, "tool_id") {
                Some(id) => id,
                None => return vec![],
            };
            let status = json_str(line, "status").unwrap_or_else(|| "success".into());
            let exit_code = if status == "success" { 0i32 } else { 1i32 };
            vec![AgentEvent::CloseEntry {
                vendor_id: tool_id,
                exit_code,
                output_lines: vec![],
            }]
        }

        "result" => {
            let status = json_str(line, "status").unwrap_or_else(|| "success".into());
            parse_state.open_message = false;
            parse_state.open_thought = false;
            vec![AgentEvent::SessionEnded {
                success: status == "success",
            }]
        }

        "init" => {
            if let Some(sid) = json_str(line, "session_id") {
                vec![AgentEvent::SessionIDAvailable(sid)]
            } else {
                vec![]
            }
        }

        "message" => parse_message_event(line, parse_state),

        _ => vec![],
    }
}

fn parse_message_event(line: &str, parse_state: &mut ParseState) -> Vec<AgentEvent> {
    let role = json_str(line, "role").unwrap_or_default().to_lowercase();
    if role == "user" || role == "system" {
        return vec![];
    }

    let text = json_str(line, "content")
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return vec![];
    }

    if !role.contains("thought") {
        let embedded = parse_embedded_thought_segments(&text);
        if embedded.len() > 1 {
            return emit_embedded_segments(embedded, parse_state);
        }
    }

    let (vendor_id, category, is_open) = if role.contains("thought") {
        ("gemini-thought", "thought", &mut parse_state.open_thought)
    } else {
        ("gemini-message", "message", &mut parse_state.open_message)
    };

    if !*is_open {
        *is_open = true;
        return vec![AgentEvent::NewEntry {
            vendor_id: vendor_id.into(),
            tool: text,
            category: category.into(),
            raw_cmd: String::new(),
            file_paths: vec![],
        }];
    }

    vec![AgentEvent::AppendToEntry {
        vendor_id: vendor_id.into(),
        text,
    }]
}

fn emit_embedded_segments(
    segments: Vec<(bool, String)>,
    parse_state: &mut ParseState,
) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let mut last_was_thought = false;

    for (is_thought, text) in segments {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }

        let vendor_id = if is_thought {
            "gemini-thought"
        } else {
            "gemini-message"
        };
        let category = if is_thought { "thought" } else { "message" };

        events.push(AgentEvent::NewEntry {
            vendor_id: vendor_id.into(),
            tool: trimmed.to_string(),
            category: category.into(),
            raw_cmd: String::new(),
            file_paths: vec![],
        });

        last_was_thought = is_thought;
    }

    parse_state.open_message = !last_was_thought;
    parse_state.open_thought = last_was_thought;
    events
}

fn parse_embedded_thought_segments(text: &str) -> Vec<(bool, String)> {
    let mut segments = Vec::new();
    let mut cursor = 0usize;
    let mut current_is_thought = false;

    while let Some((start, end)) = find_thought_marker(text, cursor) {
        let prefix = text[cursor..start].trim();
        if !prefix.is_empty() {
            segments.push((current_is_thought, prefix.to_string()));
        }
        current_is_thought = true;
        cursor = end;
    }

    let tail = text[cursor..].trim();
    if !tail.is_empty() {
        segments.push((current_is_thought, tail.to_string()));
    }

    if segments.is_empty() {
        segments.push((false, text.trim().to_string()));
    }

    segments
}

fn find_thought_marker(text: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut i = from;

    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }

        let mut j = i + 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }

        if !ascii_starts_with_ignore_case(&bytes[j..], b"thought") {
            i += 1;
            continue;
        }
        j += "thought".len();

        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b':' {
            i += 1;
            continue;
        }
        j += 1;

        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if !ascii_starts_with_ignore_case(&bytes[j..], b"true") {
            i += 1;
            continue;
        }
        j += "true".len();

        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b']' {
            i += 1;
            continue;
        }

        return Some((i, j + 1));
    }

    None
}

fn ascii_starts_with_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack[..needle.len()]
            .iter()
            .zip(needle.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

// ---------------------------------------------------------------------------
// Minimal JSON helpers (no external dependencies)
// ---------------------------------------------------------------------------

/// Extract a JSON string field value by key from a flat JSON object line.
fn json_str(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let pos = json.find(needle.as_str())?;
    let after_key = &json[pos + needle.len()..];
    let colon = after_key.find(':')? + 1;
    let rest = after_key[colon..].trim_start();
    if rest.starts_with('"') {
        parse_json_string(&rest[1..])
    } else {
        None
    }
}

/// Parse a JSON string starting after the opening quote, handling \n, \t, \", \\.
fn parse_json_string(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.chars();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
}

/// Extract the raw text of a JSON object value for `key` (returns the braces and interior).
fn json_object_raw(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let pos = json.find(needle.as_str())?;
    let after_key = &json[pos + needle.len()..];
    let colon = after_key.find(':')? + 1;
    let rest = after_key[colon..].trim_start();
    if !rest.starts_with('{') {
        return None;
    }
    // Find matching closing brace.
    let mut depth = 0usize;
    let mut end = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end == 0 {
        None
    } else {
        Some(rest[..end].to_string())
    }
}

/// Turn a JSON object's key-value pairs into a "key=value ..." summary string.
fn params_kv_string(obj: &str) -> String {
    if obj.len() < 2 {
        return String::new();
    }
    let inner = &obj[1..obj.len().saturating_sub(1)];
    let mut parts: Vec<String> = Vec::new();
    let mut rest = inner;
    while let Some(key) = json_str(&format!("{{{}}}", rest), "") // parse first key
        .or_else(|| {
            // fallback: scan for "key":
            let ki = rest.find('"')?;
            let after = &rest[ki + 1..];
            let ke = after.find('"')?;
            Some(after[..ke].to_string())
        })
    {
        let kn = format!("\"{}\"", key);
        let Some(kpos) = rest.find(kn.as_str()) else {
            break;
        };
        rest = &rest[kpos + kn.len()..];
        let Some(cp) = rest.find(':') else { break };
        rest = rest[cp + 1..].trim_start();
        let val: String = if rest.starts_with('"') {
            let v = parse_json_string(&rest[1..]).unwrap_or_default();
            let vlen = v.len() + 2 + v.chars().filter(|&c| c == '"' || c == '\\').count();
            rest = &rest[vlen.min(rest.len())..];
            v.chars().take(40).collect()
        } else {
            let end = rest.find([',', '}', ']'].as_ref()).unwrap_or(rest.len());
            let v = rest[..end].trim().to_string();
            rest = &rest[end.min(rest.len())..];
            v
        };
        parts.push(format!("{}={}", key, val));
        if let Some(comma) = rest.find(',') {
            rest = &rest[comma + 1..];
        } else {
            break;
        }
    }
    parts.join(" ")
}

fn tool_display_name(tool_name: &str, params_raw: &str) -> String {
    let base = prettify_tool_name(tool_name);

    // Append the primary path param if present.
    for key in &["file_path", "path", "dir_path"] {
        let needle = format!("\"{}\"", key);
        if let Some(_) = params_raw.find(needle.as_str()) {
            if let Some(path) = json_str(
                &format!("{{{}}}", &params_raw[1..params_raw.len().saturating_sub(1)]),
                key,
            ) {
                let filename = path.rsplit('/').next().unwrap_or(&path);
                return format!("{} {}", base, filename);
            }
        }
    }
    base
}

fn human_raw_cmd(tool_name: &str, params_raw: &str) -> String {
    let lower = tool_name.to_lowercase();
    let base = prettify_tool_name(tool_name);

    let file_path = param_value(params_raw, "file_path");
    let path = param_value(params_raw, "path");
    let dir_path = param_value(params_raw, "dir_path");
    let src_path = param_value(params_raw, "source_path");
    let dst_path = param_value(params_raw, "destination_path");
    let command = first_present(
        params_raw,
        &["command", "cmd", "shell_command", "bash_command", "script"],
    );
    let query = first_present(params_raw, &["query", "pattern", "search_term", "regex"]);
    let content = first_present(params_raw, &["content", "text", "replacement", "new_text"]);
    let file_target = file_path.as_deref().or(path.as_deref());
    let dir_target = dir_path.as_deref().or(path.as_deref());
    let any_target = file_path
        .as_deref()
        .or(path.as_deref())
        .or(dir_path.as_deref());

    if lower.contains("run")
        || lower.contains("exec")
        || lower.contains("shell")
        || lower.contains("bash")
        || lower.contains("command")
    {
        return command.unwrap_or_else(|| fallback_with_params(&base, params_raw));
    }

    if lower.contains("move") {
        return match (src_path, dst_path) {
            (Some(src), Some(dst)) => format!("Move {src} -> {dst}"),
            _ => fallback_with_params(&base, params_raw),
        };
    }

    if lower.contains("delete") {
        return match any_target {
            Some(target) => format!("Delete {target}"),
            None => fallback_with_params(&base, params_raw),
        };
    }

    if lower.contains("create") {
        return match any_target {
            Some(target) => format!("Create {target}"),
            None => fallback_with_params(&base, params_raw),
        };
    }

    if lower.contains("write") || lower.contains("edit") || lower.contains("replace") {
        return match file_target {
            Some(target) => {
                let suffix = content
                    .as_deref()
                    .map(compact_snippet)
                    .filter(|s| !s.is_empty())
                    .map(|snippet| format!(" ({snippet})"))
                    .unwrap_or_default();
                format!("Write {target}{suffix}")
            }
            None => fallback_with_params(&base, params_raw),
        };
    }

    if lower.contains("read") || lower.contains("get") {
        return match file_target {
            Some(target) => format!("Read {target}"),
            None => fallback_with_params(&base, params_raw),
        };
    }

    if lower.contains("list") {
        return match dir_target {
            Some(target) => format!("List {target}"),
            None => fallback_with_params(&base, params_raw),
        };
    }

    if lower.contains("search") || lower.contains("find") {
        return match query {
            Some(q) => {
                if let Some(target) = any_target {
                    format!("Search {target} for {}", compact_snippet(&q))
                } else {
                    format!("Search for {}", compact_snippet(&q))
                }
            }
            None => fallback_with_params(&base, params_raw),
        };
    }

    fallback_with_params(&base, params_raw)
}

fn prettify_tool_name(tool_name: &str) -> String {
    tool_name
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn param_value(params_raw: &str, key: &str) -> Option<String> {
    if !(params_raw.starts_with('{') && params_raw.ends_with('}')) {
        return None;
    }
    json_str(
        &format!("{{{}}}", &params_raw[1..params_raw.len() - 1]),
        key,
    )
    .filter(|s| !s.is_empty())
}

fn first_present(params_raw: &str, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| param_value(params_raw, key))
}

fn compact_snippet(value: &str) -> String {
    let single_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= 48 {
        return single_line;
    }
    let shortened: String = single_line.chars().take(45).collect();
    format!("{shortened}...")
}

fn fallback_with_params(base: &str, params_raw: &str) -> String {
    let params = params_kv_string(params_raw);
    if params.is_empty() {
        base.to_string()
    } else {
        format!("{base}: {params}")
    }
}

fn gemini_category(tool_name: &str) -> String {
    let n = tool_name.to_lowercase();
    if n.contains("list") {
        return "list".into();
    }
    if n.contains("read") || n.contains("get") {
        return "read".into();
    }
    if n.contains("search") || n.contains("find") {
        return "search".into();
    }
    if n.contains("move") || n.contains("rename") {
        return "move".into();
    }
    if n.contains("delete") || n.contains("remove") {
        return "delete".into();
    }
    if n.contains("create") || n.contains("mkdir") {
        return "create".into();
    }
    if n.contains("write") || n.contains("edit") || n.contains("replace") {
        return "write".into();
    }
    if n.contains("run")
        || n.contains("exec")
        || n.contains("shell")
        || n.contains("bash")
        || n.contains("command")
        || n.contains("web")
        || n.contains("fetch")
        || n.contains("http")
    {
        return "run".into();
    }
    "run".into()
}

fn gemini_file_paths(params_raw: &str) -> Vec<String> {
    let mut paths = Vec::new();
    // Extract the inner part of the params object to scan keys
    let inner = if params_raw.starts_with('{') && params_raw.ends_with('}') {
        &params_raw[1..params_raw.len() - 1]
    } else {
        params_raw
    };
    for key in &[
        "file_path",
        "path",
        "dir_path",
        "source_path",
        "destination_path",
    ] {
        if let Some(p) = json_str(&format!("{{{}}}", inner), key) {
            if !p.is_empty() {
                paths.push(p);
            }
        }
    }
    paths
}
