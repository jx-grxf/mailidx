use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use rayon::prelude::*;
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::emlx;
use crate::store::Store;

const SCHEMA_VERSION: i64 = 1;
const INCOMPLETE_RECHECK_SECS: u64 = 60;

pub struct Index {
    conn: Connection,
    pub path: PathBuf,
    last_fingerprint: Option<(i64, i64)>,
    last_incomplete_check: Option<Instant>,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct SyncStats {
    pub added: usize,
    pub removed: usize,
    pub upgraded: usize,
    pub without_file: usize,
    pub ms: u128,
}

#[derive(Debug, Serialize)]
pub struct IndexStatus {
    pub path: String,
    pub documents: i64,
    pub without_body: i64,
    pub size_bytes: u64,
}

pub struct Hit {
    pub id: i64,
    pub snippet: String,
}

struct Doc {
    id: i64,
    complete: bool,
    subject: String,
    sender: String,
    recipients: String,
    body: String,
    attachments: String,
}

pub fn index_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("MAILIDX_INDEX") {
        return Ok(PathBuf::from(p));
    }
    if let Some(p) = crate::config::Config::load_or_default().index_path {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Library/Caches/mailidx/index.sqlite"))
}

impl Index {
    pub fn open() -> Result<Index> {
        let path = index_path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            conn.execute_batch(
                "DROP TABLE IF EXISTS doc;
                 DROP TABLE IF EXISTS fts;",
            )?;
        }
        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS doc (id INTEGER PRIMARY KEY, complete INTEGER NOT NULL);
             CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(
                 subject, sender, recipients, body, attachments,
                 tokenize = 'unicode61 remove_diacritics 2',
                 prefix = '2 3'
             );
             PRAGMA user_version = {SCHEMA_VERSION};"
        ))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Index { conn, path, last_fingerprint: None, last_incomplete_check: None })
    }

    /// Brings the index in line with Mail: adds new messages, drops deleted ones,
    /// and re-reads messages whose body was not on disk last time. Cheap when
    /// nothing changed — a single aggregate query against the Envelope Index.
    pub fn sync(&mut self, store: &mut Store) -> Result<SyncStats> {
        let t0 = Instant::now();
        let fp = store.fingerprint()?;
        let recheck = self
            .last_incomplete_check
            .is_none_or(|t| t.elapsed().as_secs() >= INCOMPLETE_RECHECK_SECS);
        if self.last_fingerprint == Some(fp) && !recheck {
            return Ok(SyncStats { ms: t0.elapsed().as_millis(), ..Default::default() });
        }
        let live: HashMap<i64, i64> = store.live_ids()?.into_iter().collect();
        let mut have: HashMap<i64, bool> = HashMap::new();
        {
            let mut st = self.conn.prepare("SELECT id, complete FROM doc")?;
            for row in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? != 0)))? {
                let (id, c) = row?;
                have.insert(id, c);
            }
        }
        let removed: Vec<i64> = have.keys().filter(|id| !live.contains_key(id)).copied().collect();
        let mut todo: Vec<(i64, i64, bool)> = live
            .iter()
            .filter_map(|(&id, &mb)| match have.get(&id) {
                None => Some((id, mb, false)),
                Some(false) if recheck => Some((id, mb, true)),
                _ => None,
            })
            .collect();
        todo.sort_unstable();

        let mut stats = SyncStats { removed: removed.len(), ..Default::default() };
        let jobs: Vec<(i64, bool, Option<PathBuf>, Option<String>)> = {
            let mut v = Vec::with_capacity(todo.len());
            let meta = store_meta(store, &todo)?;
            for (id, mb, upgrade) in &todo {
                let path = store.emlx_path(*id, *mb);
                if *upgrade && path.is_none() {
                    continue;
                }
                v.push((*id, *upgrade, path, meta.get(id).cloned()));
            }
            v
        };
        let docs: Vec<(bool, Doc)> = jobs
            .into_par_iter()
            .map(|(id, upgrade, path, fallback)| (upgrade, build_doc(id, path, fallback)))
            .collect();

        let tx = self.conn.transaction()?;
        {
            let mut del_doc = tx.prepare_cached("DELETE FROM doc WHERE id = ?")?;
            let mut del_fts = tx.prepare_cached("DELETE FROM fts WHERE rowid = ?")?;
            for id in &removed {
                del_doc.execute([id])?;
                del_fts.execute([id])?;
            }
            let mut ins_doc =
                tx.prepare_cached("INSERT OR REPLACE INTO doc (id, complete) VALUES (?, ?)")?;
            let mut ins_fts = tx.prepare_cached(
                "INSERT INTO fts (rowid, subject, sender, recipients, body, attachments) VALUES (?,?,?,?,?,?)",
            )?;
            for (upgrade, d) in &docs {
                if *upgrade {
                    if !d.complete {
                        continue;
                    }
                    del_fts.execute([d.id])?;
                    stats.upgraded += 1;
                } else {
                    stats.added += 1;
                }
                if !d.complete {
                    stats.without_file += 1;
                }
                ins_doc.execute(params![d.id, d.complete as i64])?;
                ins_fts.execute(params![
                    d.id, d.subject, d.sender, d.recipients, d.body, d.attachments
                ])?;
            }
        }
        tx.commit()?;
        self.last_fingerprint = Some(fp);
        if recheck {
            self.last_incomplete_check = Some(Instant::now());
        }
        stats.ms = t0.elapsed().as_millis();
        Ok(stats)
    }

    pub fn rebuild(&mut self, store: &mut Store) -> Result<SyncStats> {
        self.conn.execute_batch("DELETE FROM doc; DELETE FROM fts;")?;
        self.last_fingerprint = None;
        self.last_incomplete_check = None;
        let s = self.sync(store)?;
        self.conn.execute_batch("INSERT INTO fts(fts) VALUES('optimize'); VACUUM;")?;
        Ok(s)
    }

    pub fn search(&self, query: &str, raw: bool, any: bool, limit: usize) -> Result<Vec<Hit>> {
        let q = if raw { query.to_string() } else { build_query(query, any) };
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let mut st = self.conn.prepare_cached(
            "SELECT rowid, snippet(fts, 3, '«', '»', '…', 14)
             FROM fts WHERE fts MATCH ?
             ORDER BY bm25(fts, 6.0, 3.0, 1.5, 1.0, 2.0) LIMIT ?",
        )?;
        let rows = st.query_map(params![q, limit as i64], |r| {
            Ok(Hit { id: r.get(0)?, snippet: r.get::<_, Option<String>>(1)?.unwrap_or_default() })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("bad search query: {q}"))
    }

    pub fn status(&self) -> Result<IndexStatus> {
        let (documents, without_body) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(complete = 0), 0) FROM doc",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(IndexStatus {
            path: self.path.display().to_string(),
            documents,
            without_body,
            size_bytes: std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
        })
    }
}

