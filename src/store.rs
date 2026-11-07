use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use percent_encoding::percent_decode_str;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params_from_iter, types::Value};
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::config::Config;
use crate::time::fmt_ts;

const FLAG_ANSWERED: i64 = 1 << 2;
const FLAG_FORWARDED: i64 = 1 << 8;

const REQUIRED_COLUMNS: &[(&str, &[&str])] = &[
    (
        "messages",
        &[
            "ROWID", "sender", "subject_prefix", "subject", "summary", "date_sent",
            "date_received", "mailbox", "flags", "read", "flagged", "deleted", "size",
            "conversation_id", "list_id_hash", "automated_conversation", "global_message_id",
        ],
    ),
    ("mailboxes", &["ROWID", "url", "total_count", "unread_count"]),
    ("addresses", &["ROWID", "address", "comment"]),
    ("subjects", &["ROWID", "subject"]),
    ("summaries", &["ROWID", "summary"]),
    ("recipients", &["message", "address", "type", "position"]),
    ("attachments", &["message", "attachment_id", "name"]),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    Outbox,
    Other,
}

impl Role {
    fn detect(name: &str) -> Role {
        let n = name.to_lowercase();
        match n.as_str() {
            "inbox" | "posteingang" => Role::Inbox,
            "sent messages" | "sent" | "sent items" | "sent mail" | "gesendet"
            | "gesendete elemente" | "gesendete objekte" => Role::Sent,
            "drafts" | "entwürfe" => Role::Drafts,
            "deleted messages" | "trash" | "deleted items" | "papierkorb"
            | "gelöschte elemente" | "gelöschte objekte" | "bin" => Role::Trash,
            "junk" | "spam" | "junk-e-mail" | "junk e-mail" | "junk email" => Role::Junk,
            "archive" | "archiv" | "all mail" => Role::Archive,
            "outbox" | "postausgang" => Role::Outbox,
            _ => Role::Other,
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        let r = Role::detect(s);
        (r != Role::Other).then_some(r)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Mailbox {
    pub id: i64,
    pub account: String,
    pub path: String,
    pub role: Role,
    pub total: i64,
    pub unread: i64,
    #[serde(skip)]
    pub account_uuid: String,
    #[serde(skip)]
    pub local: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub name: String,
    pub uuid: String,
    pub mailboxes: usize,
    pub messages: i64,
    pub unread: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageRow {
    pub id: i64,
    pub date: String,
    pub from: String,
    pub subject: String,
    pub account: String,
    pub mailbox: String,
    pub read: bool,
    pub flagged: bool,
    pub answered: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub forwarded: bool,
    #[serde(skip_serializing_if = "is_zero")]
    pub attachments: i64,
    pub thread: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<&'static str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub important: bool,
    /// Apple Intelligence summary written by Mail on this Mac.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip)]
    pub ts: i64,
    #[serde(skip)]
    pub mailbox_id: i64,
    #[serde(skip)]
    pub sender_address: String,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Serialize)]
pub struct Recipient {
    pub kind: &'static str,
    pub address: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
}

#[derive(Debug, Default, Clone)]
pub struct Filter {
    pub ids: Option<Vec<i64>>,
    pub after_id: Option<i64>,
    pub mailbox_ids: Option<Vec<i64>>,
    pub exclude_mailbox_ids: Vec<i64>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub subject: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub unread: Option<bool>,
    pub flagged: Option<bool>,
    pub answered: Option<bool>,
    pub has_attachments: Option<bool>,
    pub thread: Option<i64>,
    pub category: Option<i64>,
    pub important: Option<bool>,
    pub invite: Option<bool>,
    pub limit: usize,
    pub offset: usize,
    pub oldest_first: bool,
}

pub struct SubscriptionMsg {
    pub id: i64,
    pub address: String,
    pub name: String,
    pub ts: i64,
    pub read: bool,
    pub mailbox_id: i64,
    pub category: Option<&'static str>,
    pub unsubscribe: crate::emlx::Unsubscribe,
}

pub struct Store {
    conn: Connection,
    has_index: bool,
    pub mail_root: PathBuf,
    mailboxes: Vec<Mailbox>,
    mailbox_dirs: HashMap<i64, Option<PathBuf>>,
    account_names: HashMap<String, String>,
    config: Config,
    hidden: Vec<i64>,
}

pub fn find_mail_root() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("MAILIDX_MAIL_DIR") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var("HOME").context("HOME not set")?;
    let base = Path::new(&home).join("Library/Mail");
    let mut best: Option<(u32, PathBuf)> = None;
    let entries = std::fs::read_dir(&base).with_context(|| {
        format!(
            "cannot read {} — grant Full Disk Access to the app that launches mailidx",
            base.display()
        )
    })?;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if let Some(v) = name.strip_prefix('V').and_then(|n| n.parse::<u32>().ok())
            && e.path().join("MailData/Envelope Index").exists()
                && best.as_ref().is_none_or(|(b, _)| v > *b)
            {
                best = Some((v, e.path()));
            }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| anyhow!("no ~/Library/Mail/V*/MailData/Envelope Index found"))
}

fn load_account_names() -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(home) = std::env::var("HOME") else { return out };
    let path = Path::new(&home).join("Library/Accounts/Accounts4.sqlite");
    let Ok(conn) = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return out;
    };
    let sql = "SELECT a.ZIDENTIFIER, COALESCE(NULLIF(a.ZACCOUNTDESCRIPTION,''), p.ZACCOUNTDESCRIPTION, a.ZUSERNAME)
               FROM ZACCOUNT a LEFT JOIN ZACCOUNT p ON p.Z_PK = a.ZPARENTACCOUNT";
    if let Ok(mut st) = conn.prepare(sql) {
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?))
        });
        if let Ok(rows) = rows {
            for (id, name) in rows.flatten() {
                if let (Some(id), Some(name)) = (id, name) {
                    out.insert(id, name);
                }
            }
        }
    }
    out
}

