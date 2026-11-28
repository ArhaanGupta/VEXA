//! Minimal Claude Messages API client (raw HTTP: there is no official Rust SDK).
//!
//! One request per voice command. Nothing is cached or logged; the request and answer exist only
//! in memory for the duration of the call.

use crate::edition::Edition;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::Duration;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Server-side tool loops can pause; resume at most this many times.
const MAX_CONTINUATIONS: usize = 4;

pub struct Claude {
    agent: ureq::Agent,
    api_key: String,
    model: String,
    edition: Edition,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    /// Plain text, suitable to speak aloud.
    pub text: String,
    pub usage: Usage,
}

/// Tokens and searches used by one answer (summed over resumed tool loops).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub searches: u64,
    pub cost_usd: f64,
    /// Tools used, in order, without repeats (e.g. "stock_prices", "web_search").
    pub sources: Vec<String>,
}

/// List prices in USD per million tokens (input, output) for standard requests.
fn price_per_mtok(model: &str) -> (f64, f64) {
    match model {
        m if m.starts_with("claude-haiku-5-5") => (0.10, 0.50),
        m if m.starts_with("claude-sonnet-5-5") => (2.0, 10.0),
        m if m.starts_with("claude-opus-5-5") => (4.0, 20.0),
        _ => (5.0, 25.0),
    }
}

/// Web search is billed per search on top of tokens ($10 per 1,000).
const SEARCH_PRICE_USD: f64 = 0.01;

impl Usage {
    pub fn add(&mut self, model: &str, usage: &Value) {
        let n = |v: &Value| v.as_u64().unwrap_or(0);
        let fresh = n(&usage["input_tokens"]);
        let cache_read = n(&usage["cache_read_input_tokens"]);
        let cache_write = n(&usage["cache_creation_input_tokens"]);
        let output = n(&usage["output_tokens"]);
        let searches = n(&usage["server_tool_use"]["web_search_requests"]);
        let (pin, pout) = price_per_mtok(model);
        self.input_tokens += fresh + cache_read + cache_write;
        self.output_tokens += output;
        self.searches += searches;
        // Cache reads bill at 0.1x the input price, 5-minute cache writes at 1.25x.
        let input_cost = fresh as f64 + cache_read as f64 * 0.1 + cache_write as f64 * 1.25;
        self.cost_usd += input_cost * pin / 1e6
            + output as f64 * pout / 1e6
            + searches as f64 * SEARCH_PRICE_USD;
    }

    /// Record which tools a response used.
    pub fn note_sources(&mut self, content: &Value) {
        for b in content.as_array().into_iter().flatten() {
            let kind = b["type"].as_str().unwrap_or("");
            if (kind == "tool_use" || kind == "server_tool_use")
                && let Some(name) = b["name"].as_str()
                && !self.sources.iter().any(|s| s == name)
            {
                self.sources.push(name.to_string());
            }
        }
    }

    /// Short summary, e.g. "12.3k in · 85 out · 2 searches · ≈$0.07".
    pub fn summary(&self) -> String {
        let k = |t: u64| {
            if t >= 1000 {
                format!("{:.1}k", t as f64 / 1000.0)
            } else {
                t.to_string()
            }
        };
        let via = if self.sources.is_empty() {
            "no tools".to_string()
        } else {
            format!("via {}", self.sources.join(", "))
        };
        format!(
            "{via} · {} in · {} out · {} search{} · \u{2248}${:.3}",
            k(self.input_tokens),
            k(self.output_tokens),
            self.searches,
            if self.searches == 1 { "" } else { "es" },
            self.cost_usd
        )
    }
}

#[derive(Debug)]
pub enum ClaudeError {
    InvalidKey,
    RateLimited,
    Refused,
    Other(anyhow::Error),
}

impl std::fmt::Display for ClaudeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClaudeError::InvalidKey => write!(
                f,
                "Your Anthropic API key was rejected. Open Vexa settings to update it."
            ),
            ClaudeError::RateLimited => {
                write!(f, "Claude is busy right now. Try again in a moment.")
            }
            ClaudeError::Refused => write!(f, "Sorry, I can't help with that one."),
            ClaudeError::Other(e) => write!(f, "Something went wrong: {e}"),
        }
    }
}

/// Recent questions and answers, so follow-ups like "why was that?" have context.
/// Held in memory only and forgotten after a few minutes of inactivity.
pub struct Conversation {
    turns: Vec<(String, String)>,
    last: Option<std::time::Instant>,
}

