use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::{self, JsonSchema};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::emlx;
use crate::fts::Index;
use crate::script::{self, Kind, MsgRef, Outgoing};
use crate::store::{Filter, MessageRow, Role, Store, category_id, format_addr, truncate_chars};
use crate::time::{fmt_ts, now, parse_when};

type ToolResult = std::result::Result<CallToolResult, ErrorData>;

struct Inner {
    store: Mutex<Store>,
    index: Mutex<Index>,
    last_refresh: Mutex<Instant>,
    my_addresses: tokio::sync::Mutex<Option<Vec<(String, Vec<String>)>>>,
}

#[derive(Clone)]
pub struct Server {
    inner: Arc<Inner>,
    tool_router: ToolRouter<Self>,
}

fn ok<T: Serialize>(v: &T) -> ToolResult {
    let text = serde_json::to_string(v).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"));
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

fn fail(e: anyhow::Error) -> ToolResult {
    Ok(CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]))
}

macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return fail(e.into()),
        }
    };
}

impl Server {
    pub fn new(store: Store, index: Index) -> Server {
        Server {
            inner: Arc::new(Inner {
                store: Mutex::new(store),
                index: Mutex::new(index),
                last_refresh: Mutex::new(Instant::now()),
                my_addresses: tokio::sync::Mutex::new(None),
            }),
            tool_router: Self::tool_router(),
        }
    }

    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Store, &Mutex<Index>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = inner.store.lock().map_err(|_| anyhow!("store lock poisoned"))?;
            {
                let mut last = inner.last_refresh.lock().map_err(|_| anyhow!("lock poisoned"))?;
                if last.elapsed().as_secs() >= 3 {
                    store.refresh_mailboxes()?;
                    *last = Instant::now();
                }
            }
            f(&mut store, &inner.index)
        })
        .await?
    }

    /// Mail's account addresses need one AppleScript round trip; fetch them while
    /// the client is still initializing so the first triage call does not wait.
    pub fn warm_up(&self) {
        let me = self.clone();
        tokio::spawn(async move {
            me.addresses().await;
        });
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.live_index().await {
                eprintln!("mailidx: live index off: {e:#}");
            }
        });
    }

    /// Keeps the full-text index in step with Mail. FSEvents reports every write
    /// to Mail's Envelope Index (and its WAL); after a quiet second the index syncs
    /// in the background, so searches never pay for indexing new mail.
    async fn live_index(&self) -> Result<()> {
        use notify::{RecursiveMode, Watcher};
        let dir = {
            let store = self.inner.store.lock().map_err(|_| anyhow!("store lock poisoned"))?;
            store.mail_root.join("MailData")
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let mut watcher = notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
            if let Ok(ev) = ev
                && ev.paths.iter().any(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("Envelope Index")))
            {
                let _ = tx.send(());
            }
        })?;
        watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        self.sync_index().await;
        while rx.recv().await.is_some() {
            loop {
                match tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await {
                    Ok(Some(())) => continue,
                    Ok(None) => return Ok(()),
                    Err(_) => break,
                }
            }
            self.sync_index().await;
        }
        drop(watcher);
        Ok(())
    }

    async fn sync_index(&self) {
        let res = self
            .with(|s, idx| {
                let mut idx = idx.lock().map_err(|_| anyhow!("index lock poisoned"))?;
                idx.sync(s)
            })
            .await;
        match res {
            Ok(st) if st.added + st.removed + st.upgraded > 0 => {
                eprintln!("mailidx: index +{} -{} ~{} in {} ms", st.added, st.removed, st.upgraded, st.ms)
            }
            Ok(_) => {}
            Err(e) => eprintln!("mailidx: background sync failed: {e:#}"),
        }
    }

    async fn addresses(&self) -> Vec<(String, Vec<String>)> {
        let mut guard = self.inner.my_addresses.lock().await;
        if guard.is_none()
            && let Ok(a) = script::account_addresses().await {
                *guard = Some(a);
            }
        guard.clone().unwrap_or_default()
    }

    async fn me(&self) -> HashSet<String> {
        let mut set: HashSet<String> =
            self.addresses().await.into_iter().flat_map(|(_, a)| a).collect();
        if let Ok(sent) = self
            .with(|s, _| {
                let ids = s.role_ids(&[Role::Sent]);
                let rows = s.list(&Filter { mailbox_ids: Some(ids), limit: 500, ..Default::default() })?;
                Ok(rows.into_iter().map(|r| r.sender_address).collect::<HashSet<_>>())
            })
            .await
        {
            set.extend(sent);
        }
        set.retain(|a| !a.is_empty());
        set
    }

    async fn msg_ref(&self, id: i64) -> Result<(MsgRef, MessageRow)> {
        self.with(move |s, _| {
            let row = s.get(id)?;
            let mb = s.mailbox(row.mailbox_id).ok_or_else(|| anyhow!("mailbox missing"))?;
            Ok((
                MsgRef {
                    account: if mb.local { String::new() } else { mb.account.clone() },
                    mailbox: mb.path.clone(),
                    id,
                },
                row,
            ))
        })
        .await
    }
}