fn nfc(s: &str) -> String {
    s.nfc().collect()
}

impl Store {
    pub fn open() -> Result<Store> {
        let mail_root = find_mail_root()?;
        let db = mail_root.join("MailData/Envelope Index");
        let conn = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("cannot open {}", db.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(3))?;
        check_schema(&conn)?;
        let has_index = attach_index(&conn);
        let mut store = Store {
            has_index,
            conn,
            mail_root,
            mailboxes: Vec::new(),
            mailbox_dirs: HashMap::new(),
            account_names: load_account_names(),
            config: Config::load_or_default(),
            hidden: Vec::new(),
        };
        store.refresh_mailboxes()?;
        Ok(store)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn refresh_mailboxes(&mut self) -> Result<()> {
        let mut counts: HashMap<i64, (i64, i64)> = HashMap::new();
        {
            let mut st = self.conn.prepare_cached(
                "SELECT mailbox, COUNT(*), SUM(read = 0) FROM messages WHERE deleted = 0 GROUP BY mailbox",
            )?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?)))?;
            for row in rows {
                let (mb, total, unread) = row?;
                counts.insert(mb, (total, unread));
            }
        }
        let mut st = self.conn.prepare_cached("SELECT ROWID, url FROM mailboxes")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, url) = row?;
            let Some((scheme, rest)) = url.split_once("://") else { continue };
            let (uuid, raw_path) = rest.split_once('/').unwrap_or((rest, ""));
            let uuid = uuid.to_string();
            let path = nfc(&percent_decode_str(raw_path).decode_utf8_lossy());
            let local = scheme == "local";
            let account = self
                .account_names
                .get(&uuid)
                .cloned()
                .unwrap_or_else(|| if local { "On My Mac".into() } else { uuid.clone() });
            let leaf = path.rsplit('/').next().unwrap_or(&path).to_string();
            let (total, unread) = counts.get(&id).copied().unwrap_or((0, 0));
            out.push(Mailbox {
                id,
                account,
                role: Role::detect(&leaf),
                path,
                total,
                unread,
                account_uuid: uuid,
                local,
            });
        }
        out.sort_by(|a, b| a.account.cmp(&b.account).then(a.path.cmp(&b.path)));
        let (hidden, visible): (Vec<_>, Vec<_>) =
            out.into_iter().partition(|m| self.config.hides_mailbox(&m.account, &m.path));
        self.hidden = hidden.iter().map(|m| m.id).collect();
        self.mailboxes = visible;
        Ok(())
    }

    pub fn hidden_mailbox_count(&self) -> usize {
        self.hidden.len()
    }

    pub fn mailboxes(&self) -> &[Mailbox] {
        &self.mailboxes
    }

    pub fn mailbox(&self, id: i64) -> Option<&Mailbox> {
        self.mailboxes.iter().find(|m| m.id == id)
    }

    pub fn accounts(&self) -> Vec<Account> {
        let mut map: HashMap<String, Account> = HashMap::new();
        for mb in &self.mailboxes {
            let a = map.entry(mb.account.clone()).or_insert_with(|| Account {
                name: mb.account.clone(),
                uuid: mb.account_uuid.clone(),
                mailboxes: 0,
                messages: 0,
                unread: 0,
            });
            a.mailboxes += 1;
            a.messages += mb.total;
            if !matches!(mb.role, Role::Trash | Role::Junk | Role::Sent | Role::Drafts) {
                a.unread += mb.unread;
            }
        }
        let mut v: Vec<_> = map.into_values().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Resolves a user-supplied mailbox reference: a role word ("inbox", "sent"),
    /// a path ("Business", "Projects/2026") or a numeric id. Role words match every
    /// account's mailbox of that role, which is what "search my inbox" means.
    pub fn resolve_mailboxes(&self, spec: &str, account: Option<&str>) -> Result<Vec<i64>> {
        let spec_n = nfc(spec.trim());
        let acct_ok = |m: &Mailbox| {
            account.is_none_or(|a| m.account.eq_ignore_ascii_case(a) || m.account_uuid == a)
        };
        if let Ok(id) = spec_n.parse::<i64>()
            && self.mailbox(id).is_some() {
                return Ok(vec![id]);
            }
        let exact: Vec<i64> = self
            .mailboxes
            .iter()
            .filter(|m| acct_ok(m) && m.path.eq_ignore_ascii_case(&spec_n))
            .map(|m| m.id)
            .collect();
        if !exact.is_empty() {
            return Ok(exact);
        }
        if let Some(role) = Role::parse(&spec_n) {
            let ids: Vec<i64> = self
                .mailboxes
                .iter()
                .filter(|m| acct_ok(m) && m.role == role)
                .map(|m| m.id)
                .collect();
            if !ids.is_empty() {
                return Ok(ids);
            }
        }
        let lower = spec_n.to_lowercase();
        let fuzzy: Vec<i64> = self
            .mailboxes
            .iter()
            .filter(|m| acct_ok(m) && m.path.to_lowercase().contains(&lower))
            .map(|m| m.id)
            .collect();
        if fuzzy.is_empty() {
            bail!("no mailbox matches '{spec}'. Call the mailboxes tool to see what exists.");
        }
        Ok(fuzzy)
    }

    pub fn account_mailbox_ids(&self, account: &str) -> Result<Vec<i64>> {
        let ids: Vec<i64> = self
            .mailboxes
            .iter()
            .filter(|m| m.account.eq_ignore_ascii_case(account) || m.account_uuid == account)
            .map(|m| m.id)
            .collect();
        if ids.is_empty() {
            bail!("no account named '{account}'");
        }
        Ok(ids)
    }

    pub fn role_ids(&self, roles: &[Role]) -> Vec<i64> {
        self.mailboxes.iter().filter(|m| roles.contains(&m.role)).map(|m| m.id).collect()
    }

    fn attachment_count_sql(&self) -> &'static str {
        if self.has_index {
            "MAX(0, (SELECT COUNT(*) FROM attachments t WHERE t.message = m.ROWID)
                  - COALESCE((SELECT x.inline FROM idx.doc x WHERE x.id = m.ROWID), 0))"
        } else {
            "(SELECT COUNT(*) FROM attachments t WHERE t.message = m.ROWID)"
        }
    }

    pub fn list(&self, f: &Filter) -> Result<Vec<MessageRow>> {
        let att = self.attachment_count_sql();
        let mut sql = format!(
            "SELECT m.ROWID, COALESCE(m.subject_prefix,'') || COALESCE(s.subject,''), a.address, a.comment,
                    m.date_received, m.mailbox, m.read, m.flagged, m.flags, m.conversation_id,
                    {att}, su.summary,
                    g.model_category, COALESCE(g.model_high_impact, 0), gs.summary
             FROM messages m
             LEFT JOIN message_global_data g ON g.ROWID = m.global_message_id
             LEFT JOIN generated_summaries gs ON gs.ROWID = g.generated_summary
             LEFT JOIN subjects s ON s.ROWID = m.subject
             LEFT JOIN addresses a ON a.ROWID = m.sender
             LEFT JOIN summaries su ON su.ROWID = m.summary
             WHERE m.deleted = 0",
        );
        let mut args: Vec<Value> = Vec::new();
        let in_list = |sql: &mut String, col: &str, ids: &[i64], neg: bool| {
            let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
            sql.push_str(&format!(" AND {col} {}IN ({list})", if neg { "NOT " } else { "" }));
        };
        if let Some(ids) = &f.ids {
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            in_list(&mut sql, "m.ROWID", ids, false);
        }
        if let Some(id) = f.after_id {
            sql.push_str(" AND m.ROWID > ?");
            args.push(id.into());
        }
        if let Some(mbs) = &f.mailbox_ids {
            if mbs.is_empty() {
                return Ok(Vec::new());
            }
            in_list(&mut sql, "m.mailbox", mbs, false);
        }
        if !f.exclude_mailbox_ids.is_empty() {
            in_list(&mut sql, "m.mailbox", &f.exclude_mailbox_ids, true);
        }
        if !self.hidden.is_empty() {
            in_list(&mut sql, "m.mailbox", &self.hidden, true);
        }
        if let Some(from) = &f.from {
            sql.push_str(" AND (a.address LIKE ? OR a.comment LIKE ?)");
            let p = format!("%{from}%");
            args.push(p.clone().into());
            args.push(p.into());
        }
        if let Some(to) = &f.to {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM recipients r JOIN addresses ra ON ra.ROWID = r.address
                  WHERE r.message = m.ROWID AND (ra.address LIKE ? OR ra.comment LIKE ?))",
            );
            let p = format!("%{to}%");
            args.push(p.clone().into());
            args.push(p.into());
        }
        if let Some(subj) = &f.subject {
            sql.push_str(" AND s.subject LIKE ?");
            args.push(format!("%{subj}%").into());
        }
        if let Some(t) = f.since {
            sql.push_str(" AND m.date_received >= ?");
            args.push(t.into());
        }
        if let Some(t) = f.until {
            sql.push_str(" AND m.date_received < ?");
            args.push(t.into());
        }
        if let Some(u) = f.unread {
            sql.push_str(if u { " AND m.read = 0" } else { " AND m.read = 1" });
        }
        if let Some(fl) = f.flagged {
            sql.push_str(if fl { " AND m.flagged = 1" } else { " AND m.flagged = 0" });
        }
        if let Some(an) = f.answered {
            sql.push_str(&format!(" AND (m.flags & {FLAG_ANSWERED}) {} 0", if an { "!=" } else { "=" }));
        }
        if let Some(h) = f.has_attachments {
            sql.push_str(&format!(" AND {att} {} 0", if h { ">" } else { "=" }));
        }
        if let Some(c) = f.category {
            sql.push_str(" AND g.model_category = ?");
            args.push(c.into());
        }
        if let Some(i) = f.important {
            sql.push_str(if i { " AND g.model_high_impact = 1" } else { " AND COALESCE(g.model_high_impact,0) = 0" });
        }
        if let Some(inv) = f.invite {
            if !self.has_index {
                bail!("has_invite needs the search index; run `mailidx index` once");
            }
            sql.push_str(&format!(
                " AND COALESCE((SELECT x.invite FROM idx.doc x WHERE x.id = m.ROWID), 0) = {}",
                inv as i64
            ));
        }
        if let Some(c) = f.thread {
            sql.push_str(" AND m.conversation_id = ?");
            args.push(c.into());
        }
        sql.push_str(if f.oldest_first {
            " ORDER BY m.date_received ASC"
        } else {
            " ORDER BY m.date_received DESC"
        });
        if f.limit > 0 {
            sql.push_str(&format!(" LIMIT {} OFFSET {}", f.limit, f.offset));
        }
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(params_from_iter(args.iter()), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, i64>(9)?,
                r.get::<_, i64>(10)?,
                r.get::<_, Option<String>>(11)?,
                r.get::<_, Option<i64>>(12)?,
                r.get::<_, i64>(13)?,
                r.get::<_, Option<Vec<u8>>>(14)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, subject, addr, comment, ts, mb, read, flagged, flags, conv, natt, summary, cat, hi, ai) =
                row?;
            let mbx = self.mailbox(mb);
            let addr = addr.unwrap_or_default();
            let ts = ts.unwrap_or(0);
            out.push(MessageRow {
                id,
                date: fmt_ts(ts),
                from: format_addr(&addr, comment.as_deref().unwrap_or("")),
                subject,
                account: mbx.map(|m| m.account.clone()).unwrap_or_default(),
                mailbox: mbx.map(|m| m.path.clone()).unwrap_or_default(),
                read: read != 0,
                flagged: flagged != 0,
                answered: flags & FLAG_ANSWERED != 0,
                forwarded: flags & FLAG_FORWARDED != 0,
                attachments: natt,
                thread: conv,
                category: cat.and_then(category_name),
                important: hi != 0,
                summary: ai.as_deref().and_then(decode_summary),
                preview: summary.map(|s| truncate_chars(s.trim(), 200)),
                snippet: None,
                ts,
                mailbox_id: mb,
                sender_address: addr.to_lowercase(),
            });
        }
        Ok(out)
    }

    pub fn get(&self, id: i64) -> Result<MessageRow> {
        let f = Filter { ids: Some(vec![id]), ..Default::default() };
        self.list(&f)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("message {id} not found (deleted or never indexed by Mail)"))
    }

    /// Mail gives a moved message a new ROWID; `global_message_id` survives the move.
    pub fn global_id(&self, id: i64) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT global_message_id FROM messages WHERE ROWID = ?", [id], |r| r.get(0))?)
    }

    pub fn id_in_mailbox(&self, global_id: i64, mailbox_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT ROWID FROM messages WHERE global_message_id = ? AND mailbox = ? AND deleted = 0
                 ORDER BY ROWID DESC LIMIT 1",
                [global_id, mailbox_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn recipients(&self, id: i64) -> Result<Vec<Recipient>> {
        let mut st = self.conn.prepare_cached(
            "SELECT r.type, a.address, a.comment FROM recipients r JOIN addresses a ON a.ROWID = r.address
             WHERE r.message = ? ORDER BY r.type, r.position",
        )?;
        let rows = st.query_map([id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (t, address, name) = row?;
            let kind = match t {
                0 => "to",
                1 => "cc",
                2 => "bcc",
                _ => "other",
            };
            out.push(Recipient { kind, address, name });
        }
        Ok(out)
    }

    pub fn attachment_names(&self, id: i64) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare_cached(
            "SELECT COALESCE(attachment_id,''), COALESCE(name,'') FROM attachments WHERE message = ? ORDER BY ROWID",
        )?;
        let rows = st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Mail stores every message as `<rowid>.emlx` (or `.partial.emlx` when the
    /// attachments live next to it) under a directory derived from the rowid:
    /// the decimal digits of `rowid / 1000`, reversed, one directory each.
    pub fn emlx_path(&mut self, id: i64, mailbox_id: i64) -> Option<PathBuf> {
        let dir = self.mailbox_data_dir(mailbox_id)?;
        let mut d = dir.clone();
        let mut n = id / 1000;
        while n > 0 {
            d.push((n % 10).to_string());
            n /= 10;
        }
        let msgs = d.join("Messages");
        for name in [format!("{id}.emlx"), format!("{id}.partial.emlx")] {
            let p = msgs.join(&name);
            if p.exists() {
                return Some(p);
            }
        }
        find_file(&dir, &format!("{id}.emlx"), &format!("{id}.partial.emlx"))
    }

    fn mailbox_data_dir(&mut self, mailbox_id: i64) -> Option<PathBuf> {
        if let Some(cached) = self.mailbox_dirs.get(&mailbox_id) {
            return cached.clone();
        }
        let resolved = self.mailbox(mailbox_id).and_then(|mb| {
            let mut p = self.mail_root.join(&mb.account_uuid);
            for seg in mb.path.split('/') {
                p.push(format!("{seg}.mbox"));
            }
            if !p.exists() {
                p = find_mbox_dir(&self.mail_root.join(&mb.account_uuid), &mb.path)?;
            }
            std::fs::read_dir(&p)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .find(|c| c.join("Data").is_dir())
                .map(|c| c.join("Data"))
        });
        self.mailbox_dirs.insert(mailbox_id, resolved.clone());
        resolved
    }

    pub fn live_ids(&self) -> Result<Vec<(i64, i64)>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT ROWID, mailbox FROM messages WHERE deleted = 0")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, mb) = row?;
            if !self.hidden.contains(&mb) {
                out.push((id, mb));
            }
        }
        Ok(out)
    }

    /// Messages carrying a List-Unsubscribe header, newest first.
    pub fn subscriptions(&self, since: i64, exclude: &[i64]) -> Result<Vec<SubscriptionMsg>> {
        if !self.has_index {
            bail!("needs the search index; run `mailidx index` once");
        }
        let hidden: Vec<String> = self.hidden.iter().chain(exclude).map(|i| i.to_string()).collect();
        let not_in = if hidden.is_empty() { String::new() } else { format!(" AND m.mailbox NOT IN ({})", hidden.join(",")) };
        let sql = format!(
            "SELECT m.ROWID, COALESCE(a.address,''), COALESCE(a.comment,''), m.date_received, m.read, m.mailbox,
                    g.model_category, x.unsub_url, x.unsub_mailto, x.unsub_one_click
             FROM messages m
             JOIN idx.doc x ON x.id = m.ROWID
             LEFT JOIN addresses a ON a.ROWID = m.sender
             LEFT JOIN message_global_data g ON g.ROWID = m.global_message_id
             WHERE m.deleted = 0 AND m.date_received >= ?
               AND (x.unsub_url IS NOT NULL OR x.unsub_mailto IS NOT NULL){not_in}
             ORDER BY m.date_received DESC"
        );
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map([since], |r| {
            Ok(SubscriptionMsg {
                id: r.get(0)?,
                address: r.get(1)?,
                name: r.get(2)?,
                ts: r.get(3)?,
                read: r.get::<_, i64>(4)? != 0,
                mailbox_id: r.get(5)?,
                category: r.get::<_, Option<i64>>(6)?.and_then(category_name),
                unsubscribe: crate::emlx::Unsubscribe {
                    url: r.get(7)?,
                    mailto: r.get(8)?,
                    one_click: r.get::<_, i64>(9)? != 0,
                },
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// (address, display name, messages, latest date) for senders of mail outside `exclude`.
    pub fn sender_counts(&self, exclude: &[i64]) -> Result<Vec<(String, String, i64, i64)>> {
        let skip: Vec<String> = self.hidden.iter().chain(exclude).map(|i| i.to_string()).collect();
        let not_in = if skip.is_empty() { String::new() } else { format!(" AND m.mailbox NOT IN ({})", skip.join(",")) };
        self.address_counts(&format!(
            "SELECT LOWER(a.address), MAX(COALESCE(a.comment,'')), COUNT(*), MAX(m.date_received)
             FROM messages m JOIN addresses a ON a.ROWID = m.sender
             WHERE m.deleted = 0{not_in} GROUP BY LOWER(a.address)"
        ))
    }

    /// Same for recipients (to/cc) of mail in `include`, i.e. the sent mailboxes.
    pub fn recipient_counts(&self, include: &[i64]) -> Result<Vec<(String, String, i64, i64)>> {
        let ids: Vec<String> = include.iter().filter(|i| !self.hidden.contains(i)).map(|i| i.to_string()).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.address_counts(&format!(
            "SELECT LOWER(ra.address), MAX(COALESCE(ra.comment,'')), COUNT(DISTINCT m.ROWID), MAX(m.date_received)
             FROM recipients r JOIN messages m ON m.ROWID = r.message JOIN addresses ra ON ra.ROWID = r.address
             WHERE m.deleted = 0 AND m.mailbox IN ({}) GROUP BY LOWER(ra.address)",
            ids.join(",")
        ))
    }

    fn address_counts(&self, sql: &str) -> Result<Vec<(String, String, i64, i64)>> {
        let mut st = self.conn.prepare(sql)?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    pub fn fingerprint(&self) -> Result<(i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(ROWID),0), COUNT(*) FROM messages WHERE deleted = 0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }
}

/// Our own index knows inline images, unsubscribe headers and invitations.
/// It is attached to the read-only connection, so it is only read; an index
/// from an older mailidx without those columns is left out.
fn attach_index(conn: &Connection) -> bool {
    let Some(path) = crate::fts::index_path().ok().filter(|p| p.exists()) else {
        return false;
    };
    if conn.execute("ATTACH DATABASE ? AS idx", [path.to_string_lossy()]).is_err() {
        return false;
    }
    let ok = conn
        .prepare("SELECT inline, unsub_url, unsub_mailto, unsub_one_click, invite FROM idx.doc LIMIT 0")
        .is_ok();
    if !ok {
        let _ = conn.execute("DETACH DATABASE idx", []);
    }
    ok
}

/// Mail writes folder names in NFD on disk while the index stores them
/// percent-encoded; compare normalized names when the direct path misses.
fn find_mbox_dir(account_dir: &Path, path: &str) -> Option<PathBuf> {
    let mut cur = account_dir.to_path_buf();
    for seg in path.split('/') {
        let want = nfc(&format!("{seg}.mbox"));
        let next = std::fs::read_dir(&cur)
            .ok()?
            .flatten()
            .find(|e| nfc(&e.file_name().to_string_lossy()) == want)?
            .path();
        cur = next;
    }
    Some(cur)
}

fn find_file(dir: &Path, a: &str, b: &str) -> Option<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let p = e.path();
            if p.is_dir() {
                if e.file_name() != "Attachments" {
                    stack.push(p);
                }
            } else {
                let n = e.file_name();
                if n == a || n == b {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn check_schema(conn: &Connection) -> Result<()> {
    let mut missing = Vec::new();
    for (table, cols) in REQUIRED_COLUMNS {
        let mut st = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let have: Vec<String> = st
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<_, _>>()?;
        if have.is_empty() {
            missing.push(format!("table {table}"));
            continue;
        }
        for c in *cols {
            if *c != "ROWID" && !have.iter().any(|h| h == c) {
                missing.push(format!("{table}.{c}"));
            }
        }
    }
    if !missing.is_empty() {
        bail!(
            "Mail's Envelope Index changed in this macOS version; missing: {}. Please open an issue with your macOS version.",
            missing.join(", ")
        );
    }
    Ok(())
}

pub const CATEGORIES: &[&str] = &["primary", "transactions", "updates", "promotions"];

fn category_name(c: i64) -> Option<&'static str> {
    CATEGORIES.get(usize::try_from(c).ok()?).copied()
}

pub fn category_id(name: &str) -> Option<i64> {
    let n = name.to_lowercase();
    let n = match n.as_str() {
        "primär" | "primaer" | "main" => "primary",
        "transaktionen" | "transaction" | "receipts" => "transactions",
        "update" | "notifications" => "updates",
        "werbung" | "promotion" | "marketing" | "newsletter" => "promotions",
        other => other,
    };
    CATEGORIES.iter().position(|c| *c == n).map(|p| p as i64)
}

/// Mail keeps its on-device summaries as an NSKeyedArchiver'd NSAttributedString;
/// the plain text is the string object the root's `NSString` key points at.
fn decode_summary(blob: &[u8]) -> Option<String> {
    let v: plist::Value = plist::from_bytes(blob).ok()?;
    let objects = v.as_dictionary()?.get("$objects")?.as_array()?;
    let from_root = objects.iter().find_map(|o| {
        let uid = o.as_dictionary()?.get("NSString")?.as_uid()?.get();
        objects.get(uid as usize)?.as_string().map(str::to_string)
    });
    from_root
        .or_else(|| {
            objects
                .iter()
                .filter_map(|o| o.as_string())
                .filter(|s| *s != "$null")
                .max_by_key(|s| s.len())
                .map(str::to_string)
        })
        .filter(|s| !s.trim().is_empty())
}

pub fn format_addr(addr: &str, name: &str) -> String {
    let name = name.trim().trim_matches('"');
    if name.is_empty() || name.eq_ignore_ascii_case(addr) {
        addr.to_string()
    } else {
        format!("{name} <{addr}>")
    }
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}
