use mail_parser::Message;
use serde::Serialize;

use crate::emlx;

#[derive(Debug, Serialize, PartialEq)]
pub struct Found {
    pub codes: Vec<String>,
    pub verify_links: Vec<String>,
}

pub fn from_message(msg: &Message<'_>) -> Found {
    let text = format!("{}\n{}", msg.subject().unwrap_or(""), emlx::body_text(msg));
    Found { codes: codes(&text), verify_links: verify_links(&emlx::links(msg)) }
}

const CODE_WORDS: &[&str] = &[
    "code", "kode", "otp", "pin", "passcode", "password", "passwort", "kennwort", "token",
    "verification", "verify", "verifizierung", "bestätigung", "bestaetigung", "confirmation",
    "sicherheitscode", "einmal", "tan", "2fa", "authentication", "anmeldung", "login", "sign-in",
];

/// One-time codes next to a keyword such as "code" or "Bestätigung". A bare
/// number only counts when it sits in the first line (the subject), where
/// senders like "482913 is your code" put it.
pub fn codes(text: &str) -> Vec<String> {
    let cleaned: String = text
        .split_inclusive(char::is_whitespace)
        .map(|w| {
            let t = w.trim();
            if t.contains("://") || t.contains('@') || t.starts_with("www.") {
                " ".repeat(w.chars().count())
            } else {
                w.to_string()
            }
        })
        .collect();
    let chars: Vec<char> = cleaned.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let subject_end = chars.iter().position(|c| *c == '\n').unwrap_or(chars.len());
    let keyword_at: Vec<usize> = (0..lower.len())
        .filter(|&i| i == 0 || !lower[i - 1].is_alphanumeric())
        .filter(|&i| CODE_WORDS.iter().any(|k| starts_with(&lower[i..], k)))
        .collect();

    let mut scored: Vec<(usize, String)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        if i > 0 && (is_glue(chars[i - 1]) || chars[i - 1].is_alphanumeric()) {
            while i < chars.len() && chars[i].is_alphanumeric() {
                i += 1;
            }
            continue;
        }
        let (token, end) = read_token(&chars, i);
        let next_is_glue = chars.get(end).is_some_and(|c| is_glue(*c) && chars.get(end + 1).is_some_and(char::is_ascii_alphanumeric));
        if !next_is_glue && looks_like_code(&token) {
            let dist = keyword_at
                .iter()
                .filter(|&&k| i >= subject_end || k < subject_end)
                .map(|&k| if k <= i { i - k } else { (k - i) * 2 })
                .min()
                .unwrap_or(usize::MAX);
            let in_subject = i < subject_end && token.chars().all(|c| c.is_ascii_digit()) && token.len() >= 5;
            if dist <= 80 || in_subject {
                let rank = if dist <= 80 { dist } else { 100 };
                if !scored.iter().any(|(_, t)| *t == token) {
                    scored.push((rank, token));
                }
            }
        }
        i = end.max(i + 1);
    }
    // A bare number in the subject is only a guess; once a code sits next to a
    // keyword, it is more likely a ticket number or time.
    if scored.iter().any(|(r, _)| *r <= 80) {
        scored.retain(|(r, _)| *r <= 80);
    }
    scored.sort_by_key(|(r, _)| *r);
    scored.into_iter().map(|(_, t)| t).take(3).collect()
}

fn starts_with(s: &[char], word: &str) -> bool {
    let mut n = 0;
    for (a, b) in s.iter().zip(word.chars()) {
        if *a != b {
            return false;
        }
        n += 1;
    }
    n == word.chars().count()
}

/// Characters that join a number to something that is not a code: dates,
/// times, decimals, paths, amounts.
fn is_glue(c: char) -> bool {
    matches!(c, '.' | ':' | '/' | ',' | '€' | '$' | '%' | '+' | '#' | '_' | '=')
}