fn store_meta(store: &Store, todo: &[(i64, i64, bool)]) -> Result<HashMap<i64, String>> {
    let wanted: HashSet<i64> = todo.iter().map(|t| t.0).collect();
    let mut out = HashMap::new();
    let mut st = store.conn().prepare(
        "SELECT m.ROWID, COALESCE(m.subject_prefix,'') || COALESCE(s.subject,''),
                COALESCE(a.comment,'') || ' ' || COALESCE(a.address,''), COALESCE(su.summary,'')
         FROM messages m
         LEFT JOIN subjects s ON s.ROWID = m.subject
         LEFT JOIN addresses a ON a.ROWID = m.sender
         LEFT JOIN summaries su ON su.ROWID = m.summary
         WHERE m.deleted = 0",
    )?;
    let rows = st.query_map([], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
    })?;
    for row in rows {
        let (id, subj, from, summary) = row?;
        if wanted.contains(&id) {
            out.insert(id, format!("{subj}\u{1}{from}\u{1}{summary}"));
        }
    }
    Ok(out)
}

fn build_doc(id: i64, path: Option<PathBuf>, fallback: Option<String>) -> Doc {
    if let Some(path) = path
        && let Ok(e) = emlx::read(&path)
            && let Some(msg) = emlx::parse(&e.raw) {
                let mut recipients = emlx::addresses(msg.to());
                recipients.extend(emlx::addresses(msg.cc()));
                let names: Vec<String> =
                    msg.attachments().filter_map(|a| a.attachment_name().map(String::from)).collect();
                return Doc {
                    id,
                    complete: true,
                    subject: msg.subject().unwrap_or("").to_string(),
                    sender: emlx::addresses(msg.from()).join(", "),
                    recipients: recipients.join(", "),
                    body: emlx::body_text(&msg),
                    attachments: names.join(" "),
                };
            }
    let fb = fallback.unwrap_or_default();
    let mut it = fb.split('\u{1}');
    Doc {
        id,
        complete: false,
        subject: it.next().unwrap_or("").to_string(),
        sender: it.next().unwrap_or("").to_string(),
        recipients: String::new(),
        body: it.next().unwrap_or("").to_string(),
        attachments: String::new(),
    }
}

