use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use mail_parser::Message;
use serde::Serialize;

use crate::emlx::{self, Emlx};

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Invite {
    /// REQUEST (invitation or update), CANCEL, REPLY, PUBLISH.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub method: String,
    pub summary: String,
    pub start: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub end: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub all_day: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub location: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub organizer: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attendees: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub uid: String,
}

/// `emlx` is needed for `.partial.emlx`, where Mail keeps the .ics in a separate file.
pub fn invites(emlx: Option<&Emlx>, msg: &Message<'_>) -> Vec<Invite> {
    let mut out = Vec::new();
    for part in msg.parts() {
        let is_cal = part.content_type().is_some_and(|c| {
            c.ctype().eq_ignore_ascii_case("text") && c.subtype().is_some_and(|s| s.eq_ignore_ascii_case("calendar"))
                || c.subtype().is_some_and(|s| s.eq_ignore_ascii_case("ics"))
        }) || part.attachment_name().is_some_and(|n| n.to_ascii_lowercase().ends_with(".ics"));
        if !is_cal {
            continue;
        }
        let mut data = part.decoded().into_owned();
        if data.is_empty()
            && let Some(e) = emlx.filter(|e| e.partial)
            && let Some(p) = emlx::partial_attachment_path(e, part.attachment_name().unwrap_or(""))
        {
            data = std::fs::read(p).unwrap_or_default();
        }
        let text = String::from_utf8_lossy(&data).into_owned();
        for inv in parse(&text) {
            if !out.iter().any(|o: &Invite| o.uid == inv.uid && o.start == inv.start) {
                out.push(inv);
            }
        }
    }
    out
}

pub fn parse(ics: &str) -> Vec<Invite> {
    let mut lines: Vec<String> = Vec::new();
    for raw in ics.split('\n') {
        let raw = raw.trim_end_matches('\r');
        if (raw.starts_with(' ') || raw.starts_with('\t')) && !lines.is_empty() {
            lines.last_mut().unwrap().push_str(&raw[1..]);
        } else {
            lines.push(raw.to_string());
        }
    }
    let mut method = String::new();
    let mut out = Vec::new();
    let mut cur: Option<Invite> = None;
    let mut depth = 0;
    for line in &lines {
        let Some((head, value)) = line.split_once(':') else { continue };
        let mut params = head.split(';');
        let name = params.next().unwrap_or("").to_ascii_uppercase();
        let params: Vec<(String, String)> = params
            .filter_map(|p| p.split_once('='))
            .map(|(k, v)| (k.to_ascii_uppercase(), v.trim_matches('"').to_string()))
            .collect();
        let param = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        match (name.as_str(), value) {
            ("METHOD", v) => method = v.trim().to_string(),
            ("BEGIN", "VEVENT") => {
                cur = Some(Invite { method: method.clone(), ..Default::default() });
                depth = 0;
            }
            ("BEGIN", _) if cur.is_some() => depth += 1,
            ("END", "VEVENT") => out.extend(cur.take()),
            ("END", _) if cur.is_some() => depth -= 1,
            _ => {}
        }
        let Some(ev) = cur.as_mut() else { continue };
        if depth > 0 {
            continue;
        }
        match name.as_str() {
            "SUMMARY" => ev.summary = unescape(value),
            "LOCATION" => ev.location = unescape(value),
            "STATUS" => ev.status = value.trim().to_string(),
            "UID" => ev.uid = value.trim().to_string(),
            "DESCRIPTION" => ev.description = unescape(value).chars().take(500).collect(),
            "DTSTART" => {
                let (t, all_day) = when(value, param("TZID"));
                ev.start = t;
                ev.all_day = all_day;
            }
            "DTEND" => ev.end = when(value, param("TZID")).0,
            "ORGANIZER" => ev.organizer = person(value, param("CN"), None),
            "ATTENDEE" => ev.attendees.push(person(value, param("CN"), param("PARTSTAT"))),
            _ => {}
        }
    }
    out
}

fn unescape(v: &str) -> String {
    v.replace("\\n", "\n").replace("\\N", "\n").replace("\\,", ",").replace("\\;", ";").replace("\\\\", "\\").trim().to_string()
}