fn default_limit() -> usize {
    25
}
fn default_true() -> bool {
    true
}
fn default_max_chars() -> usize {
    20_000
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct AccountArg {
    /// Account name as shown by the accounts tool (e.g. "iCloud"). Optional.
    pub account: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ListArgs {
    /// Mailbox: a role ("inbox", "sent", "drafts", "trash", "junk", "archive"), a folder path ("Business", "Projects/2026") or a mailbox id. Default: inbox of every account.
    pub mailbox: Option<String>,
    pub account: Option<String>,
    /// Only unread (true) or only read (false) messages.
    pub unread: Option<bool>,
    pub flagged: Option<bool>,
    /// Apple Intelligence category Mail assigned on this Mac: primary, transactions, updates, promotions.
    pub category: Option<String>,
    /// Only messages Mail marked as important (high impact).
    pub important: Option<bool>,
    /// Lower bound on date received: "2026-11-01", "2026-11-01 08:00", "today", or relative "7d", "12h", "2w".
    pub since: Option<String>,
    /// Upper bound on date received (exclusive), same formats as since.
    pub until: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub oldest_first: bool,
    /// Include Mail's short preview text per message.
    #[serde(default = "default_true")]
    pub preview: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Full-text query over subject, sender, recipients, body and attachment names. Words are ANDed and prefix-matched; "quoted phrase" for exact phrases; -word excludes; OR between words. Diacritics are ignored (Muller finds Müller).
    pub query: Option<String>,
    /// Sender name or address contains this text.
    pub from: Option<String>,
    /// A recipient (to/cc) name or address contains this text.
    pub to: Option<String>,
    /// Subject contains this text.
    pub subject: Option<String>,
    pub mailbox: Option<String>,
    pub account: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub unread: Option<bool>,
    pub flagged: Option<bool>,
    pub has_attachments: Option<bool>,
    /// Apple Intelligence category Mail assigned on this Mac: primary, transactions, updates, promotions.
    pub category: Option<String>,
    /// Only messages Mail marked as important (high impact).
    pub important: Option<bool>,
    /// Also search trash and junk (excluded by default unless mailbox says otherwise).
    #[serde(default)]
    pub include_trash: bool,
    /// "relevance" (default when query is set) or "date".
    pub sort: Option<String>,
    /// true: a message matches if any word matches (ranked by how many do).
    /// Default false: all words must match; if that finds nothing, any-word
    /// matching is tried automatically and the result says "matched": "any".
    #[serde(default)]
    pub match_any: bool,
    /// Pass query to SQLite FTS5 unchanged (advanced: NEAR, column filters like subject:foo).
    #[serde(default)]
    pub raw_query: bool,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetArgs {
    /// Message id from list_messages or search.
    pub id: i64,
    /// Cut the body after this many characters.
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
    /// "text" (default) or "html" for the raw HTML part.
    pub format: Option<String>,
    /// Extract every link (href) in the message.
    #[serde(default)]
    pub links: bool,
    /// Include all raw headers.
    #[serde(default)]
    pub all_headers: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct ThreadArgs {
    /// Any message id of the conversation.
    pub id: i64,
    #[serde(default = "default_thread_chars")]
    pub max_chars_per_message: usize,
}
fn default_thread_chars() -> usize {
    4_000
}

#[derive(Deserialize, JsonSchema)]
pub struct SourceArgs {
    pub id: i64,
    #[serde(default)]
    pub headers_only: bool,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
}
fn default_max_bytes() -> usize {
    256 * 1024
}

#[derive(Deserialize, JsonSchema)]
pub struct SaveAttArgs {
    pub id: i64,
    /// Attachment index from get_message. Omit to save all attachments.
    pub index: Option<usize>,
    /// Target directory. Default ~/Downloads. Existing files are never overwritten.
    pub directory: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ExportArgs {
    /// Message ids to export. Alternatively use query/mailbox/since.
    pub ids: Option<Vec<i64>>,
    pub query: Option<String>,
    pub mailbox: Option<String>,
    pub since: Option<String>,
    /// "eml" (original RFC 822, default), "md" (readable text) or "json" (one file with all messages).
    pub format: Option<String>,
    /// Target directory, created if missing.
    pub directory: String,
    #[serde(default = "default_export_limit")]
    pub limit: usize,
}
fn default_export_limit() -> usize {
    200
}

#[derive(Deserialize, JsonSchema)]
pub struct StatsArgs {
    /// Time window, default "30d".
    pub since: Option<String>,
    pub account: Option<String>,
    #[serde(default = "default_top")]
    pub top: usize,
}
fn default_top() -> usize {
    10
}

#[derive(Deserialize, JsonSchema)]
pub struct TriageArgs {
    /// "needs_reply": mail from people that you have not answered. "awaiting_reply": mail you sent that got no answer yet.
    pub mode: String,
    /// Look-back window, default "14d".
    pub since: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Deserialize, JsonSchema)]
pub struct IndexArgs {
    /// Throw the index away and rebuild it from the .emlx files.
    #[serde(default)]
    pub rebuild: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct ComposeArgs {
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub subject: String,
    /// Plain-text body.
    pub body: String,
    /// Optional rich-text (HTML) version of body: bold, links, lists. Pasted as formatted text.
    pub html_body: Option<String>,
    /// Sender address or "Name <address>"; must be one of your account addresses. Default: Mail's default.
    pub from: Option<String>,
    /// Absolute file paths to attach.
    #[serde(default)]
    pub attachments: Vec<String>,
    /// false (default): save as draft in Mail. true: send immediately.
    #[serde(default)]
    pub send: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct ReplyArgs {
    pub id: i64,
    pub body: String,
    #[serde(default)]
    pub reply_all: bool,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<String>,
    pub from: Option<String>,
    /// Optional rich-text version of body.
    pub html_body: Option<String>,
    /// false (default): save as draft. true: send immediately.
    #[serde(default)]
    pub send: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct ForwardArgs {
    pub id: i64,
    pub to: Vec<String>,
    /// Text above the forwarded message.
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    pub from: Option<String>,
    /// false (default): save as draft. true: send immediately.
    #[serde(default)]
    pub send: bool,
}

#[derive(Deserialize, JsonSchema)]
pub struct UpdateArgs {
    pub ids: Vec<i64>,
    /// One of: mark_read, mark_unread, flag, unflag, flag_color, move, trash, junk, not_junk.
    pub action: String,
    /// For move: target mailbox (role or path).
    pub mailbox: Option<String>,
    /// For move: target account, default the message's own account.
    pub account: Option<String>,
    /// For flag_color: red, orange, yellow, green, blue, purple, gray.
    pub color: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct DraftArgs {
    /// "list", "send" or "trash".
    pub action: String,
    /// Draft message id for send/trash.
    pub id: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
pub struct MailboxArgs {
    /// Name of the new mailbox; use "Parent/Child" for nesting.
    pub name: String,
    pub account: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct IdArg {
    pub id: i64,
}

fn base_filter(
    s: &Store,
    mailbox: Option<&str>,
    account: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Filter> {
    let mut f = Filter::default();
    match (mailbox, account) {
        (Some(m), a) => f.mailbox_ids = Some(s.resolve_mailboxes(m, a)?),
        (None, Some(a)) => f.mailbox_ids = Some(s.account_mailbox_ids(a)?),
        (None, None) => {}
    }
    if let Some(x) = since {
        f.since = Some(parse_when(x)?);
    }
    if let Some(x) = until {
        f.until = Some(parse_when(x)?);
    }
    Ok(f)
}

fn parse_category(c: Option<&str>) -> Result<Option<i64>> {
    match c {
        None => Ok(None),
        Some(c) => category_id(c)
            .map(Some)
            .ok_or_else(|| anyhow!("unknown category '{c}' (primary, transactions, updates, promotions)")),
    }
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (1..)
        .map(|i| dir.join(format!("{stem} ({i}){ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Ok(h) = std::env::var("HOME") {
            return PathBuf::from(h).join(rest);
        }
    PathBuf::from(p)
}

fn check_attachments(paths: &[String]) -> Result<Vec<String>> {
    paths
        .iter()
        .map(|p| {
            let pb = expand_home(p);
            if !pb.is_absolute() {
                bail!("attachment path must be absolute: {p}");
            }
            if !pb.is_file() {
                bail!("attachment not found: {p}");
            }
            Ok(pb.display().to_string())
        })
        .collect()
}

fn load_parsed<T>(
    s: &mut Store,
    id: i64,
    f: impl FnOnce(&emlx::Emlx, &mail_parser::Message<'_>) -> Result<T>,
) -> Result<T> {
    let row = s.get(id)?;
    let path = s
        .emlx_path(id, row.mailbox_id)
        .ok_or_else(|| anyhow!("message {id} has no local copy yet; Mail has not downloaded it"))?;
    let e = emlx::read(&path)?;
    let msg = emlx::parse(&e.raw).ok_or_else(|| anyhow!("could not parse message {id}"))?;
    f(&e, &msg)
}

const AUTOMATED_SENDERS: &[&str] = &[
    "noreply", "no-reply", "donotreply", "do-not-reply", "notification", "mailer-daemon",
    "postmaster", "bounce", "newsletter", "news@", "updates@", "alerts@",
];

#[tool_router]
impl Server {
    #[tool(
        description = "List mail accounts with their email addresses, mailbox count, message count and unread count.",
        annotations(read_only_hint = true)
    )]
    async fn accounts(&self) -> ToolResult {
        let accounts = tri!(self.with(|s, _| Ok(s.accounts())).await);
        let addrs: HashMap<String, Vec<String>> = self.addresses().await.into_iter().collect();
        let out: Vec<_> = accounts
            .into_iter()
            .map(|a| {
                json!({
                    "name": a.name,
                    "addresses": addrs.get(&a.name).cloned().unwrap_or_default(),
                    "mailboxes": a.mailboxes,
                    "messages": a.messages,
                    "unread": a.unread,
                })
            })
            .collect();
        ok(&out)
    }

    #[tool(
        description = "List mailboxes (folders) with role, message count and unread count. Instant, reads Mail's index directly.",
        annotations(read_only_hint = true)
    )]
    async fn mailboxes(&self, Parameters(a): Parameters<AccountArg>) -> ToolResult {
        let out = tri!(
            self.with(move |s, _| {
                s.refresh_mailboxes()?;
                Ok(s.mailboxes()
                    .iter()
                    .filter(|m| {
                        a.account.as_deref().is_none_or(|x| m.account.eq_ignore_ascii_case(x))
                    })
                    .cloned()
                    .collect::<Vec<_>>())
            })
            .await
        );
        ok(&out)
    }

    #[tool(
        description = "List messages in a mailbox, newest first. Defaults to the inbox of every account. Use for 'what's new', unread lists, or browsing a folder.",
        annotations(read_only_hint = true)
    )]
    async fn list_messages(&self, Parameters(a): Parameters<ListArgs>) -> ToolResult {
        let out = tri!(
            self.with(move |s, _| {
                let mailbox = a.mailbox.clone().unwrap_or_else(|| "inbox".into());
                let mut f = base_filter(
                    s,
                    Some(&mailbox),
                    a.account.as_deref(),
                    a.since.as_deref(),
                    a.until.as_deref(),
                )?;
                f.unread = a.unread;
                f.flagged = a.flagged;
                f.category = parse_category(a.category.as_deref())?;
                f.important = a.important;
                f.limit = a.limit.clamp(1, 500);
                f.offset = a.offset;
                f.oldest_first = a.oldest_first;
                let mut rows = s.list(&f)?;
                if !a.preview {
                    rows.iter_mut().for_each(|r| r.preview = None);
                }
                Ok(rows)
            })
            .await
        );
        ok(&out)
    }

    #[tool(
        description = "Search all mail. Full-text over subject, sender, recipients, body and attachment names via a local index (milliseconds, never touches Mail.app), combinable with filters. Trash and junk are skipped unless include_trash or mailbox says otherwise.",
        annotations(read_only_hint = true)
    )]
    async fn search(&self, Parameters(a): Parameters<SearchArgs>) -> ToolResult {
        let out = tri!(
            self.with(move |s, index| {
                let mut f = base_filter(
                    s,
                    a.mailbox.as_deref(),
                    a.account.as_deref(),
                    a.since.as_deref(),
                    a.until.as_deref(),
                )?;
                if a.mailbox.is_none() && !a.include_trash {
                    f.exclude_mailbox_ids = s.role_ids(&[Role::Trash, Role::Junk]);
                }
                f.from = a.from.clone();
                f.to = a.to.clone();
                f.subject = a.subject.clone();
                f.unread = a.unread;
                f.flagged = a.flagged;
                f.has_attachments = a.has_attachments;
                f.category = parse_category(a.category.as_deref())?;
                f.important = a.important;
                let limit = a.limit.clamp(1, 500);
                let query = a.query.as_deref().map(str::trim).filter(|q| !q.is_empty());
                let Some(q) = query else {
                    f.limit = limit;
                    f.offset = a.offset;
                    let rows = s.list(&f)?;
                    return Ok(json!({ "results": rows }));
                };
                let mut idx = index.lock().map_err(|_| anyhow!("index lock poisoned"))?;
                idx.sync(s)?;
                let mut matched = if a.match_any { "any" } else { "all" };
                let mut hits = idx.search(q, a.raw_query, a.match_any, 20_000)?;
                if hits.is_empty() && !a.match_any && !a.raw_query && q.split_whitespace().count() > 1 {
                    hits = idx.search(q, false, true, 20_000)?;
                    matched = "any";
                }
                drop(idx);
                let rank: HashMap<i64, usize> =
                    hits.iter().enumerate().map(|(i, h)| (h.id, i)).collect();
                let snippets: HashMap<i64, String> =
                    hits.into_iter().map(|h| (h.id, h.snippet)).collect();
                f.ids = Some(rank.keys().copied().collect());
                let mut rows = s.list(&f)?;
                let by_date = a.sort.as_deref() == Some("date");
                if !by_date {
                    rows.sort_by_key(|r| rank.get(&r.id).copied().unwrap_or(usize::MAX));
                }
                let total = rows.len();
                let mut page: Vec<MessageRow> =
                    rows.into_iter().skip(a.offset).take(limit).collect();
                for r in &mut page {
                    r.snippet = snippets.get(&r.id).filter(|x| !x.is_empty()).cloned();
                    if r.snippet.is_some() {
                        r.preview = None;
                    }
                }
                Ok(json!({ "total": total, "matched": matched, "results": page }))
            })
            .await
        );
        ok(&out)
    }

    #[tool(
        description = "Read one message: headers, recipients, body as text (or HTML), attachment list, optionally all links. Reads the .emlx file directly.",
        annotations(read_only_hint = true)
    )]
    async fn get_message(&self, Parameters(a): Parameters<GetArgs>) -> ToolResult {
        let out = tri!(
            self.with(move |s, _| {
                let row = s.get(a.id)?;
                let recipients = s.recipients(a.id)?;
                let db_atts = s.attachment_names(a.id)?;
                let parsed = load_parsed(s, a.id, |e, m| {
                    let body = if a.format.as_deref() == Some("html") {
                        m.body_html(0).map(|h| h.into_owned()).unwrap_or_default()
                    } else {
                        emlx::body_text(m)
                    };
                    let mut v = json!({
                        "message_id": m.message_id(),
                        "reply_to": emlx::addresses(m.reply_to()),
                        "sent": m.date().map(|d| fmt_ts(d.to_timestamp())),
                        "body": truncate_chars(&body, a.max_chars),
                        "body_chars": body.chars().count(),
                        "attachments": emlx::attachments(e, m),
                    });
                    if let Some(irt) = m.in_reply_to() {
                        v["in_reply_to"] = json!(irt.iter().collect::<Vec<_>>());
                    }
                    if a.links {
                        v["links"] = json!(emlx::links(m));
                    }
                    if a.all_headers {
                        let hs: Vec<(String, String)> = m
                            .headers()
                            .iter()
                            .map(|h| (h.name().as_str().to_string(), String::from_utf8_lossy(h.raw_value()).trim().to_string()))
                            .collect();
                        v["headers"] = json!(hs);
                    }
                    Ok(v)
                });
                let mut v = serde_json::to_value(&row)?;
                v["recipients"] = json!(recipients);
                match parsed {
                    Ok(extra) => {
                        if let (Some(o), Some(x)) = (v.as_object_mut(), extra.as_object()) {
                            for (k, val) in x {
                                o.insert(k.clone(), val.clone());
                            }
                        }
                    }
                    Err(e) => {
                        v["body"] = json!(row.preview);
                        v["note"] = json!(format!("{e:#}"));
                        v["attachments"] = json!(
                            db_atts.iter().map(|(_, n)| n).collect::<Vec<_>>()
                        );
                    }
                }
                Ok(v)
            })
            .await
        );
        ok(&out)
    }

    #[tool(
        description = "Read a whole conversation (all messages sharing the thread id, across mailboxes incl. your sent replies), oldest first, with bodies.",
        annotations(read_only_hint = true)
    )]
    async fn get_thread(&self, Parameters(a): Parameters<ThreadArgs>) -> ToolResult {
        let out = tri!(
            self.with(move |s, _| {
                let row = s.get(a.id)?;
                let f = Filter {
                    thread: Some(row.thread),
                    oldest_first: true,
                    limit: 200,
                    ..Default::default()
                };
                let rows = s.list(&f)?;
                let mut seen = HashSet::new();
                let mut out = Vec::new();
                for r in rows {
                    let body = load_parsed(s, r.id, |_, m| {
                        Ok((m.message_id().map(String::from), emlx::body_text(m)))
                    });
                    let (mid, text) = match body {
                        Ok((mid, t)) => (mid, t),
                        Err(_) => (None, r.preview.clone().unwrap_or_default()),
                    };
                    if let Some(m) = &mid
                        && !seen.insert(m.clone()) {
                            continue;
                        }
                    let mut v = serde_json::to_value(&r)?;
                    v["body"] = json!(truncate_chars(&text, a.max_chars_per_message));
                    if let Some(o) = v.as_object_mut() {
                        o.remove("preview");
                    }
                    out.push(v);
                }
                Ok(json!({ "thread": row.thread, "messages": out }))
            })
            .await
        );
        ok(&out)
    }

    #[tool(
        description = "Raw RFC 822 source of a message (or just its headers), for header forensics, DKIM/SPF checks or MIME debugging.",
        annotations(read_only_hint = true)
    )]
    async fn get_source(&self, Parameters(a): Parameters<SourceArgs>) -> ToolResult {
        let out = tri!(
            self.with(move |s, _| {
                let row = s.get(a.id)?;
                let path = s
                    .emlx_path(a.id, row.mailbox_id)
                    .ok_or_else(|| anyhow!("no local copy of message {}", a.id))?;
                let e = emlx::read(&path)?;
                let text = String::from_utf8_lossy(&e.raw).to_string();
                let text = if a.headers_only {
                    let end = text.find("\r\n\r\n").or_else(|| text.find("\n\n")).unwrap_or(text.len());
                    text[..end].to_string()
                } else {
                    text
                };
                let size = text.len();
                let mut cut = a.max_bytes.min(size);
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                Ok(json!({
                    "id": a.id,
                    "bytes": size,
                    "truncated": cut < size,
                    "partial_emlx": e.partial,
                    "source": &text[..cut],
                }))
            })
            .await
        );
        ok(&out)
    }

    #[tool(
        description = "Save one or all attachments of a message to disk (default ~/Downloads). Never overwrites existing files."
    )]
    async fn save_attachments(&self, Parameters(a): Parameters<SaveAttArgs>) -> ToolResult {
        let dir = expand_home(a.directory.as_deref().unwrap_or("~/Downloads"));
        tri!(std::fs::create_dir_all(&dir));
        let d2 = dir.clone();
        let res = self
            .with(move |s, _| {
                load_parsed(s, a.id, |e, m| {
                    let count = m.attachments().count();
                    if count == 0 {
                        bail!("message {} has no attachments", a.id);
                    }
                    let which: Vec<usize> = match a.index {
                        Some(i) => vec![i],
                        None => (0..count).collect(),
                    };
                    let mut saved = Vec::new();
                    for i in which {
                        let (name, data) = emlx::attachment_bytes(e, m, i)?;
                        let p = unique_path(&d2, &name);
                        std::fs::write(&p, data)?;
                        saved.push(p.display().to_string());
                    }
                    Ok(saved)
                })
            })
            .await;
        match res {
            Ok(saved) => ok(&json!({ "saved": saved })),
            Err(e) if a.index.is_some() && format!("{e}").contains("not downloaded") => {
                let (r, _) = tri!(self.msg_ref(a.id).await);
                let idx = a.index.unwrap_or(0);
                let name = format!("attachment-{}-{}", a.id, idx);
                let p = unique_path(&dir, &name);
                tri!(script::save_attachment(&r, idx, &p.display().to_string()).await);
                ok(&json!({ "saved": [p.display().to_string()], "via": "Mail" }))
            }
            Err(e) => fail(e),
        }
    }

    #[tool(
        description = "Export messages to files: eml (original source), md (readable) or json. Select by ids or by query/mailbox/since."
    )]
    async fn export_messages(&self, Parameters(a): Parameters<ExportArgs>) -> ToolResult {
        let dir = expand_home(&a.directory);
        tri!(std::fs::create_dir_all(&dir));
        let out = tri!(
            self.with(move |s, index| {
                let mut f = base_filter(s, a.mailbox.as_deref(), None, a.since.as_deref(), None)?;
                if let Some(ids) = a.ids.clone() {
                    f.ids = Some(ids);
                } else if let Some(q) = a.query.as_deref() {
                    let mut idx = index.lock().map_err(|_| anyhow!("index lock poisoned"))?;
                    idx.sync(s)?;
                    f.ids = Some(idx.search(q, false, false, a.limit)?.into_iter().map(|h| h.id).collect());
                } else if f.mailbox_ids.is_none() && f.since.is_none() {
                    bail!("give ids, query, mailbox or since");
                }
                f.limit = a.limit.clamp(1, 5000);
                let rows = s.list(&f)?;
                let format = a.format.clone().unwrap_or_else(|| "eml".into());
                let mut files = Vec::new();
                let mut json_out = Vec::new();
                for r in &rows {
                    let Some(path) = s.emlx_path(r.id, r.mailbox_id) else { continue };
                    let e = emlx::read(&path)?;
                    let base = emlx::sanitize_filename(&format!(
                        "{} {}",
                        r.date.get(..10).unwrap_or(""),
                        truncate_chars(&r.subject, 60).trim_end_matches('…')
                    ));
                    match format.as_str() {
                        "eml" => {
                            let p = unique_path(&dir, &format!("{base}.eml"));
                            std::fs::write(&p, &e.raw)?;
                            files.push(p.display().to_string());
                        }
                        "md" | "json" => {
                            let body = emlx::parse(&e.raw).map(|m| emlx::body_text(&m)).unwrap_or_default();
                            if format == "md" {
                                let md = format!(
                                    "# {}\n\n- From: {}\n- Date: {}\n- Mailbox: {} / {}\n\n{}\n",
                                    r.subject, r.from, r.date, r.account, r.mailbox, body
                                );
                                let p = unique_path(&dir, &format!("{base}.md"));
                                std::fs::write(&p, md)?;
                                files.push(p.display().to_string());
                            } else {
                                let mut v = serde_json::to_value(r)?;
                                v["body"] = json!(body);
                                json_out.push(v);
                            }
                        }
                        other => bail!("unknown format '{other}' (eml, md, json)"),
                    }
                }
                if format == "json" {
                    let p = unique_path(&dir, "mail-export.json");
                    std::fs::write(&p, serde_json::to_vec_pretty(&json_out)?)?;
                    files.push(p.display().to_string());
                }
                Ok(json!({ "exported": rows.len(), "files": files.len(), "directory": dir.display().to_string(), "paths": files.into_iter().take(20).collect::<Vec<_>>() }))
            })
            .await
        );
        ok(&out)
    }

    #[tool(
        description = "Mail statistics for a time window: volume per mailbox and per day, unread, top senders and domains, attachments, sent count.",
        annotations(read_only_hint = true)
    )]
    async fn stats(&self, Parameters(a): Parameters<StatsArgs>) -> ToolResult {
        let out = tri!(
            self.with(move |s, _| {
                let since = parse_when(a.since.as_deref().unwrap_or("30d"))?;
                let mut f = base_filter(s, None, a.account.as_deref(), None, None)?;
                f.since = Some(since);
                f.exclude_mailbox_ids = s.role_ids(&[Role::Trash, Role::Junk]);
                let rows = s.list(&f)?;
                let sent_ids: HashSet<i64> = s.role_ids(&[Role::Sent]).into_iter().collect();
                let mut by_mailbox: HashMap<String, (i64, i64)> = HashMap::new();
                let mut by_day: std::collections::BTreeMap<String, i64> = Default::default();
                let mut senders: HashMap<String, i64> = HashMap::new();
                let mut domains: HashMap<String, i64> = HashMap::new();
                let (mut received, mut sent, mut unread, mut with_att, mut flagged) = (0, 0, 0, 0, 0);
                for r in &rows {
                    let e = by_mailbox.entry(format!("{} / {}", r.account, r.mailbox)).or_default();
                    e.0 += 1;
                    if !r.read {
                        e.1 += 1;
                    }
                    if sent_ids.contains(&r.mailbox_id) {
                        sent += 1;
                        continue;
                    }
                    received += 1;
                    if !r.read {
                        unread += 1;
                    }
                    if r.flagged {
                        flagged += 1;
                    }
                    if r.attachments > 0 {
                        with_att += 1;
                    }
                    *by_day.entry(r.date.get(..10).unwrap_or("").to_string()).or_default() += 1;
                    *senders.entry(r.from.clone()).or_default() += 1;
                    if let Some((_, d)) = r.sender_address.rsplit_once('@') {
                        *domains.entry(d.to_string()).or_default() += 1;
                    }
                }
                let top = |m: HashMap<String, i64>| {
                    let mut v: Vec<_> = m.into_iter().collect();
                    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                    v.truncate(a.top);
                    v.into_iter().map(|(k, n)| json!({ "name": k, "count": n })).collect::<Vec<_>>()
                };
                let mut mbs: Vec<_> = by_mailbox
                    .into_iter()
                    .map(|(k, (t, u))| json!({ "mailbox": k, "messages": t, "unread": u }))
                    .collect();
                mbs.sort_by_key(|v| -v["messages"].as_i64().unwrap_or(0));
                Ok(json!({
                    "since": fmt_ts(since),
                    "received": received,
                    "sent": sent,
                    "unread": unread,
                    "flagged": flagged,
                    "with_attachments": with_att,
                    "per_mailbox": mbs,
                    "per_day": by_day,
                    "top_senders": top(senders),
                    "top_domains": top(domains),
                }))
            })
            .await
        );
        ok(&out)
    }

    #[tool(
        description = "One-call inbox dashboard: unread per account, latest inbox messages, flagged messages and how many people wait for your reply.",
        annotations(read_only_hint = true)
    )]
    async fn overview(&self) -> ToolResult {
        let me = self.me().await;
        let out = tri!(
            self.with(move |s, _| {
                s.refresh_mailboxes()?;
                let inbox = s.role_ids(&[Role::Inbox]);
                let latest = s.list(&Filter { mailbox_ids: Some(inbox.clone()), limit: 10, ..Default::default() })?;
                let unread = s.list(&Filter { mailbox_ids: Some(inbox.clone()), unread: Some(true), ..Default::default() })?;
                let mut flagged_f = Filter { flagged: Some(true), limit: 10, ..Default::default() };
                flagged_f.exclude_mailbox_ids = s.role_ids(&[Role::Trash, Role::Junk]);
                let flagged = s.list(&flagged_f)?;
                let needs = needs_reply(s, &me, now() - 14 * 86_400, 50)?;
                let accounts: Vec<_> = s
                    .accounts()
                    .into_iter()
                    .map(|a| json!({ "account": a.name, "unread": a.unread, "messages": a.messages }))
                    .collect();
                Ok(json!({
                    "accounts": accounts,
                    "inbox_unread": unread.len(),
                    "latest": latest,
                    "flagged": flagged,
                    "needs_reply_14d": needs.len(),
                    "needs_reply_top": needs.into_iter().take(5).collect::<Vec<_>>(),
                }))
            })
            .await
        );
        ok(&out)
    }

    #[tool(
        description = "Triage: 'needs_reply' lists mail from real people you have not answered; 'awaiting_reply' lists mail you sent that got no answer. Automated senders and newsletters are skipped.",
        annotations(read_only_hint = true)
    )]
    async fn triage(&self, Parameters(a): Parameters<TriageArgs>) -> ToolResult {
        let me = self.me().await;
        let out = tri!(
            self.with(move |s, _| {
                let since = parse_when(a.since.as_deref().unwrap_or("14d"))?;
                let limit = a.limit.clamp(1, 200);
                match a.mode.as_str() {
                    "needs_reply" => Ok(json!(needs_reply(s, &me, since, limit)?)),
                    "awaiting_reply" => Ok(json!(awaiting_reply(s, &me, since, limit)?)),
                    m => bail!("unknown mode '{m}' (needs_reply, awaiting_reply)"),
                }
            })
            .await
        );
        ok(&out)
    }

    #[tool(
        description = "Status of the local full-text index (documents, size, messages without downloaded body). rebuild=true recreates it.",
        annotations(read_only_hint = true)
    )]
    async fn index_status(&self, Parameters(a): Parameters<IndexArgs>) -> ToolResult {
        let out = tri!(
            self.with(move |s, index| {
                let mut idx = index.lock().map_err(|_| anyhow!("index lock poisoned"))?;
                let sync = if a.rebuild { idx.rebuild(s)? } else { idx.sync(s)? };
                Ok(json!({ "index": idx.status()?, "last_sync": sync }))
            })
            .await
        );
        ok(&out)
    }

    #[tool(
        description = "Write a new email. Mail opens its own compose window (your signature included), the text is pasted in, then it is saved as a draft or, with send=true, sent. Mail comes to the front for about two seconds. body is plain text; html_body is optional rich text.",
        annotations(destructive_hint = false, open_world_hint = true)
    )]
    async fn compose(&self, Parameters(a): Parameters<ComposeArgs>) -> ToolResult {
        if a.to.is_empty() && a.cc.is_empty() && a.bcc.is_empty() {
            return fail(anyhow!("no recipients"));
        }
        let attachments = tri!(check_attachments(&a.attachments));
        let o = Outgoing {
            subject: a.subject.clone(),
            body: a.body.clone(),
            html: a.html_body.clone(),
            from: a.from.clone().unwrap_or_default(),
            to: a.to.clone(),
            cc: a.cc.clone(),
            bcc: a.bcc.clone(),
            attachments,
            send: a.send,
        };
        let (status, subject) = tri!(script::compose_window(Kind::New, None, &o).await);
        let draft_id = if status == "draft" { self.find_draft(&subject).await } else { None };
        ok(&json!({ "status": status, "subject": subject, "to": a.to, "draft_id": draft_id }))
    }

    #[tool(
        description = "Reply to a message by id (reply_all optional) exactly like Mail's Reply button: Mail quotes the original and adds your signature, your text goes on top. Saves a draft by default; send=true sends.",
        annotations(destructive_hint = false, open_world_hint = true)
    )]
    async fn reply(&self, Parameters(a): Parameters<ReplyArgs>) -> ToolResult {
        let (r, row) = tri!(self.msg_ref(a.id).await);
        let attachments = tri!(check_attachments(&a.attachments));
        let o = Outgoing {
            subject: String::new(),
            body: format!("{}\n", a.body),
            html: a.html_body.clone(),
            from: a.from.clone().unwrap_or_default(),
            to: Vec::new(),
            cc: a.cc.clone(),
            bcc: a.bcc.clone(),
            attachments,
            send: a.send,
        };
        let kind = if a.reply_all { Kind::ReplyAll } else { Kind::Reply };
        let (status, subject) = tri!(script::compose_window(kind, Some(&r), &o).await);
        let draft_id = if status == "draft" { self.find_draft(&subject).await } else { None };
        ok(&json!({ "status": status, "in_reply_to": a.id, "to": row.from, "subject": subject, "draft_id": draft_id }))
    }

    #[tool(
        description = "Forward a message by id exactly like Mail's Forward button, with the original and its attachments. Saves a draft by default; send=true sends.",
        annotations(destructive_hint = false, open_world_hint = true)
    )]
    async fn forward(&self, Parameters(a): Parameters<ForwardArgs>) -> ToolResult {
        if a.to.is_empty() {
            return fail(anyhow!("no recipients"));
        }
        let (r, _) = tri!(self.msg_ref(a.id).await);
        let o = Outgoing {
            subject: String::new(),
            body: if a.body.is_empty() { String::new() } else { format!("{}\n", a.body) },
            html: None,
            from: a.from.clone().unwrap_or_default(),
            to: a.to.clone(),
            cc: a.cc.clone(),
            bcc: a.bcc.clone(),
            attachments: Vec::new(),
            send: a.send,
        };
        let (status, subject) = tri!(script::compose_window(Kind::Forward, Some(&r), &o).await);
        let draft_id = if status == "draft" { self.find_draft(&subject).await } else { None };
        ok(&json!({ "status": status, "forwarded": a.id, "to": a.to, "subject": subject, "draft_id": draft_id }))
    }

    #[tool(
        description = "Change messages in Mail: mark_read, mark_unread, flag, unflag, flag_color (with color), move (with mailbox), trash, junk, not_junk. Takes a list of ids.",
        annotations(destructive_hint = true)
    )]
    async fn update_messages(&self, Parameters(a): Parameters<UpdateArgs>) -> ToolResult {
        if a.ids.is_empty() {
            return fail(anyhow!("no ids"));
        }
        let mut refs = Vec::new();
        let mut first_account = String::new();
        for id in &a.ids {
            let (r, row) = tri!(self.msg_ref(*id).await);
            if first_account.is_empty() {
                first_account = row.account.clone();
            }
            refs.push(r);
        }
        let res = match a.action.as_str() {
            "mark_read" => script::set_status(&refs, "read", "true").await,
            "mark_unread" => script::set_status(&refs, "read", "false").await,
            "flag" => script::set_status(&refs, "flagged", "true").await,
            "unflag" => script::set_status(&refs, "flagged", "false").await,
            "junk" => script::set_status(&refs, "junk", "true").await,
            "not_junk" => script::set_status(&refs, "junk", "false").await,
            "flag_color" => {
                let idx = match a.color.as_deref().unwrap_or("red") {
                    "red" => 0,
                    "orange" => 1,
                    "yellow" => 2,
                    "green" => 3,
                    "blue" => 4,
                    "purple" => 5,
                    "gray" | "grey" => 6,
                    c => return fail(anyhow!("unknown color '{c}'")),
                };
                script::set_status(&refs, "flag_color", &idx.to_string()).await
            }
            "trash" => script::trash(&refs).await,
            "move" => {
                let Some(target) = a.mailbox.clone() else {
                    return fail(anyhow!("move needs a mailbox"));
                };
                let account = a.account.clone().unwrap_or(first_account);
                let dest = tri!(
                    self.with(move |s, _| {
                        let ids = s.resolve_mailboxes(&target, Some(&account))?;
                        let mbs: Vec<_> = ids.iter().filter_map(|i| s.mailbox(*i)).collect();
                        let mb = mbs
                            .iter()
                            .max_by_key(|m| m.total)
                            .ok_or_else(|| anyhow!("no such mailbox"))?;
                        Ok((if mb.local { String::new() } else { mb.account.clone() }, mb.path.clone()))
                    })
                    .await
                );
                script::move_to(&refs, &dest.0, &dest.1).await.map(|r| format!("{r}\n→ {}", dest.1))
            }
            other => return fail(anyhow!("unknown action '{other}'")),
        };
        let out = tri!(res);
        let mut lines = out.lines();
        let done: usize = lines.next().and_then(|n| n.trim().parse().ok()).unwrap_or(0);
        let errors: Vec<&str> = lines.filter(|l| !l.trim().is_empty() && !l.starts_with('→')).collect();
        ok(&json!({ "action": a.action, "changed": done, "requested": a.ids.len(), "errors": errors }))
    }

    #[tool(
        description = "Drafts: action=list shows drafts of every account; action=send sends a saved draft (re-created from its content, then the draft is trashed); action=trash discards one.",
        annotations(destructive_hint = true, open_world_hint = true)
    )]
    async fn drafts(&self, Parameters(a): Parameters<DraftArgs>) -> ToolResult {
        match a.action.as_str() {
            "list" => {
                let rows = tri!(
                    self.with(|s, _| {
                        let ids = s.role_ids(&[Role::Drafts]);
                        s.list(&Filter { mailbox_ids: Some(ids), limit: 100, ..Default::default() })
                    })
                    .await
                );
                ok(&rows)
            }
            "trash" => {
                let Some(id) = a.id else { return fail(anyhow!("trash needs an id")) };
                let (r, _) = tri!(self.msg_ref(id).await);
                tri!(script::trash(&[r]).await);
                ok(&json!({ "trashed": id }))
            }
            "send" => {
                let Some(id) = a.id else { return fail(anyhow!("send needs an id")) };
                let tmp = std::env::temp_dir().join(format!("mailidx-draft-{id}"));
                let t2 = tmp.clone();
                let o = tri!(
                    self.with(move |s, _| {
                        let row = s.get(id)?;
                        if s.mailbox(row.mailbox_id).map(|m| m.role) != Some(Role::Drafts) {
                            bail!("message {id} is not in a drafts mailbox");
                        }
                        let recipients = s.recipients(id)?;
                        load_parsed(s, id, |e, m| {
                            std::fs::create_dir_all(&t2)?;
                            let mut atts = Vec::new();
                            for i in 0..m.attachments().count() {
                                let (name, data) = emlx::attachment_bytes(e, m, i)?;
                                let p = unique_path(&t2, &name);
                                std::fs::write(&p, data)?;
                                atts.push(p.display().to_string());
                            }
                            let pick = |k: &str| {
                                recipients
                                    .iter()
                                    .filter(|r| r.kind == k)
                                    .map(|r| format_addr(&r.address, &r.name))
                                    .collect::<Vec<_>>()
                            };
                            Ok(Outgoing {
                                html: None,
                                subject: m.subject().unwrap_or("").to_string(),
                                body: emlx::body_text(m),
                                from: emlx::addresses(m.from()).into_iter().next().unwrap_or_default(),
                                to: pick("to"),
                                cc: pick("cc"),
                                bcc: pick("bcc"),
                                attachments: atts,
                                send: true,
                            })
                        })
                    })
                    .await
                );
                if o.to.is_empty() && o.cc.is_empty() && o.bcc.is_empty() {
                    return fail(anyhow!("draft has no recipients"));
                }
                let status = script::compose_window(Kind::New, None, &o).await;
                let _ = std::fs::remove_dir_all(&tmp);
                tri!(status);
                let (r, _) = tri!(self.msg_ref(id).await);
                let _ = script::trash(&[r]).await;
                ok(&json!({ "sent": id, "subject": o.subject, "to": o.to }))
            }
            other => fail(anyhow!("unknown action '{other}' (list, send, trash)")),
        }
    }

    #[tool(description = "Create a new mailbox (folder) in an account. Use 'Parent/Child' for nesting.")]
    async fn create_mailbox(&self, Parameters(a): Parameters<MailboxArgs>) -> ToolResult {
        let account = match a.account.clone() {
            Some(x) => x,
            None => {
                let accs = tri!(self.with(|s, _| Ok(s.accounts())).await);
                match accs.iter().find(|x| x.name != "On My Mac" && x.name != "Auf meinem Mac") {
                    Some(x) => x.name.clone(),
                    None => String::new(),
                }
            }
        };
        tri!(script::create_mailbox(&account, &a.name).await);
        ok(&json!({ "created": a.name, "account": account }))
    }

    #[tool(description = "Ask Mail to fetch new mail and sync an account (or all accounts).")]
    async fn check_mail(&self, Parameters(a): Parameters<AccountArg>) -> ToolResult {
        tri!(script::check_mail(a.account.as_deref().unwrap_or("")).await);
        ok(&json!({ "status": "requested", "account": a.account }))
    }

    #[tool(description = "Open a message in a Mail window so the user can see it.")]
    async fn open_in_mail(&self, Parameters(a): Parameters<IdArg>) -> ToolResult {
        let (r, _) = tri!(self.msg_ref(a.id).await);
        tri!(script::open_in_mail(&r).await);
        ok(&json!({ "opened": a.id }))
    }
}

