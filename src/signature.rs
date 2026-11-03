use std::collections::HashSet;
use std::path::Path;

use crate::emlx;

/// Mail's signatures, read from its own `.mailsignature` files. Re-creating a draft
/// in a fresh compose window makes Mail add the signature again, so the copy already
/// in the draft (text and logo images) has to come out first.
pub struct Signatures {
    texts: Vec<String>,
    cids: HashSet<String>,
}

impl Signatures {
    pub fn load(mail_root: &Path) -> Signatures {
        let mut out = Signatures { texts: Vec::new(), cids: HashSet::new() };
        let Ok(entries) = std::fs::read_dir(mail_root.join("MailData/Signatures")) else {
            return out;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "mailsignature") {
                continue;
            }
            let Ok(raw) = std::fs::read(&path) else { continue };
            let Some(msg) = emlx::parse(&raw) else { continue };
            let Some(html) = msg.body_html(0) else { continue };
            let mut rest = html.as_ref();
            while let Some(i) = rest.find("cid:") {
                rest = &rest[i + 4..];
                let end = rest.find(['"', '\'', ')', '>', ' ']).unwrap_or(rest.len());
                out.cids.insert(rest[..end].to_string());
                rest = &rest[end..];
            }
            let text = emlx::html_to_text(&html);
            if !squash(&text).is_empty() {
                out.texts.push(text);
            }
        }
        out
    }

    pub fn is_signature_image(&self, content_id: &str) -> bool {
        self.cids.contains(content_id.trim_matches(['<', '>']))
    }

    /// Removes a signature from the end of `body`. Blank lines, link annotations
    /// like `<https://…>` and attachment placeholders are ignored when comparing.
    pub fn strip(&self, body: &str) -> String {
        let lines: Vec<&str> = body.lines().collect();
        for sig in &self.texts {
            let target = squash(sig);
            for i in (0..lines.len()).rev() {
                if squash(lines[i]).is_empty() {
                    continue;
                }
                let tail = squash(&lines[i..].join("\n"));
                if tail == target {
                    return lines[..i].join("\n").trim_end().to_string();
                }
                if tail.len() > target.len() {
                    break;
                }
            }
        }
        body.to_string()
    }
}

fn squash(s: &str) -> String {
    let mut out = String::new();
    let mut push = |part: &str| out.extend(part.chars().filter(|c| !c.is_whitespace() && *c != '\u{FFFC}'));
    let mut rest = s;
    while let Some(i) = rest.find('<') {
        push(&rest[..i]);
        let tail = &rest[i..];
        match tail.find('>') {
            Some(end) if tail.starts_with("<http") || tail.starts_with("<mailto:") => rest = &tail[end + 1..],
            _ => {
                push("<");
                rest = &tail[1..];
            }
        }
    }
    push(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sigs() -> Signatures {
        Signatures {
            texts: vec!["Johannes Grof\nÖffiGo · die Öffi-App\noeffigo.app".into()],
            cids: HashSet::from(["ABC-1".to_string()]),
        }
    }

    #[test]
    fn strips_trailing_signature_with_blank_lines_and_links() {
        let body = "Hallo,\n\ndanke.\n\nJohannes Grof\n\nÖffiGo · die Öffi-App\n\noeffigo.app <https://oeffigo.app/>\n";
        assert_eq!(sigs().strip(body), "Hallo,\n\ndanke.");
    }

    #[test]
    fn keeps_body_without_signature_at_the_end() {
        let body = "Johannes Grof\nÖffiGo · die Öffi-App\noeffigo.app\n\nPS: noch etwas";
        assert_eq!(sigs().strip(body), body);
    }

    #[test]
    fn matches_signature_images_by_content_id() {
        assert!(sigs().is_signature_image("<ABC-1>"));
        assert!(!sigs().is_signature_image("XYZ"));
    }
}
