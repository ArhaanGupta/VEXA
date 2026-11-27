//! Minimal Claude Messages API client (raw HTTP: there is no official Rust SDK).
//!
//! One request per voice command. Nothing is cached or logged; the request and answer exist only
//! in memory for the duration of the call.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::Duration;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
pub const MODEL: &str = "claude-opus-5-5";
/// Server-side tool loops can pause; resume at most this many times.
const MAX_CONTINUATIONS: usize = 4;

pub struct Claude {
    agent: ureq::Agent,
    api_key: String,
    model: String,
    effort: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// Plain text, suitable to speak aloud.
    pub text: String,
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

pub fn system_prompt(now: chrono::DateTime<chrono::Local>) -> String {
    format!(
        "You are a voice assistant running in the background on the user's computer. \
The user asked out loud while doing something else. Your reply is read aloud, so it must be short.\n\
\n\
How to answer:\n\
- Answer in ONE short sentence with just the fact asked for, e.g. \"Nvidia closed at 165 dollars \
and 5 cents on Monday.\" Two sentences only if one cannot answer it.\n\
- No background, reasons, sources, caveats or offers of more help unless the user asks. They can \
ask a follow-up (\"why was that?\"), and then you explain.\n\
- If the question has a false premise, answer the closest useful thing in the same sentence, e.g. \
markets are closed at weekends: \"Markets were closed on Sunday; Nvidia closed at ... on Friday.\"\n\
- Plain text only: no markdown, lists, URLs or emoji. Say numbers the way a person would.\n\
- The request comes from speech recognition and may contain mis-heard words; interpret it charitably.\n\
- Never say your own name.\n\
\n\
Accuracy:\n\
- Use web search for anything current: prices, markets, news, weather, flights, sports, schedules.\n\
- Trust only reputable sources: official and primary sources (exchanges, company filings and \
investor relations, government agencies), established news organisations (Reuters, AP, Bloomberg, \
Financial Times, Wall Street Journal, CNBC, BBC, New York Times) and established data providers \
(Nasdaq, NYSE, Yahoo Finance, Google Finance, MarketWatch). Ignore content farms, \
aggregators, blogs, forums and trading-app marketing sites.\n\
- For numbers, prefer a source that states the figure directly. If reputable sources disagree or \
you could only find a weak source, say so in a few words instead of guessing.\n\
- Be quick: one or two searches is usually enough.\n\
\n\
Current local date and time: {} (UTC{}).",
        now.format("%A %-d %B %Y, %-I:%M %p"),
        now.format("%:z"),
    )
}

impl Claude {
    pub fn new(api_key: String) -> Self {
        let agent = crate::http::agent(Duration::from_secs(120));
        Self {
            agent,
            api_key,
            model: MODEL.to_string(),
            effort: "low".to_string(),
        }
    }

    pub fn build_request(&self, messages: &[Value], system: &str) -> Value {
        json!({
            "model": self.model,
            "max_tokens": 16000,
            "system": system,
            // Low effort keeps voice answers fast; web search still runs when needed.
            "output_config": { "effort": self.effort },
            // Route around a safety-classifier false positive instead of failing the request.
            "fallbacks": "default",
            "tools": [
                { "type": "web_search_20260209", "name": "web_search", "max_uses": 3 }
            ],
            "messages": messages,
        })
    }

    fn post(&self, body: &Value) -> Result<Value, ClaudeError> {
        let mut resp = self
            .agent
            .post(API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", FALLBACK_BETA)
            .header("content-type", "application/json")
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
    pub fn ask(&self, request: &str) -> Result<Answer, ClaudeError> {
        let system = system_prompt(chrono::Local::now());
        let mut messages = vec![json!({ "role": "user", "content": request })];
        for _ in 0..=MAX_CONTINUATIONS {
            let resp = self.post(&self.build_request(&messages, &system))?;
            match resp["stop_reason"].as_str() {
                Some("pause_turn") => {
                    // Resume the server-side tool loop: resend with the partial assistant turn.
                    messages.truncate(1);
                    messages
                        .push(json!({ "role": "assistant", "content": resp["content"].clone() }));
                    continue;
                }
                Some("refusal") => return Err(ClaudeError::Refused),
                _ => {
                    let text = extract_text(&resp).map_err(ClaudeError::Other)?;
                    return Ok(Answer {
                        text: speakable(&text),
                    });
                }
            }
        }
        Err(ClaudeError::Other(anyhow!("the search took too long")))
    }
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
        let c = Claude::new("k".into());
        let body = c.build_request(&[json!({ "role": "user", "content": "hi" })], "sys");
        assert_eq!(body["model"], MODEL);
        assert_eq!(body["tools"][0]["type"], "web_search_20260209");
        assert_eq!(body["output_config"]["effort"], "low");
        assert!(body.get("thinking").is_none());
        assert!(body.get("temperature").is_none());
    }
}
