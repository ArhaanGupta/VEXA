//! Decides whether a transcribed utterance is addressed to Vexa, and pulls out the request.
//!
//! The wake word can be anywhere in the sentence ("What's the best flight to Delhi, Vexa, under
//! 1000 dollars?"). Speech recognition mis-hears names, so a few close spellings are accepted.

#[derive(Debug, PartialEq, Eq)]
pub enum Wake {
    /// Not addressed to Vexa.
    None,
    /// Only the wake word was said ("Hey Vexa."), so the next utterance is the request.
    WakeOnly,
    /// A request addressed to Vexa, with the wake word removed.
    Request(String),
}

const FILLERS: &[&str] = &[
    "hey", "hi", "ok", "okay", "yo", "um", "uh", "hmm", "so", "please", "oh",
];
const EXPLICIT: &[&str] = &[
    "vexa", "vexah", "vexer", "vexxa", "vecsa", "vexas", "bexa", "vexo",
];

fn norm(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn is_wake_token(t: &str) -> bool {
    if EXPLICIT.contains(&t) {
        return true;
    }
    // Close spellings that keep the distinctive "x" sound ("vexia", "vixa"), but not real words
    // such as "vexed" (distance 2) or names such as "Vera" (no "x").
    t.len() >= 4 && t.contains('x') && levenshtein(t, "vexa") <= 1
}

/// Strip things speech recognition emits for non-speech, like "[BLANK_AUDIO]" or "(music)".
pub fn clean_transcript(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0i32;
    let mut in_stars = false;
    for c in raw.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = (depth - 1).max(0),
            '*' => in_stars = !in_stars,
            _ if depth == 0 && !in_stars => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn detect(transcript: &str) -> Wake {
    let text = clean_transcript(transcript);
    let words: Vec<&str> = text.split_whitespace().collect();
    let normed: Vec<String> = words.iter().map(|w| norm(w)).collect();

    // Find the wake word, either as one token or split in two ("vex a").
    let mut hit: Option<(usize, usize)> = None;
    for i in 0..normed.len() {
        if is_wake_token(&normed[i]) {
            hit = Some((i, 1));
            break;
        }
        if i + 1 < normed.len()
            && normed[i] == "vex"
            && matches!(normed[i + 1].as_str(), "a" | "ah" | "uh" | "er")
        {
            hit = Some((i, 2));
            break;
        }
    }
    let Some((idx, len)) = hit else {
        return Wake::None;
    };

    // Remove the wake word plus fillers directly before it ("hey vexa").
    let mut start = idx;
    while start > 0 && FILLERS.contains(&normed[start - 1].as_str()) {
        start -= 1;
    }
    // Keep punctuation that was attached to the wake word ("Vexa, what's..." / "...dollars, Vexa?").
    let trailing: String = words[idx + len - 1]
        .chars()
        .rev()
        .take_while(|c| matches!(c, '?' | '.' | '!'))
        .collect();

    let mut rest: Vec<&str> = Vec::with_capacity(words.len());
    rest.extend_from_slice(&words[..start]);
    rest.extend_from_slice(&words[idx + len..]);

    let meaningful = rest
        .iter()
        .map(|w| norm(w))
        .filter(|w| !w.is_empty() && !FILLERS.contains(&w.as_str()))
        .count();
    if meaningful == 0 {
        return Wake::WakeOnly;
    }

    let mut request = rest.join(" ");
    request = request
        .trim_matches(|c: char| c == ',' || c == ' ' || c == '-')
        .replace(" ,", ",")
        .replace(",,", ",");
    if let Some(stripped) = request.strip_suffix(',') {
        request = stripped.to_string();
    }
    if !trailing.is_empty() && !request.ends_with(['?', '.', '!']) {
        request.extend(trailing.chars().rev());
    }
    // Capitalise the first letter for display.
    let mut chars = request.chars();
    let request = match chars.next() {
        Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    };
    Wake::Request(request)
}

/// Short commands that control Vexa itself rather than asking Claude anything.
#[derive(Debug, PartialEq, Eq)]
pub enum Control {
    Stop,
}

pub fn control(request: &str) -> Option<Control> {
    let n: Vec<String> = request
        .split_whitespace()
        .map(norm)
        .filter(|w| !w.is_empty())
        .collect();
    let joined = n.join(" ");
    match joined.as_str() {
        "stop" | "cancel" | "be quiet" | "quiet" | "shut up" | "never mind" | "nevermind"
        | "stop talking" | "thats enough" | "enough" => Some(Control::Stop),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(s: &str) -> String {
        match detect(s) {
            Wake::Request(r) => r,
            other => panic!("expected request for {s:?}, got {other:?}"),
        }
    }

    #[test]
    fn wake_word_in_the_middle() {
        assert_eq!(
            req(
                "What is the best flight from Sydney to Delhi in December, Vexa, under 1000 dollars?"
            ),
            "What is the best flight from Sydney to Delhi in December, under 1000 dollars?"
        );
    }

    #[test]
    fn wake_word_at_start_and_end() {
        assert_eq!(
            req("Hey Vexa, what's the weather tomorrow?"),
            "What's the weather tomorrow?"
        );
        assert_eq!(
            req("What's the closing price of Nvidia on Friday, Vexa?"),
            "What's the closing price of Nvidia on Friday?"
        );
    }

    #[test]
    fn misheard_spellings() {
        assert!(matches!(detect("Vexer, set a timer"), Wake::Request(_)));
        assert!(matches!(
            detect("Vex a, what time is it in London"),
            Wake::Request(_)
        ));
        assert!(matches!(detect("Vexia what's new"), Wake::Request(_)));
    }

    #[test]
    fn similar_words_do_not_trigger() {
        assert_eq!(detect("I was so vexed by that meeting"), Wake::None);
        assert_eq!(detect("Vera said she would call later"), Wake::None);
        assert_eq!(detect("Alexa play music"), Wake::None);
        assert_eq!(detect("Let's push the release to Friday"), Wake::None);
    }

    #[test]
    fn wake_only() {
        assert_eq!(detect("Hey Vexa."), Wake::WakeOnly);
        assert_eq!(detect("Vexa?"), Wake::WakeOnly);
        assert_eq!(detect("Okay, Vexa, um"), Wake::WakeOnly);
    }

    #[test]
    fn ignores_non_speech_tags() {
        assert_eq!(
            clean_transcript("[BLANK_AUDIO] hello (music) there *laughs*"),
            "hello there"
        );
        assert_eq!(detect("[BLANK_AUDIO]"), Wake::None);
    }

    #[test]
    fn controls() {
        assert_eq!(control("Stop."), Some(Control::Stop));
        assert_eq!(control("Never mind"), Some(Control::Stop));
        assert_eq!(control("Stop the timer in ten minutes"), None);
    }
}
