//! Editions: the same app tuned for different kinds of user. An edition decides the instructions,
//! which tools Claude may use, which sources are trusted, the model and effort, and the vocabulary
//! hints for speech recognition.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edition {
    #[default]
    General,
    /// Australian law: answers only from official, current sources, always with a citation.
    LegalAu,
}

impl Edition {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub const ALL: &'static [Edition] = &[Edition::General, Edition::LegalAu];

    /// Name shown in the tray and on the status panel.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn label(self) -> &'static str {
        match self {
            Edition::General => "General",
            Edition::LegalAu => "Legal (Australia)",
        }
    }

    /// Question used by "Test Claude connection" in the tray.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn test_question(self) -> &'static str {
        match self {
            Edition::General => {
                "What is today's date, and what is the weather in Sydney right now?"
            }
            Edition::LegalAu => {
                "What is the time limit to lodge an unfair dismissal application under the Fair Work Act?"
            }
        }
    }

    /// System prompt for this edition.
    pub fn system_prompt(self) -> &'static str {
        match self {
            Edition::General => GENERAL_PROMPT,
            Edition::LegalAu => LEGAL_AU_PROMPT,
        }
    }

    /// Our own tools (see `tools.rs`) this edition may use.
    pub fn tools(self) -> &'static [&'static str] {
        match self {
            Edition::General => &["stock_prices", "weather", "fetch_page"],
            Edition::LegalAu => &["fetch_page"],
        }
    }

    /// Hosts that search and page reading are limited to (subdomains included), or `None` for
    /// any reputable source.
    pub fn trusted_domains(self) -> Option<&'static [&'static str]> {
        match self {
            Edition::General => None,
            Edition::LegalAu => Some(LEGAL_AU_SOURCES),
        }
    }

    /// Web searches allowed per request.
    pub fn max_searches(self) -> u32 {
        match self {
            Edition::General => 3,
            // Finding the provision and confirming it is current can take more than one search.
            Edition::LegalAu => 5,
        }
    }

    /// Model used unless the user sets one in settings.
    pub fn default_model(self) -> &'static str {
        match self {
            Edition::General => "claude-sonnet-5-5",
            // Accuracy matters more than cost for legal answers.
            Edition::LegalAu => "claude-opus-5-5",
        }
    }

    /// Thinking effort: low keeps everyday voice answers fast.
    pub fn effort(self) -> &'static str {
        match self {
            Edition::General => "low",
            // Enough reasoning to check the provision, jurisdiction and currency of the law.
            Edition::LegalAu => "medium",
        }
    }

    /// Example text that biases speech recognition towards this edition's vocabulary.
    pub fn stt_prompt(self) -> &'static str {
        match self {
            Edition::General => {
                "Hey Vexa, what's the weather in Sydney? Why was that, Vexa? \
Vexa, what did Nvidia, Apple, Microsoft, Tesla, Amazon, Google and AMD close at on Friday?"
            }
            Edition::LegalAu => {
                "Vexa, what's the limitation period for negligence in New South Wales? Vexa, under \
section 394 of the Fair Work Act, the Corporations Act, the Competition and Consumer Act, the Privacy \
Act and the Civil Liability Act, what are the time limits? High Court, Federal Court, Fair Work \
Commission, NCAT, VCAT, plaintiff, defendant, affidavit, subpoena, injunction, estoppel, tort, \
statutory declaration, unfair dismissal."
            }
        }
    }
}

const GENERAL_PROMPT: &str = "You are a voice assistant running in the background on the user's computer. \
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
Choosing where the answer comes from. Use the cheapest source that is reliable, in this order:\n\
1. Your own knowledge, for things that do not change: definitions, history, science, geography, \
maths, unit conversions, how-to. No tools.\n\
2. The current date and time (given after the question), for time zones, dates and countdowns. No tools.\n\
3. A direct data tool: stock_prices for stocks, indices, ETFs, crypto and currency exchange rates; \
weather for weather. These are exact, fast and nearly free.\n\
4. fetch_page, when you can name a specific authoritative page or public API that holds the \
answer (a Wikipedia summary, an official site, a government or company page).\n\
5. web_search, only for news, recent events, the reasons behind a move, or when you do not know \
where the answer lives. It costs the most, so use it last.\n\
\n\
Accuracy:\n\
- Trust only reputable sources: official and primary sources (exchanges, company filings and \
investor relations, government agencies), established news organisations (Reuters, AP, Bloomberg, \
Financial Times, Wall Street Journal, CNBC, BBC, New York Times) and established data providers \
(Nasdaq, NYSE, Yahoo Finance, Google Finance, MarketWatch). Ignore content farms, \
aggregators, blogs, forums and trading-app marketing sites.\n\
- For numbers, prefer a source that states the figure directly. If reputable sources disagree or \
you could only find a weak source, say so in a few words instead of guessing.\n\
- Be quick: one tool call is usually enough. Do not write anything before or between tool calls.\n\
\n\
The current local date and time are given in a system message after the user's question.";

