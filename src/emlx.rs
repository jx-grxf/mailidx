use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use mail_parser::{Address, Message, MessageParser};
use serde::Serialize;

pub struct Emlx {
    pub raw: Vec<u8>,
    pub partial: bool,
    pub path: PathBuf,
}

/// An `.emlx` file is `<byte count>\n<RFC 822 message><Apple plist>`.
pub fn read(path: &Path) -> Result<Emlx> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let nl = bytes
        .iter()
        .position(|&b| b == b'\n')
        .ok_or_else(|| anyhow!("{} is not an emlx file", path.display()))?;
    let len: usize = std::str::from_utf8(&bytes[..nl])?.trim().parse()?;
    let start = nl + 1;
    let end = (start + len).min(bytes.len());
    Ok(Emlx {
        raw: bytes[start..end].to_vec(),
        partial: path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(".partial.emlx")),
        path: path.to_path_buf(),
    })
}

pub fn parse(raw: &[u8]) -> Option<Message<'_>> {
    MessageParser::default().parse(raw)
}

pub fn addresses(list: Option<mail_parser::AddressList<'_>>) -> Vec<String> {
    let Some(list) = list else { return Vec::new() };
    let mut out = Vec::new();
    for a in list.iter() {
        match a {
            Address::Mailbox(m) => out.push(fmt_mailbox(m.name(), m.address())),
            Address::Group(g) => {
                for m in g.mailboxes() {
                    out.push(fmt_mailbox(m.name(), m.address()));
                }
            }
        }
    }
    out
}

fn fmt_mailbox(name: Option<&str>, addr: Option<&str>) -> String {
    match (name, addr) {
        (Some(n), Some(a)) if !n.is_empty() => format!("{n} <{a}>"),
        (_, Some(a)) => a.to_string(),
        (Some(n), None) => n.to_string(),
        _ => String::new(),
    }
}

/// Plain text of the message: the text/plain body if there is one, otherwise
/// the HTML body converted to text.
pub fn body_text(msg: &Message<'_>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(t) = msg.body_text(i) {
        parts.push(t.into_owned());
        i += 1;
    }
    let text = parts.join("\n\n");
    if !text.trim().is_empty() {
        return normalize_ws(&text);
    }
    msg.body_html(0).map(|h| html_to_text(&h)).unwrap_or_default()
}

fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let l = line.trim_end();
        if l.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(l);
        out.push('\n');
    }
    out.trim().to_string()
}

pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut chars = html.char_indices().peekable();
    let lower = html.to_ascii_lowercase();
    while let Some((i, c)) = chars.next() {
        if c == '<' {
            let rest = &lower[i..];
            let skip_block = ["<style", "<script", "<head"]
                .iter()
                .find(|t| rest.starts_with(**t))
                .map(|t| format!("</{}", &t[1..]));
            let end = if let Some(close) = skip_block {
                rest.find(&close).and_then(|p| rest[p..].find('>').map(|q| p + q))
            } else {
                rest.find('>')
            };
            let Some(end) = end else { break };
            let tag = &rest[1..end];
            let name: String = tag
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if matches!(
                name.as_str(),
                "br" | "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "table" | "blockquote"
            ) {
                out.push('\n');
            }
            while let Some(&(j, _)) = chars.peek() {
                if j > i + end {
                    break;
                }
                chars.next();
            }
        } else if c == '&' {
            let rest = &html[i..];
            if let Some(semi) = rest[..rest.len().min(10)].find(';') {
                let ent = &rest[1..semi];
                let rep = match ent {
                    "nbsp" => Some(" ".to_string()),
                    "amp" => Some("&".into()),
                    "lt" => Some("<".into()),
                    "gt" => Some(">".into()),
                    "quot" => Some("\"".into()),
                    "apos" | "#39" => Some("'".into()),
                    "auml" => Some("ä".into()),
                    "ouml" => Some("ö".into()),
                    "uuml" => Some("ü".into()),
                    "Auml" => Some("Ä".into()),
                    "Ouml" => Some("Ö".into()),
                    "Uuml" => Some("Ü".into()),
                    "szlig" => Some("ß".into()),
                    "euro" => Some("€".into()),
                    "zwnj" | "shy" | "zwj" => Some(String::new()),
                    e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16)
                        .ok()
                        .and_then(char::from_u32)
                        .map(String::from),
                    e if e.starts_with('#') => {
                        e[1..].parse::<u32>().ok().and_then(char::from_u32).map(String::from)
                    }
                    _ => None,
                };
                if let Some(r) = rep {
                    out.push_str(&r);
                    for _ in 0..semi {
                        chars.next();
                    }
                    continue;
                }
            }
            out.push('&');
        } else {
            out.push(c);
        }
    }
    let collapsed: String = out
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n");
    normalize_ws(&collapsed)
}