impl Server {
    async fn find_draft(&self, subject: &str) -> Option<i64> {
        for _ in 0..12 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let subj = subject.to_string();
            let found = self
                .with(move |s, _| {
                    let ids = s.role_ids(&[Role::Drafts]);
                    let rows = s.list(&Filter {
                        mailbox_ids: Some(ids),
                        since: Some(now() - 120),
                        limit: 20,
                        ..Default::default()
                    })?;
                    Ok(rows.into_iter().find(|r| r.subject == subj).map(|r| r.id))
                })
                .await
                .ok()
                .flatten();
            if found.is_some() {
                return found;
            }
        }
        None
    }
}

fn is_automated(addr: &str) -> bool {
    AUTOMATED_SENDERS.iter().any(|p| addr.contains(p))
}

fn automated_flags(s: &Store, ids: &[i64]) -> Result<HashSet<i64>> {
    if ids.is_empty() {
        return Ok(HashSet::new());
    }
    let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let mut st = s.conn().prepare(&format!(
        "SELECT ROWID FROM messages WHERE ROWID IN ({list}) AND COALESCE(automated_conversation,0) != 0"
    ))?;
    let rows = st.query_map([], |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn needs_reply(s: &Store, me: &HashSet<String>, since: i64, limit: usize) -> Result<Vec<MessageRow>> {
    let inbox = s.role_ids(&[Role::Inbox, Role::Archive, Role::Other]);
    let rows = s.list(&Filter {
        mailbox_ids: Some(inbox),
        since: Some(since),
        answered: Some(false),
        ..Default::default()
    })?;
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let automated = automated_flags(s, &ids)?;
    let mut latest_mine: HashMap<i64, i64> = HashMap::new();
    let mine = s.list(&Filter { mailbox_ids: Some(s.role_ids(&[Role::Sent])), since: Some(since), ..Default::default() })?;
    for r in mine {
        let e = latest_mine.entry(r.thread).or_insert(0);
        *e = (*e).max(r.ts);
    }
    let mut seen_threads = HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        if me.contains(&r.sender_address)
            || automated.contains(&r.id)
            || is_automated(&r.sender_address)
            || latest_mine.get(&r.thread).is_some_and(|t| *t >= r.ts)
            || !seen_threads.insert(r.thread)
        {
            continue;
        }
        out.push(r);
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

fn awaiting_reply(s: &Store, me: &HashSet<String>, since: i64, limit: usize) -> Result<Vec<serde_json::Value>> {
    let sent = s.list(&Filter { mailbox_ids: Some(s.role_ids(&[Role::Sent])), since: Some(since), ..Default::default() })?;
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for r in sent {
        if !seen.insert(r.thread) {
            continue;
        }
        let thread = s.list(&Filter { thread: Some(r.thread), ..Default::default() })?;
        let answered = thread.iter().any(|m| m.ts > r.ts && !me.contains(&m.sender_address));
        if answered {
            continue;
        }
        let to: Vec<String> = s
            .recipients(r.id)?
            .into_iter()
            .filter(|x| x.kind == "to")
            .map(|x| x.address.to_lowercase())
            .collect();
        if to.is_empty() || to.iter().all(|a| me.contains(a)) {
            continue;
        }
        let days = (now() - r.ts) / 86_400;
        let mut v = serde_json::to_value(&r)?;
        v["to"] = json!(to);
        v["days_waiting"] = json!(days);
        if let Some(o) = v.as_object_mut() {
            o.remove("from");
        }
        out.push(v);
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

const INSTRUCTIONS: &str = "mailidx reads Apple Mail's own index and message files directly, so every read tool is instant and Mail.app does not need to do anything. \
Start with overview or search. Message ids are stable and work in every tool. \
Writing tools (compose, reply, forward, update_messages, drafts, create_mailbox, check_mail) go through Mail via AppleScript; compose/reply/forward save drafts unless send=true. \
Only set send=true when the user clearly asked to send. Permanent deletion is not available; trash moves to the account's trash.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        let mut info = Implementation::new("mailidx", env!("CARGO_PKG_VERSION"));
        info.title = Some("mailidx — fast Apple Mail MCP server".into());
        info.website_url = Some("https://github.com/jx-grxf/mailidx".into());
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(info)
            .with_instructions(INSTRUCTIONS)
    }
}
