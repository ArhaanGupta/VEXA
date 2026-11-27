//! Tools Claude can call that run on this PC. Direct data is faster, cheaper and more exact than
//! a web search for questions like stock prices.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::time::Duration;

/// Tool definitions sent with every request.
pub fn definitions() -> Vec<Value> {
    vec![
        json!({
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
        }),
        json!({
            "name": "weather",
            "description": "Current weather and a daily forecast for a place, from national weather \
        models. Use this instead of web search for any weather question.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "place": {
                        "type": "string",
                        "description": "City or town, optionally with region or country, e.g. \
        \"Sydney, Australia\" or \"Delhi\"."
                    },
                    "days": {
                        "type": "integer",
                        "description": "Forecast days to include, 1 to 7."
                    }
                },
                "required": ["place", "days"],
                "additionalProperties": false
            },
            "strict": true
        }),
    ]
}

/// Run a tool. Returns the result text and whether it is an error.
pub fn run(name: &str, input: &Value) -> (String, bool) {
    let result = match name {
        "stock_prices" => input["symbol"]
            .as_str()
            .ok_or_else(|| anyhow!("missing symbol"))
            .and_then(stock_prices),
        "weather" => input["place"]
            .as_str()
            .ok_or_else(|| anyhow!("missing place"))
            .and_then(|p| weather(p, input["days"].as_u64().unwrap_or(1))),
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

fn get_json(url: &str) -> Result<Value> {
    let mut resp = crate::http::agent(Duration::from_secs(10))
        .get(url)
        .call()
        .context("request failed")?;
    let status = resp.status().as_u16();
    let body: Value = resp
        .body_mut()
        .read_json()
        .context("response was not JSON")?;
    if status != 200 {
        return Err(anyhow!("HTTP {status}"));
    }
    Ok(body)
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Weather from Open-Meteo (free, no key). Interim source; commercial use needs their paid plan.
fn weather(place: &str, days: u64) -> Result<String> {
    let days = days.clamp(1, 7);
    let mut parts = place.splitn(2, ',');
    let name = parts.next().unwrap_or("").trim();
    let qualifier = parts.next().map(|q| q.trim().to_lowercase());
    let geo = get_json(&format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=10&language=en&format=json",
        url_encode(name)
    ))
    .context("place lookup failed")?;
    let results = geo["results"]
        .as_array()
        .filter(|r| !r.is_empty())
        .ok_or_else(|| anyhow!("place {place:?} not found"))?;
    // Results are ordered by population; honour "Sydney, Canada" style qualifiers.
    let pick = qualifier
        .as_deref()
        .and_then(|q| {
            results.iter().find(|r| {
                ["country", "admin1", "country_code"]
                    .iter()
                    .any(|k| r[k].as_str().is_some_and(|v| v.to_lowercase().contains(q)))
            })
        })
        .unwrap_or(&results[0]);
    let label = [
        pick["name"].as_str(),
        pick["admin1"].as_str(),
        pick["country"].as_str(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ");
    let forecast = get_json(&format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&timezone=auto&forecast_days={days}\
&current=temperature_2m,apparent_temperature,relative_humidity_2m,precipitation,weather_code,wind_speed_10m\
&daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max,precipitation_sum",
        pick["latitude"], pick["longitude"]
    ))
    .context("forecast request failed")?;
    format_weather(&label, &forecast)
}

/// WMO weather interpretation codes used by Open-Meteo.
fn weather_text(code: i64) -> &'static str {
    match code {
        0 => "clear sky",
        1 => "mainly clear",
        2 => "partly cloudy",
        3 => "overcast",
        45 | 48 => "fog",
        51 | 53 | 55 => "drizzle",
        56 | 57 => "freezing drizzle",
        61 => "light rain",
        63 => "rain",
        65 => "heavy rain",
        66 | 67 => "freezing rain",
        71 => "light snow",
        73 => "snow",
        75 => "heavy snow",
        77 => "snow grains",
        80..=82 => "rain showers",
        85 | 86 => "snow showers",
        95 => "thunderstorm",
        96 | 99 => "thunderstorm with hail",
        _ => "unknown conditions",
    }
}

pub fn format_weather(label: &str, f: &Value) -> Result<String> {
    let c = &f["current"];
    let num = |v: &Value| v.as_f64().map_or("?".to_string(), |x| format!("{x:.0}"));
    let mut lines = vec![format!(
        "{label} (local time zone {}):",
        f["timezone"].as_str().unwrap_or("?")
    )];
    if c.is_object() {
        lines.push(format!(
            "Now ({}): {}, {}°C (feels like {}°C), humidity {}%, wind {} km/h, precipitation {} mm",
            c["time"].as_str().unwrap_or("?"),
            weather_text(c["weather_code"].as_i64().unwrap_or(-1)),
            num(&c["temperature_2m"]),
            num(&c["apparent_temperature"]),
            num(&c["relative_humidity_2m"]),
            num(&c["wind_speed_10m"]),
            c["precipitation"].as_f64().unwrap_or(0.0),
        ));
    }
    let d = &f["daily"];
    let dates = d["time"].as_array().context("no forecast returned")?;
    for (i, date) in dates.iter().enumerate() {
        lines.push(format!(
            "{}: {}, {} to {}°C, {}% chance of rain, {} mm",
            date.as_str().unwrap_or("?"),
            weather_text(d["weather_code"][i].as_i64().unwrap_or(-1)),
            num(&d["temperature_2m_min"][i]),
            num(&d["temperature_2m_max"][i]),
            num(&d["precipitation_probability_max"][i]),
            d["precipitation_sum"][i].as_f64().unwrap_or(0.0),
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
    fn formats_weather() {
        let f = json!({
            "timezone": "Australia/Sydney",
            "current": { "time": "2026-11-27T19:00", "temperature_2m": 22.4, "apparent_temperature": 23.1,
                "relative_humidity_2m": 60, "precipitation": 0.0, "weather_code": 2, "wind_speed_10m": 14.2 },
            "daily": { "time": ["2026-11-27"], "weather_code": [61], "temperature_2m_max": [26.0],
                "temperature_2m_min": [18.2], "precipitation_probability_max": [70], "precipitation_sum": [3.4] }
        });
        let out = format_weather("Sydney, New South Wales, Australia", &f).unwrap();
        assert!(
            out.contains("Now (2026-11-27T19:00): partly cloudy, 22°C"),
            "{out}"
        );
        assert!(
            out.contains("2026-11-27: light rain, 18 to 26°C, 70% chance of rain, 3.4 mm"),
            "{out}"
        );
    }

    #[test]
    fn encodes_place_names() {
        assert_eq!(url_encode("São Paulo"), "S%C3%A3o%20Paulo");
    }

    #[test]
    fn unknown_tool_is_an_error() {
        assert!(run("nope", &json!({})).1);
    }
}