pub fn links(msg: &Message<'_>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    fn push(out: &mut Vec<String>, u: &str) {
        let u = u.trim().replace("&amp;", "&");
        if (u.starts_with("http://") || u.starts_with("https://") || u.starts_with("mailto:"))
            && !out.iter().any(|o| o.trim_end_matches('/') == u.trim_end_matches('/'))
        {
            out.push(u);
        }
    }
    let mut i = 0;
    while let Some(h) = msg.body_html(i) {
        let lower = h.to_ascii_lowercase();
        let mut pos = 0;
        while let Some(p) = lower[pos..].find("href=") {
            let start = pos + p + 5;
            let q = h[start..].chars().next().unwrap_or(' ');
            let (s, endc) = if q == '"' || q == '\'' { (start + 1, q) } else { (start, ' ') };
            let end = h[s..].find([endc, '>']).map(|e| s + e).unwrap_or(h.len());
            push(&mut out, &h[s..end]);
            pos = end;
        }
        i += 1;
    }
    // URLs typed into a plain-text part (or HTML without an anchor) carry no href.
    let text = body_text(msg);
    for w in text.split(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"') {
        if w.starts_with("http://") || w.starts_with("https://") {
            push(&mut out, w.trim_end_matches(['.', ',', ')', ']']));
        }
    }
    out
}

#[derive(Debug, Serialize)]
pub struct AttachmentInfo {
    pub index: usize,
    pub name: String,
    pub mime: String,
    pub size: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub on_disk_only: bool,
    /// Small image the HTML shows in place (signature logo, icon): not a real attachment.
    #[serde(skip)]
    pub inline: bool,
}

/// Larger embedded images are usually photos the sender meant to attach.
const INLINE_MAX_BYTES: usize = 64 * 1024;

pub fn attachments(emlx: &Emlx, msg: &Message<'_>) -> Vec<AttachmentInfo> {
    let mut html = String::new();
    let mut i = 0;
    while let Some(h) = msg.body_html(i) {
        html.push_str(&h);
        i += 1;
    }
    let mut out = Vec::new();
    for (index, part) in msg.attachments().enumerate() {
        let name = part.attachment_name().unwrap_or("").to_string();
        let mime = part
            .content_type()
            .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("")))
            .unwrap_or_default();
        let mut size = part.decoded_len();
        let mut on_disk_only = false;
        if emlx.partial && size == 0 && !name.is_empty()
            && let Some(p) = partial_attachment_path(emlx, &name) {
                size = std::fs::metadata(&p).map(|m| m.len() as usize).unwrap_or(0);
                on_disk_only = true;
            }
        let inline = size <= INLINE_MAX_BYTES
            && mime.starts_with("image/")
            && part
                .content_id()
                .map(|c| c.trim_matches(['<', '>']))
                .is_some_and(|c| !c.is_empty() && html.contains(&format!("cid:{c}")));
        out.push(AttachmentInfo { index, name, mime, size, on_disk_only, inline });
    }
    out
}