/// Reads an alphanumeric run; digit groups split by one space or hyphen
/// ("123 456", "123-456") join into one code.
fn read_token(chars: &[char], start: usize) -> (String, usize) {
    let mut out = String::new();
    let mut i = start;
    loop {
        while i < chars.len() && chars[i].is_ascii_alphanumeric() {
            out.push(chars[i]);
            i += 1;
        }
        let group_digits = out.chars().rev().take_while(char::is_ascii_digit).count();
        let sep = chars.get(i).is_some_and(|c| *c == ' ' || *c == '-');
        let next_group = chars[(i + 1).min(chars.len())..].iter().take_while(|c| c.is_ascii_digit()).count();
        let after = chars.get(i + 1 + next_group).is_none_or(|c| !c.is_ascii_alphanumeric());
        if sep && group_digits == 3 && out.len() == 3 && next_group == 3 && after {
            i += 1;
            continue;
        }
        return (out, i);
    }
}

fn looks_like_code(t: &str) -> bool {
    let digits = t.chars().filter(char::is_ascii_digit).count();
    let letters = t.len() - digits;
    if !(4..=8).contains(&t.len()) || digits == 0 {
        return false;
    }
    if letters == 0 {
        let year = t.len() == 4 && (t.starts_with("19") || t.starts_with("20"));
        return !year;
    }
    t.len() >= 6 && digits >= 2 && t.chars().all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
}

const LINK_WORDS: &[&str] = &[
    "verify", "verif", "confirm", "activate", "activation", "bestaetig", "best%c3%a4tig", "validate",
    "magic", "signin", "sign-in", "login", "auth", "token=", "reset", "password", "review",
    "approve", "freigabe", "invite", "accept", "onboard", "register", "signup", "sign-up",
];
const NOT_VERIFY: &[&str] = &[
    "unsubscribe", "unsub", "abmelden", "abbestellen", "optout", "opt-out", "preferences",
    "privacy", "datenschutz", ".png", ".jpg", ".gif", "mailto:",
];

/// Links that confirm, verify, log in or review something, best match first.
pub fn verify_links(links: &[String]) -> Vec<String> {
    let mut scored: Vec<(usize, usize, &String)> = links
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            let u = l.to_lowercase();
            if NOT_VERIFY.iter().any(|w| u.contains(w)) {
                return None;
            }
            let score = LINK_WORDS.iter().filter(|w| u.contains(*w)).count();
            (score > 0).then_some((score, i, l))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, l)| l.clone()).take(5).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_codes_next_to_keywords() {
        assert_eq!(codes("Dein Bestätigungscode lautet 482913."), vec!["482913"]);
        assert_eq!(codes("Your verification code: 123 456"), vec!["123456"]);
        assert_eq!(codes("482913 is your Instagram code\nHi"), vec!["482913"]);
        assert_eq!(codes("Login-Code\nUse AB12CD9 to sign in"), vec!["AB12CD9"]);
        assert_eq!(codes("G-772041 ist dein Google-Bestätigungscode"), vec!["772041"]);
        assert_eq!(codes("Volltest 202051\nDein Bestätigungscode lautet 739204."), vec!["739204"]);
    }

    #[test]
    fn ignores_dates_amounts_and_urls() {
        assert!(codes("Rechnung\nBetrag 1299,00 € vom 07.11.2026, Code folgt 2026").is_empty());
        assert!(codes("Termin\nCode: siehe https://x.com/a/482913 um 14:30").is_empty());
        assert!(codes("Newsletter\nWir haben 12345 Kunden").is_empty());
    }

    #[test]
    fn ranks_verify_links_and_skips_unsubscribe() {
        let links = vec![
            "https://shop.example/unsubscribe?u=1".to_string(),
            "https://example.com/about".to_string(),
            "https://example.com/verify?token=abc".to_string(),
            "https://johannesgrof.me/api/guestbook/review?id=1&sig=2".to_string(),
        ];
        assert_eq!(
            verify_links(&links),
            vec!["https://example.com/verify?token=abc", "https://johannesgrof.me/api/guestbook/review?id=1&sig=2"]
        );
    }
}