impl Conversation {
    const MAX_TURNS: usize = 6;
    const EXPIRY: Duration = Duration::from_secs(5 * 60);

    pub const fn new() -> Self {
        Self {
            turns: Vec::new(),
            last: None,
        }
    }

    fn expire(&mut self, now: std::time::Instant) {
        if self
            .last
            .is_some_and(|t| now.duration_since(t) > Self::EXPIRY)
        {
            self.turns.clear();
        }
    }

    /// Earlier turns that are still fresh enough to use as context.
    pub fn history(&mut self, now: std::time::Instant) -> Vec<(String, String)> {
        self.expire(now);
        self.turns.clone()
    }

    pub fn record(&mut self, question: String, answer: String, now: std::time::Instant) {
        self.expire(now);
        self.turns.push((question, answer));
        if self.turns.len() > Self::MAX_TURNS {
            self.turns.remove(0);
        }
        self.last = Some(now);
    }
}

/// Build the message list: earlier turns as plain text, then the new question, then the current
/// time as a system message. The time lives here rather than in the system prompt so the system
/// prompt and tools stay byte-identical between requests and can be cached.
pub fn build_messages(
    history: &[(String, String)],
    request: &str,
    now: chrono::DateTime<chrono::Local>,
) -> Vec<Value> {
    let mut messages = Vec::with_capacity(history.len() * 2 + 2);
    for (q, a) in history {
        messages.push(json!({ "role": "user", "content": q }));
        messages.push(json!({ "role": "assistant", "content": a }));
    }
    messages.push(json!({ "role": "user", "content": request }));
    messages.push(json!({
        "role": "system",
        "content": format!(
            "Current local date and time: {} (UTC{}).",
            now.format("%A %-d %B %Y, %-I:%M %p"),
            now.format("%:z"),
        ),
    }));
    messages
}

impl Claude {
    pub fn new(api_key: String, edition: Edition, model: &str) -> Self {
        let agent = crate::http::agent(Duration::from_secs(120));
        Self {
            agent,
            api_key,
            model: model.to_string(),
            edition,
        }
    }

    /// Haiku has no server-side refusal fallback and only the basic web search tool.
    fn is_haiku(&self) -> bool {
        self.model.starts_with("claude-haiku")
    }

    pub fn build_request(&self, messages: &[Value], system: &str) -> Value {
        let search_tool = if self.is_haiku() {
            "web_search_20250305"
        } else {
            // Filters search results with code before they enter the context: fewer tokens.
            "web_search_20260209"
        };
        let mut tools = crate::tools::definitions(self.edition.tools());
        let mut search = json!({ "type": search_tool, "name": "web_search", "max_uses": 3 });
        if let Some(domains) = self.edition.trusted_domains() {
            search["allowed_domains"] = json!(domains);
        }
        tools.push(search);
        let mut body = json!({
            "model": self.model,
            "max_tokens": 16000,
            // Cache the tools and system prompt: they are identical on every request.
            "system": [{ "type": "text", "text": system, "cache_control": { "type": "ephemeral" } }],
            "output_config": { "effort": self.edition.effort() },
            "tools": tools,
            "messages": messages,
        });
        if !self.is_haiku() {
            // Route around a safety-classifier false positive instead of failing the request.
            body["fallbacks"] = json!("default");
        }
        body
    }

    fn post(&self, body: &Value) -> Result<Value, ClaudeError> {
        let mut req = self
            .agent
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json");
        if body.get("fallbacks").is_some() {
            req = req.header("anthropic-beta", FALLBACK_BETA);
        }
        let mut resp = req
            .send_json(body)
            .map_err(|e| ClaudeError::Other(anyhow!("network error: {e}")))?;
        let status = resp.status().as_u16();
        let value: Value = resp
            .body_mut()
            .read_json()
            .map_err(|e| ClaudeError::Other(anyhow!("bad response (HTTP {status}): {e}")))?;
        match status {
            200 => Ok(value),
            401 | 403 => Err(ClaudeError::InvalidKey),
            429 | 529 => Err(ClaudeError::RateLimited),
            _ => {
                let msg = value["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown error");
                Err(ClaudeError::Other(anyhow!("HTTP {status}: {msg}")))
            }
        }
    }

