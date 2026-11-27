//! Tools Claude can call that run on this PC. Direct data is faster, cheaper and more exact than
//! a web search for questions like stock prices.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::time::Duration;

/// Tool definitions sent with every request.
pub fn definitions() -> Vec<Value> {
    vec![json!({
        "name": "stock_prices",
        "description": "Daily closing prices for the last few weeks, plus the latest price, for a \
    stock, ETF, index or cryptocurrency, from market data. Use this instead of web search for any price \
    or closing-price question. Dates are in the exchange's local time zone.",
        "input_schema": {
            "type": "object",
            "properties": {
                "symbol": {
                    "type": "string",
                    "description": "Ticker in Yahoo Finance format, e.g. NVDA, AAPL, ^GSPC (S&P 500), \
    ^IXIC (Nasdaq), BHP.AX (ASX), RELIANCE.NS (NSE), BTC-USD."
                }
            },
            "required": ["symbol"],
            "additionalProperties": false
        },
        "strict": true
    })]
}

/// Run a tool. Returns the result text and whether it is an error.
pub fn run(name: &str, input: &Value) -> (String, bool) {
    let result = match name {
        "stock_prices" => input["symbol"]
            .as_str()
            .ok_or_else(|| anyhow!("missing symbol"))
            .and_then(stock_prices),
        other => Err(anyhow!("unknown tool {other}")),
    };
    match result {
        Ok(text) => (text, false),
        Err(e) => (format!("{e:#}"), true),
    }
}

fn stock_prices(symbol: &str) -> Result<String> {
    let symbol = symbol.trim().to_uppercase();
    if symbol.is_empty()
        || !symbol
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '^' | '.' | '-' | '='))
    {
        return Err(anyhow!("invalid symbol {symbol:?}"));
    }
    // Interim data source for testing; a licensed market-data provider replaces it before launch.
    let url = format!(
        "https://query1.finance.yahoo.com/v8/finance/chart/{}?range=1mo&interval=1d",
        symbol.replace('^', "%5E")
    );
    let mut resp = crate::http::agent(Duration::from_secs(10))
        .get(&url)
        .header("user-agent", "Mozilla/5.0")
        .call()
        .context("market data request failed")?;
    let status = resp.status().as_u16();
    let body: Value = resp
        .body_mut()
        .read_json()
        .context("market data response was not JSON")?;
    if status != 200 {
        let msg = body["chart"]["error"]["description"]
            .as_str()
            .unwrap_or("request failed");
        return Err(anyhow!(
            "no market data for {symbol} (HTTP {status}: {msg})"
        ));
    }
    format_chart(&symbol, &body)
}

/// Turn a Yahoo chart response into compact lines Claude can read.
pub fn format_chart(symbol: &str, body: &Value) -> Result<String> {
    let r = &body["chart"]["result"][0];
    let meta = &r["meta"];
    let currency = meta["currency"].as_str().unwrap_or("");
    let offset = meta["gmtoffset"].as_i64().unwrap_or(0);
    let tz = meta["exchangeTimezoneName"].as_str().unwrap_or("UTC");
    let times = r["timestamp"]
        .as_array()
        .context("no price history returned")?;
    let closes = r["indicators"]["quote"][0]["close"]
        .as_array()
        .context("no closing prices returned")?;

    let mut lines = vec![format!(
        "{symbol} ({}), prices in {currency}, dates in {tz}:",
        meta["longName"]
            .as_str()
            .or(meta["shortName"].as_str())
            .unwrap_or(symbol)
    )];
    let rows: Vec<String> = times
        .iter()
        .zip(closes)
        .filter_map(|(t, c)| {
            let (t, c) = (t.as_i64()?, c.as_f64()?);
            let day = chrono::DateTime::from_timestamp(t + offset, 0)?;
            Some(format!("{} close {c:.2}", day.format("%a %Y-%m-%d")))
        })
        .collect();
    // The last 12 trading days are plenty for "on Friday" / "last week" questions.
    let skip = rows.len().saturating_sub(12);
    lines.extend(rows.into_iter().skip(skip));
    if let Some(p) = meta["regularMarketPrice"].as_f64() {
        let at = meta["regularMarketTime"]
            .as_i64()
            .and_then(|t| chrono::DateTime::from_timestamp(t + offset, 0))
            .map(|d| d.format(" at %a %Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        lines.push(format!(
            "Latest price {p:.2}{at} (the last row may be today's unfinished session)"
        ));
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_closes_in_exchange_time() {
        // 2026-11-20 21:00 UTC is Friday 16:00 in New York (UTC-5).
        let body = json!({ "chart": { "result": [{
            "meta": {
                "currency": "USD", "gmtoffset": -18000, "exchangeTimezoneName": "America/New_York",
                "longName": "NVIDIA Corporation", "regularMarketPrice": 165.05,
                "regularMarketTime": 1795467600
            },
            "timestamp": [1795208400, 1795467600],
            "indicators": { "quote": [{ "close": [125.53, 165.05] }] }
        }]}});
        let out = format_chart("NVDA", &body).unwrap();
        assert!(
            out.starts_with("NVDA (NVIDIA Corporation), prices in USD"),
            "{out}"
        );
        assert!(out.contains("Fri 2026-11-20 close 125.53"), "{out}");
        assert!(out.contains("Mon 2026-11-23 close 165.05"), "{out}");
        assert!(out.contains("Latest price 165.05"), "{out}");
    }

    #[test]
    fn rejects_odd_symbols() {
        let (msg, is_error) = run("stock_prices", &json!({ "symbol": "NVDA/../x" }));
        assert!(is_error, "{msg}");
    }

    #[test]
    fn unknown_tool_is_an_error() {
        assert!(run("nope", &json!({})).1);
    }
}