/// Turns free text into an FTS5 query: every word must match, each as a prefix,
/// quoted so punctuation in phone numbers or addresses cannot break the syntax.
/// `"exact phrase"` stays a phrase, a leading `-` excludes a word, `OR` works.
/// With `any`, one matching word is enough; bm25 still ranks mails with more
/// matches first.
pub fn build_query(input: &str, any: bool) -> String {
    let mut terms: Vec<String> = Vec::new();
    let mut chars = input.chars().peekable();
    let mut cur = String::new();
    let flush = |cur: &mut String, terms: &mut Vec<String>| {
        let w = std::mem::take(cur);
        if w.is_empty() {
            return;
        }
        if w == "OR" || w == "AND" {
            terms.push(w);
            return;
        }
        let (neg, w) = match w.strip_prefix('-') {
            Some(rest) if !rest.is_empty() => (true, rest.to_string()),
            _ => (false, w),
        };
        let tokens: Vec<String> = w
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .collect();
        if tokens.is_empty() {
            return;
        }
        let quoted = format!("\"{}\"*", tokens.join(" "));
        terms.push(if neg { format!("NOT {quoted}") } else { quoted });
    };
    while let Some(c) = chars.next() {
        if c == '"' {
            flush(&mut cur, &mut terms);
            let mut phrase = String::new();
            for d in chars.by_ref() {
                if d == '"' {
                    break;
                }
                phrase.push(d);
            }
            let tokens: Vec<&str> =
                phrase.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).collect();
            if !tokens.is_empty() {
                terms.push(format!("\"{}\"", tokens.join(" ")));
            }
        } else if c.is_whitespace() {
            flush(&mut cur, &mut terms);
        } else {
            cur.push(c);
        }
    }
    flush(&mut cur, &mut terms);
    while matches!(terms.first().map(String::as_str), Some("OR" | "AND")) {
        terms.remove(0);
    }
    while matches!(terms.last().map(String::as_str), Some("OR" | "AND")) {
        terms.pop();
    }
    if terms.first().is_some_and(|t| t.starts_with("NOT ")) {
        return String::new();
    }
    if any && !terms.iter().any(|t| t == "OR" || t == "AND") {
        let (neg, pos): (Vec<String>, Vec<String>) = terms.into_iter().partition(|t| t.starts_with("NOT "));
        let mut q = format!("({})", pos.join(" OR "));
        for n in neg {
            q.push(' ');
            q.push_str(&n);
        }
        return q;
    }
    terms.join(" ")
}

#[cfg(test)]
mod tests {
    use super::build_query;

    #[test]
    fn queries() {
        assert_eq!(build_query("rechnung", false), "\"rechnung\"*");
        assert_eq!(build_query("0664 123-45", false), "\"0664\"* \"123 45\"*");
        assert_eq!(build_query("\"beta platz\" oeffigo", false), "\"beta platz\" \"oeffigo\"*");
        assert_eq!(build_query("a OR b", false), "\"a\"* OR \"b\"*");
        assert_eq!(build_query("github -dependabot", false), "\"github\"* NOT \"dependabot\"*");
        assert_eq!(build_query("\"); DROP", false), "\"DROP\"");
        assert_eq!(build_query("-only", false), "");
        assert_eq!(build_query("  ", false), "");
        assert_eq!(build_query("rechnung zahlen -spam", true), "(\"rechnung\"* OR \"zahlen\"*) NOT \"spam\"*");
        assert_eq!(build_query("a OR b", true), "\"a\"* OR \"b\"*");
    }
}