/// Official Australian legal sources: legislation registers, courts and tribunals, Parliament,
/// regulators and the free legal information institutes. Subdomains are included.
const LEGAL_AU_SOURCES: &[&str] = &[
    // Legislation registers (Commonwealth, states and territories)
    "legislation.gov.au",
    "legislation.nsw.gov.au",
    "legislation.vic.gov.au",
    "legislation.qld.gov.au",
    "legislation.wa.gov.au",
    "legislation.sa.gov.au",
    "legislation.tas.gov.au",
    "legislation.act.gov.au",
    "legislation.nt.gov.au",
    // Case law and legal information institutes
    "austlii.edu.au",
    "jade.io",
    "caselaw.nsw.gov.au",
    "sclqld.org.au",
    // Courts and tribunals
    "hcourt.gov.au",
    "fedcourt.gov.au",
    "fcfcoa.gov.au",
    "art.gov.au",
    "fwc.gov.au",
    "supremecourt.justice.nsw.gov.au",
    "ncat.nsw.gov.au",
    "supremecourt.vic.gov.au",
    "vcat.vic.gov.au",
    "courts.qld.gov.au",
    // Parliament (bills, explanatory memoranda)
    "aph.gov.au",
    "parliament.nsw.gov.au",
    // Government departments and regulators
    "ag.gov.au",
    "alrc.gov.au",
    "ato.gov.au",
    "asic.gov.au",
    "accc.gov.au",
    "fairwork.gov.au",
    "oaic.gov.au",
    "apra.gov.au",
    "austrac.gov.au",
    "ipaustralia.gov.au",
    "homeaffairs.gov.au",
    "fairtrading.nsw.gov.au",
    "consumer.vic.gov.au",
];

const LEGAL_AU_PROMPT: &str = "You are a legal research assistant for Australian law, running in the \
background on the user's computer. The user asked out loud while working. Your reply is read aloud \
and shown in a small card with its sources.\n\
\n\
How to answer:\n\
- Give the legal rule in one or two short sentences, naming the source in spoken form, e.g. \
\"Under section 394 of the Fair Work Act 2009 (Commonwealth), an unfair dismissal application must \
be made within 21 days after the dismissal takes effect.\" Exact figures, time limits and thresholds.\n\
- No background or commentary unless asked; the user can ask a follow-up for detail.\n\
- Plain text only: no markdown, lists, URLs or emoji.\n\
- The request comes from speech recognition and may contain mis-heard words; interpret it charitably.\n\
- Never say your own name. Do not write anything before or between tool calls.\n\
\n\
Jurisdiction:\n\
- Australian law only. Commonwealth law applies everywhere. Where state or territory law governs, \
use the state the user names; otherwise answer for New South Wales and say \"In New South Wales\" \
in the answer.\n\
\n\
Accuracy comes first. These rules override brevity and cost:\n\
- Never answer a legal question from memory alone. Verify every provision, time limit, amount and \
case against an official source during this conversation: find it with web_search, then read the \
exact provision with fetch_page (use `find` with the section number or heading on long Acts).\n\
- Only cite a section, case or figure you have actually read in a source in this conversation. \
Never invent or guess a citation, case name, section number or amount.\n\
- Use the current law. Prefer the authorised current version on the official legislation register \
(legislation.gov.au or the state register), then AustLII. Check the version or compilation date; if \
a provision was recently amended, repealed or is not yet in force, say so briefly.\n\
- For cases, prefer the judgment itself (AustLII, JADE or the court's site). If you see that a \
decision was overturned or distinguished on appeal, say so.\n\
- If official sources disagree, or you cannot verify the answer, say \"I couldn't verify that in \
an official source\" and what you did find. Do not fill gaps with assumptions.\n\
- This is legal information. Only if the user asks what to do about their own situation, add one \
short line suggesting they confirm with a lawyer.\n\
\n\
The current local date and time are given in a system message after the user's question.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editions_round_trip_through_settings() {
        for &e in Edition::ALL {
            let json = serde_json::to_string(&e).unwrap();
            assert_eq!(serde_json::from_str::<Edition>(&json).unwrap(), e);
            assert!(!e.label().is_empty() && !e.test_question().is_empty());
        }
        assert_eq!(
            serde_json::to_string(&Edition::LegalAu).unwrap(),
            "\"legal_au\""
        );
    }

    #[test]
    fn legal_sources_fit_the_search_tool_limit() {
        let d = LEGAL_AU_SOURCES;
        assert!(!d.is_empty() && d.len() <= 64);
        assert!(d.iter().all(|h| !h.contains('/') && !h.starts_with("www.")));
    }
}