/// For `.partial.emlx` Mail moves attachment bodies to
/// `<Data>/<dirs>/Attachments/<rowid>/<part>/<filename>`.
pub fn partial_attachment_path(emlx: &Emlx, name: &str) -> Option<PathBuf> {
    let msgs_dir = emlx.path.parent()?;
    let base = msgs_dir.parent()?;
    let id = emlx.path.file_name()?.to_string_lossy().split('.').next()?.to_string();
    let dir = base.join("Attachments").join(id);
    for part in std::fs::read_dir(&dir).ok()?.flatten() {
        let candidate = part.path().join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

pub fn attachment_bytes(emlx: &Emlx, msg: &Message<'_>, index: usize) -> Result<(String, Vec<u8>)> {
    let part = msg
        .attachments()
        .nth(index)
        .ok_or_else(|| anyhow!("attachment index {index} does not exist"))?;
    let name = part
        .attachment_name()
        .map(sanitize_filename)
        .unwrap_or_else(|| format!("attachment-{index}"));
    let data = part.decoded().into_owned();
    if data.is_empty() && emlx.partial {
        if let Some(p) = partial_attachment_path(emlx, part.attachment_name().unwrap_or("")) {
            return Ok((name, std::fs::read(p)?));
        }
        return Err(anyhow!("attachment not downloaded yet; open the message in Mail once"));
    }
    Ok((name, data))
}

pub fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '\0') { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() { "attachment".into() } else { cleaned }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_conversion() {
        let t = html_to_text(
            "<html><head><style>p{}</style></head><body><p>Hallo&nbsp;Welt</p><div>Gr&uuml;&szlig;e &amp; mehr</div></body></html>",
        );
        assert_eq!(t, "Hallo Welt\n\nGrüße & mehr");
    }

    #[test]
    fn filenames() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_filename(""), "attachment");
    }
}

pub fn raw_header(msg: &Message<'_>, name: &str) -> Option<String> {
    msg.headers().iter().find(|h| h.name().as_str().eq_ignore_ascii_case(name)).map(|h| {
        String::from_utf8_lossy(h.raw_value()).split_whitespace().collect::<Vec<_>>().join(" ")
    })
}

#[derive(Debug, Default, Serialize, Clone, PartialEq)]
pub struct Unsubscribe {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mailto: Option<String>,
    /// RFC 8058: a single POST unsubscribes, no page to click through.
    pub one_click: bool,
}

pub fn unsubscribe(msg: &Message<'_>) -> Option<Unsubscribe> {
    let header = raw_header(msg, "List-Unsubscribe")?;
    let post = raw_header(msg, "List-Unsubscribe-Post").unwrap_or_default();
    parse_unsubscribe(&header, &post)
}

pub fn parse_unsubscribe(header: &str, post: &str) -> Option<Unsubscribe> {
    let mut u = Unsubscribe::default();
    for part in header.split('<').skip(1) {
        let target = part.split('>').next().unwrap_or("").trim();
        let lower = target.to_ascii_lowercase();
        if lower.starts_with("https://") || lower.starts_with("http://") {
            u.url.get_or_insert_with(|| target.to_string());
        } else if lower.starts_with("mailto:") {
            u.mailto.get_or_insert_with(|| target.to_string());
        }
    }
    u.one_click = u.url.as_deref().is_some_and(|x| x.starts_with("https://"))
        && post.to_ascii_lowercase().replace(' ', "").contains("list-unsubscribe=one-click");
    (u.url.is_some() || u.mailto.is_some()).then_some(u)
}

#[cfg(test)]
mod unsubscribe_tests {
    use super::*;

    #[test]
    fn reads_list_unsubscribe() {
        let u = parse_unsubscribe(
            "<mailto:unsub@example.com?subject=stop>, <https://example.com/u/1>",
            "List-Unsubscribe=One-Click",
        )
        .unwrap();
        assert_eq!(u.url.as_deref(), Some("https://example.com/u/1"));
        assert_eq!(u.mailto.as_deref(), Some("mailto:unsub@example.com?subject=stop"));
        assert!(u.one_click);
        let plain = parse_unsubscribe("<http://example.com/u>", "List-Unsubscribe=One-Click").unwrap();
        assert!(!plain.one_click, "one-click needs https");
        assert!(parse_unsubscribe("no brackets here", "").is_none());
    }
}
