//! MCP server over stdio. Every tool call is forwarded to the daemon.
use crate::client::request;
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const INSTRUCTIONS: &str = "Use shlim instead of the built-in shell. Sessions persist: start long-running or interactive programs with run, then use send/read with the returned id. Output is already compacted; ask for more with read mode=all or grep only when needed.";

fn tools() -> Value {
    let id = json!({"type": "integer"});
    let timeout = json!({"type": "number", "description": "seconds"});
    json!([
        {"name": "run", "description": "Run a shell command in a persistent PTY. Returns when it exits, goes idle, or asks for input. Output with no [#id …] header means it finished with exit 0.",
         "inputSchema": {"type": "object", "properties": {"cmd": {"type": "string"}, "cwd": {"type": "string"}, "timeout": timeout}, "required": ["cmd"]}},
        {"name": "send", "description": "Send input to a session, then return its reply. keys: enter tab esc up down left right backspace ctrl+<letter>.",
         "inputSchema": {"type": "object", "properties": {"id": id, "text": {"type": "string"}, "keys": {"type": "array", "items": {"type": "string"}}, "submit": {"type": "boolean", "description": "press Enter after text"}, "timeout": timeout}, "required": ["id"]}},
        {"name": "read", "description": "Read a session. Default: output since last read. wait: exit|quiet|<regex> blocks first. mode=screen for full-screen apps, mode=all with from/grep for history.",
         "inputSchema": {"type": "object", "properties": {"id": id, "wait": {"type": "string"}, "mode": {"enum": ["new", "screen", "all"]}, "grep": {"type": "string"}, "from": {"type": "integer"}, "limit": {"type": "integer", "description": "max lines; raises the size cap"}, "timeout": timeout}, "required": ["id"]}},
        {"name": "kill", "description": "Stop a session.", "inputSchema": {"type": "object", "properties": {"id": id}, "required": ["id"]}},
        {"name": "list", "description": "List sessions.", "inputSchema": {"type": "object", "properties": {}}}
    ])
}

/// Names agents commonly guess, mapped to ours.
const ALIASES: &[(&str, &str)] = &[
    ("command", "cmd"), ("cmdline", "cmd"), ("script", "cmd"),
    ("dir", "cwd"), ("workdir", "cwd"), ("cd", "cwd"), ("directory", "cwd"),
    ("session", "id"), ("session_id", "id"), ("sessionId", "id"),
    ("input", "text"), ("data", "text"), ("line", "text"),
    ("key", "keys"), ("enter", "submit"),
];

/// Fixes guessable mistakes and rejects the rest with a message that teaches the right call,
/// so a wrong guess never silently starts or pokes the wrong thing.
fn normalize(name: &str, args: &Value) -> Result<Value, String> {
    let tools = tools();
    let Some(tool) = tools.as_array().unwrap().iter().find(|t| t["name"] == name) else { return Err(format!("unknown tool {name:?}")) };
    let props = tool["inputSchema"]["properties"].as_object().unwrap();
    let usage = || format!("{name} takes: {}", props.keys().map(String::as_str).collect::<Vec<_>>().join(", "));
    let mut out = serde_json::Map::new();
    for (k, v) in args.as_object().into_iter().flatten() {
        let key = ALIASES.iter().find(|(from, _)| from == k).map_or(k.as_str(), |(_, to)| to);
        if !props.contains_key(key) { return Err(format!("unknown argument `{k}`; {}", usage())) }
        out.insert(key.to_owned(), v.clone());
    }
    if let Some(id) = out.get("id").and_then(Value::as_str) {
        let n = id.trim_start_matches('#').parse::<u64>().map_err(|_| format!("id must be a number, got {id:?}"))?;
        out.insert("id".into(), json!(n));
    }
    if let Some(k @ Value::String(_)) = out.get("keys").cloned() { out.insert("keys".into(), json!([k])); }
    for req in tool["inputSchema"]["required"].as_array().into_iter().flatten() {
        if !out.contains_key(req.as_str().unwrap()) { return Err(format!("missing `{req}`; {}", usage()).replace('"', "")) }
    }
    if name == "run" && out["cmd"].as_str().is_none_or(|c| c.trim().is_empty()) {
        return Err("cmd is empty: pass the command to run, e.g. {\"cmd\": \"git status\"}".into());
    }
    if name == "send" && ["text", "keys", "submit"].iter().all(|k| !out.contains_key(*k)) {
        return Err("nothing to send: pass text, keys and/or submit".into());
    }
    Ok(Value::Object(out))
}

fn call(params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let mut req = match normalize(name, &params["arguments"]) {
        Ok(r) => r,
        Err(e) => return json!({"content": [{"type": "text", "text": format!("error: {e}")}], "isError": true}),
    };
    req["op"] = json!(name);
    // Default to the agent's project directory, not the daemon's.
    if name == "run" && req["cwd"].is_null() {
        if let Ok(dir) = std::env::current_dir() { req["cwd"] = json!(dir.to_string_lossy()) }
    }
    match request(req) {
        Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
        Err(e) => json!({"content": [{"type": "text", "text": format!("error: {e:#}")}], "isError": true}),
    }
}

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() { continue }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notification
        let result = match msg["method"].as_str().unwrap_or("") {
            "initialize" => Ok(json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "shlim", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => Ok(call(&msg["params"])),
            m => Err(json!({"code": -32601, "message": format!("method not found: {m}")})),
        };
        let resp = match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        };
        writeln!(out, "{resp}")?;
        out.flush()?;
    }
    Ok(())
}
