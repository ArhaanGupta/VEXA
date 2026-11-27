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
        stock, ETF, index, cryptocurrency or currency exchange rate, from market data. Use this instead of web search for any price \
        or closing-price question. Dates are in the exchange's local time zone.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "symbol": {
                        "type": "string",
                        "description": "Ticker in Yahoo Finance format, e.g. NVDA, AAPL, ^GSPC (S&P 500), \
        ^IXIC (Nasdaq), BHP.AX (ASX), RELIANCE.NS (NSE), BTC-USD, AUDINR=X (Australian dollar in rupees)."
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
        "fetch_page" => input["url"]
            .as_str()
            .ok_or_else(|| anyhow!("missing url"))
            .and_then(fetch_page),
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

const PAGE_LIMIT_CHARS: usize = 12_000;

/// Split "https://host:port/path" into (scheme, host, port, rest).
fn split_url(url: &str) -> Result<(&str, String, u16, &str)> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow!("not a full URL"))?;
    let scheme_lc = scheme.to_ascii_lowercase();
    let default_port = match scheme_lc.as_str() {
        "https" => 443,
        "http" => 80,
        _ => return Err(anyhow!("only http and https addresses are allowed")),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains('@') {
        return Err(anyhow!("addresses with user names are not allowed"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || authority.starts_with('[') && h.ends_with(']') => {
            match p.parse::<u16>() {
                Ok(port) => (h, port),
                Err(_) => (authority, default_port),
            }
        }
        _ => (authority, default_port),
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if host.is_empty() {
        return Err(anyhow!("address has no host"));
    }
    Ok((scheme, host, port, &rest[end..]))
}

/// True for addresses on the public internet (not this PC, the local network or reserved ranges).
fn is_public(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..128).contains(&o[1]))
                || (o[0] == 198 && (18..20).contains(&o[1]))
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// Refuse addresses that resolve to anything other than the public internet.
fn check_public(url: &str) -> Result<()> {
    use std::net::ToSocketAddrs;
    let (_, host, port, _) = split_url(url)?;
    if host == "localhost" || host.ends_with(".local") || host.ends_with(".localhost") {
        return Err(anyhow!("local addresses are not allowed"));
    }
    let addrs: Vec<_> = (host.as_str(), port)
        .to_socket_addrs()
        .context("could not resolve the address")?
        .collect();
    if addrs.is_empty() || addrs.iter().any(|a| !is_public(a.ip())) {
        return Err(anyhow!("only public internet addresses are allowed"));
    }
    Ok(())
}

fn fetch_page(url: &str) -> Result<String> {
    let agent = crate::http::agent_with(Duration::from_secs(10), 0);
    let mut current = url.trim().to_string();
    // Follow up to three redirects, checking each target is public too.
    for _ in 0..4 {
        check_public(&current)?;
        let mut resp = agent
            .get(&current)
            .header("user-agent", "Mozilla/5.0 (compatible; Vexa assistant)")
            .header(
                "accept",
                "text/html,application/json,text/plain;q=0.9,*/*;q=0.5",
            )
            .call()
            .context("page request failed")?;
        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow!("redirect without a location"))?;
            current = if location.contains("://") {
                location.to_string()
            } else {
                let (scheme, host, port, _) = split_url(&current)?;
                let path = if location.starts_with('/') {
                    location.to_string()
                } else {
                    format!("/{location}")
                };
                format!("{scheme}://{host}:{port}{path}")
            };
            continue;
        }
        if status != 200 {
            return Err(anyhow!("the page returned HTTP {status}"));
        }
        let is_html = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.contains("html"));
        let raw = resp
            .body_mut()
            .with_config()
            .limit(3 * 1024 * 1024)
            .read_to_string()
            .context("could not read the page")?;
        let text = if is_html { html_to_text(&raw) } else { raw };
        return Ok(format!(
            "Content of {current}:\n{}",
            truncate_chars(&text, PAGE_LIMIT_CHARS)
        ));
    }
    Err(anyhow!("too many redirects"))
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}\n[truncated]", &s[..i]),
        None => s.to_string(),
    }
}

/// Rough HTML-to-text: drops scripts, styles and tags, decodes common entities, keeps line breaks
/// at block elements. Good enough to hand Claude the readable part of a page.
pub fn html_to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 3);
    let mut i = 0;
    let bytes = html.as_bytes();
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Skip whole elements whose content is never readable text.
            let mut skipped = false;
            for tag in ["script", "style", "noscript", "svg", "template", "head"] {
                if lower[i + 1..].starts_with(tag) {
                    let close = format!("</{tag}");
                    i = lower[i..].find(&close).map_or(bytes.len(), |p| i + p);
                    i = lower[i..].find('>').map_or(bytes.len(), |p| i + p + 1);
                    skipped = true;
                    break;
                }
            }
            if skipped {
                continue;
            }
            let end = lower[i..].find('>').map_or(bytes.len(), |p| i + p + 1);
            let tag = &lower[i..end];
            if [
                "<p", "<br", "<div", "<li", "<tr", "<h1", "<h2", "<h3", "<h4", "</p", "</div",
                "</li", "</tr", "</h",
            ]
            .iter()
            .any(|t| tag.starts_with(t))
            {
                out.push('\n');
            } else {
                out.push(' ');
            }
            i = end;
        } else {
            let next = lower[i..].find('<').map_or(bytes.len(), |p| i + p);
            out.push_str(&html[i..next]);
            i = next;
        }
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'");
    decoded
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
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
    fn html_becomes_readable_text() {
        let html = "<html><head><title>x</title><style>p{color:red}</style></head><body>\
<script>var a = '<p>';</script><h1>Nvidia</h1><p>Closed at &amp; up&nbsp;5%</p><div>Next</div></body></html>";
        assert_eq!(html_to_text(html), "Nvidia\nClosed at & up 5%\nNext");
    }

    #[test]
    fn only_public_addresses() {
        use std::net::IpAddr;
        for bad in [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "0.0.0.0",
        ] {
            assert!(!is_public(bad.parse::<IpAddr>().unwrap()), "{bad}");
        }
        for good in ["8.8.8.8", "151.101.1.69", "2606:4700::1111"] {
            assert!(is_public(good.parse::<IpAddr>().unwrap()), "{good}");
        }
        assert!(check_public("http://localhost:8080/").is_err());
        assert!(check_public("https://127.0.0.1/").is_err());
        assert!(check_public("https://user@example.com/").is_err());
        assert!(check_public("file:///C:/Windows/win.ini").is_err());
    }

    #[test]
    fn splits_urls() {
        let (scheme, host, port, rest) = split_url("https://En.Wikipedia.org/api?x=1").unwrap();
        assert_eq!(
            (scheme, host.as_str(), port, rest),
            ("https", "en.wikipedia.org", 443, "/api?x=1")
        );
        assert_eq!(split_url("http://example.com:8080").unwrap().2, 8080);
        assert_eq!(split_url("http://[::1]:81/").unwrap().1, "::1");
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate_chars("héllo", 2), "hé\n[truncated]");
        assert_eq!(truncate_chars("hi", 5), "hi");
    }

    #[test]
    fn unknown_tool_is_an_error() {
        assert!(run("nope", &json!({})).1);
    }
}