fn person(value: &str, cn: Option<&str>, status: Option<&str>) -> String {
    let addr = value.trim().trim_start_matches("mailto:").trim_start_matches("MAILTO:");
    let mut s = match cn {
        Some(n) if !n.is_empty() && n != addr => format!("{n} <{addr}>"),
        _ => addr.to_string(),
    };
    if let Some(st) = status {
        s.push_str(&format!(" ({})", st.to_ascii_lowercase().replace('-', " ")));
    }
    s
}

/// UTC times are shown in local time; times with a TZID keep their wall clock
/// and name the zone, since that is what the organizer typed.
fn when(v: &str, tzid: Option<&str>) -> (String, bool) {
    let v = v.trim();
    if v.len() == 8
        && let Ok(d) = NaiveDate::parse_from_str(v, "%Y%m%d")
    {
        return (d.format("%Y-%m-%d").to_string(), true);
    }
    let utc = v.ends_with('Z');
    let Ok(t) = NaiveDateTime::parse_from_str(v.trim_end_matches('Z'), "%Y%m%dT%H%M%S") else {
        return (v.to_string(), false);
    };
    if utc {
        let local = Utc.from_utc_datetime(&t).with_timezone(&Local);
        return (local.format("%Y-%m-%d %H:%M").to_string(), false);
    }
    let mut s = t.format("%Y-%m-%d %H:%M").to_string();
    if let Some(z) = tzid {
        s.push_str(&format!(" ({z})"));
    }
    (s, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_an_invitation() {
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Vienna\r\nBEGIN:STANDARD\r\nDTSTART:19701025T030000\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:abc-1\r\nSUMMARY:Projektbesprechung\\, Teil 2\r\nDTSTART;TZID=Europe/Vienna:20261110T140000\r\nDTEND;TZID=Europe/Vienna:20261110T150000\r\nLOCATION:Raum 3\r\nORGANIZER;CN=\"Anna Muster\":mailto:anna@example.com\r\nATTENDEE;CN=Johannes;PARTSTAT=NEEDS-ACTION:mailto:jo@example.com\r\nDESCRIPTION:Bitte Unterlagen mi\r\n tbringen\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let v = parse(ics);
        assert_eq!(v.len(), 1);
        let e = &v[0];
        assert_eq!(e.method, "REQUEST");
        assert_eq!(e.summary, "Projektbesprechung, Teil 2");
        assert_eq!(e.start, "2026-11-10 14:00 (Europe/Vienna)");
        assert_eq!(e.end, "2026-11-10 15:00 (Europe/Vienna)");
        assert_eq!(e.organizer, "Anna Muster <anna@example.com>");
        assert_eq!(e.attendees, vec!["Johannes <jo@example.com> (needs action)"]);
        assert_eq!(e.description, "Bitte Unterlagen mitbringen");
        assert_eq!(e.uid, "abc-1");
    }

    #[test]
    fn finds_invite_in_mime_message() {
        let raw = "From: a@example.com\r\nTo: b@example.com\r\nSubject: Einladung\r\nMIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=XX\r\n\r\n--XX\r\nContent-Type: text/plain\r\n\r\nHallo\r\n\
--XX\r\nContent-Type: text/calendar; method=REQUEST; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n\
QkVHSU46VkNBTEVOREFSDQpNRVRIT0Q6UkVRVUVTVA0KQkVHSU46VkVWRU5UDQpVSUQ6dTENClNVTU1BUlk6VGVzdA0KRFRTVEFSVDtWQUxVRT1EQVRFOjIwMjYxMjAxDQpFTkQ6VkVWRU5UDQpFTkQ6VkNBTEVOREFSDQo=\r\n\
--XX--\r\n";
        let msg = crate::emlx::parse(raw.as_bytes()).unwrap();
        let v = invites(None, &msg);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].summary, "Test");
        assert_eq!(v[0].method, "REQUEST");
        assert_eq!(v[0].start, "2026-12-01");
    }

    #[test]
    fn all_day_event() {
        let v = parse("BEGIN:VEVENT\nUID:x\nSUMMARY:Urlaub\nDTSTART;VALUE=DATE:20261224\nEND:VEVENT\n");
        assert_eq!(v[0].start, "2026-12-24");
        assert!(v[0].all_day);
    }
}