    /// Ask one question and return a speakable answer.
    /// Ask a question, with earlier turns of the conversation as context.
    pub fn ask(&self, request: &str, history: &[(String, String)]) -> Result<Answer, ClaudeError> {
        let system = self.edition.system_prompt();
        let mut messages = build_messages(history, request, chrono::Local::now());
        let mut usage = Usage::default();
        for _ in 0..=MAX_CONTINUATIONS {
            let resp = self.post(&self.build_request(&messages, system))?;
            usage.add(
                resp["model"].as_str().unwrap_or(&self.model),
                &resp["usage"],
            );
            usage.note_sources(&resp["content"]);
            match resp["stop_reason"].as_str() {
                Some("pause_turn") => {
                    // Resume the server-side tool loop: resend with the partial assistant turn.
                    append_assistant(&mut messages, &resp["content"]);
                    continue;
                }
                Some("tool_use") => {
                    // Run our own tools and send the results back, keeping the assistant turn as is.
                    append_assistant(&mut messages, &resp["content"]);
                    let results: Vec<Value> = resp["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] == "tool_use")
                        .map(|b| {
                            let (text, is_error) = crate::tools::run(
                                b["name"].as_str().unwrap_or(""),
                                &b["input"],
                                self.edition,
                            );
                            json!({
                                "type": "tool_result",
                                "tool_use_id": b["id"],
                                "content": text,
                                "is_error": is_error,
                            })
                        })
                        .collect();
                    messages.push(json!({ "role": "user", "content": results }));
                    continue;
                }
                Some("refusal") => return Err(ClaudeError::Refused),
                _ => {
                    let text = extract_text(&resp).map_err(ClaudeError::Other)?;
                    return Ok(Answer {
                        text: speakable(&text),
                        usage,
                    });
                }
            }
        }
        Err(ClaudeError::Other(anyhow!("the search took too long")))
    }
}

/// Add assistant content to the conversation, continuing the assistant turn if the last message
/// already is one (a resumed `pause_turn`).
fn append_assistant(messages: &mut Vec<Value>, content: &Value) {
    let blocks = content.as_array().cloned().unwrap_or_default();
    if let Some(last) = messages.last_mut()
        && last["role"] == "assistant"
        && let Some(existing) = last["content"].as_array_mut()
    {
        existing.extend(blocks);
        return;
    }
    messages.push(json!({ "role": "assistant", "content": blocks }));
}

/// The final answer of a Messages API response: the text blocks after the last tool call.
/// Text before a search ("I'll look that up.") is narration, not part of the answer.
pub fn extract_text(resp: &Value) -> Result<String> {
    let blocks = resp["content"]
        .as_array()
        .context("response has no content")?;
    let last_tool = blocks.iter().rposition(|b| {
        b["type"]
            .as_str()
            .is_some_and(|t| t != "text" && t != "thinking")
    });
    let start = last_tool.map_or(0, |i| i + 1);
    let text: String = blocks[start..]
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    let text = text.trim().to_string();
    if text.is_empty() {
        bail!("Claude returned an empty answer");
    }
    Ok(text)
}

/// Remove leftover markdown so the answer reads and sounds clean.
pub fn speakable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let line = line
            .trim_start_matches(['#', '>', ' '])
            .trim_start_matches("- ")
            .trim_start_matches("* ");
        if line.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(line);
    }
    out.replace("**", "").replace("__", "").replace('`', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_text_after_search_blocks() {
        let resp = json!({
            "stop_reason": "end_turn",
            "content": [
                { "type": "server_tool_use", "id": "x", "name": "web_search", "input": { "query": "nvidia close" } },
                { "type": "web_search_tool_result", "tool_use_id": "x", "content": [] },
                { "type": "text", "text": "Nvidia closed at " },
                { "type": "text", "text": "$181.20 on Friday.", "citations": [] }
            ]
        });
        assert_eq!(
            extract_text(&resp).unwrap(),
            "Nvidia closed at $181.20 on Friday."
        );
    }

    #[test]
    fn drops_narration_before_searches() {
        let resp = json!({
            "content": [
                { "type": "text", "text": "Monday was 23 November. I'll search." },
                { "type": "server_tool_use", "id": "a", "name": "web_search", "input": {} },
                { "type": "web_search_tool_result", "tool_use_id": "a", "content": [] },
                { "type": "text", "text": "Let me double-check." },
                { "type": "server_tool_use", "id": "b", "name": "web_search", "input": {} },
                { "type": "web_search_tool_result", "tool_use_id": "b", "content": [] },
                { "type": "text", "text": "Nvidia closed at $165.05 on Monday." }
            ]
        });
        assert_eq!(
            extract_text(&resp).unwrap(),
            "Nvidia closed at $165.05 on Monday."
        );
    }

    #[test]
    fn conversation_keeps_recent_turns_in_order() {
        let t0 = std::time::Instant::now();
        let mut c = Conversation::new();
        c.record("price of nvidia".into(), "165 dollars".into(), t0);
        let msgs = build_messages(&c.history(t0), "why was that?", chrono::Local::now());
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["content"], "price of nvidia");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "why was that?");
        assert_eq!(msgs[3]["role"], "system");
        assert!(
            msgs[3]["content"]
                .as_str()
                .unwrap()
                .starts_with("Current local date")
        );
    }

    #[test]
    fn conversation_expires_and_is_capped() {
        let t0 = std::time::Instant::now();
        let mut c = Conversation::new();
        for i in 0..10 {
            c.record(format!("q{i}"), format!("a{i}"), t0);
        }
        assert_eq!(c.history(t0).len(), Conversation::MAX_TURNS);
        assert_eq!(c.history(t0)[0].0, "q4");
        let later = t0 + Conversation::EXPIRY + Duration::from_secs(1);
        assert!(c.history(later).is_empty());
    }

    #[test]
    fn usage_cost_includes_tokens_and_searches() {
        let mut u = Usage::default();
        u.add(
            "claude-opus-5-5",
            &json!({ "input_tokens": 1_000_000, "output_tokens": 1_000, "server_tool_use": { "web_search_requests": 2 } }),
        );
        assert!(
            (u.cost_usd - (4.0 + 0.02 + 0.02)).abs() < 1e-9,
            "{}",
            u.cost_usd
        );
        assert_eq!(
            u.summary(),
            "no tools · 1000.0k in · 1.0k out · 2 searches · \u{2248}$4.040"
        );
        u.note_sources(&json!([
            { "type": "tool_use", "name": "stock_prices" },
            { "type": "server_tool_use", "name": "web_search" },
            { "type": "tool_use", "name": "stock_prices" }
        ]));
        assert!(
            u.summary().starts_with("via stock_prices, web_search · "),
            "{}",
            u.summary()
        );
    }

    #[test]
    fn haiku_request_uses_basic_search_and_no_fallbacks() {
        let c = Claude::new("k".into(), Edition::General, "claude-haiku-5-5");
        let body = c.build_request(&[json!({ "role": "user", "content": "hi" })], "sys");
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.last().unwrap()["type"], "web_search_20250305");
        assert!(body.get("fallbacks").is_none());
    }

    #[test]
    fn resumed_turn_extends_the_assistant_message() {
        let mut m = vec![json!({ "role": "user", "content": "q" })];
        append_assistant(&mut m, &json!([{ "type": "text", "text": "a" }]));
        append_assistant(&mut m, &json!([{ "type": "text", "text": "b" }]));
        assert_eq!(m.len(), 2);
        assert_eq!(m[1]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn cached_tokens_are_discounted() {
        let mut u = Usage::default();
        u.add(
            "claude-sonnet-5-5",
            &json!({ "input_tokens": 0, "cache_read_input_tokens": 1_000_000, "output_tokens": 0 }),
        );
        assert!((u.cost_usd - 0.2).abs() < 1e-9, "{}", u.cost_usd);
    }

    #[test]
    fn empty_answer_is_an_error() {
        assert!(extract_text(&json!({ "content": [] })).is_err());
    }

    #[test]
    fn strips_markdown() {
        assert_eq!(
            speakable("**Best option:**\n- Qantas, $940\n- Air India, $820"),
            "Best option: Qantas, $940 Air India, $820"
        );
    }

    #[test]
    fn request_shape() {
        let c = Claude::new("k".into(), Edition::General, "claude-sonnet-5-5");
        let body = c.build_request(&[json!({ "role": "user", "content": "hi" })], "sys");
        assert_eq!(body["model"], "claude-sonnet-5-5");
        assert_eq!(body["tools"][0]["name"], "stock_prices");
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.last().unwrap()["type"], "web_search_20260209");
        assert_eq!(body["output_config"]["effort"], "low");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(body.get("thinking").is_none());
        assert!(body.get("temperature").is_none());
    }
}
