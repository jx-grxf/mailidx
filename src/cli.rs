use std::io::IsTerminal;

use anyhow::{Result, bail};
use serde_json::json;

use crate::config::{self, Config, KEYS};
use crate::{fts, store, update};

struct Paint(bool);

impl Paint {
    fn new() -> Paint {
        Paint(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
    }
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.0 { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }
    fn ok(&self, s: &str) -> String {
        self.wrap("32", s)
    }
    fn warn(&self, s: &str) -> String {
        self.wrap("33", s)
    }
    fn bad(&self, s: &str) -> String {
        self.wrap("31", s)
    }
    fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }
    fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
}

fn ago(ts: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(ts);
    match (now - ts).max(0) {
        s if s < 90 => "just now".into(),
        s if s < 5400 => format!("{} min ago", s / 60),
        s if s < 172_800 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

fn home_short(p: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if p.starts_with(&h) => format!("~{}", &p[h.len()..]),
        _ => p.to_string(),
    }
}

pub fn status(as_json: bool) -> Result<()> {
    let cfg = Config::load();
    let cfg_ok = cfg.as_ref().map(|c| c.clone()).unwrap_or_default();
    let upd = if cfg_ok.update_check { update::check(false).map_err(|e| format!("{e:#}")) } else { Err("off".into()) };
    let st = store::Store::open();
    let idx = fts::Index::open().and_then(|i| i.status());
    let bin = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();

    if as_json {
        let out = json!({
            "version": update::CURRENT,
            "binary": bin,
            "update": match &upd {
                Ok(u) => json!(u),
                Err(e) => json!({ "error": e }),
            },
            "mail": match &st {
                Ok(s) => json!({
                    "root": s.mail_root.display().to_string(),
                    "accounts": s.accounts().iter().map(|a| json!({"name": a.name, "messages": a.messages, "unread": a.unread})).collect::<Vec<_>>(),
                    "mailboxes": s.mailboxes().len(),
                    "hidden_mailboxes": s.hidden_mailbox_count(),
                }),
                Err(e) => json!({ "error": format!("{e:#}") }),
            },
            "index": match &idx {
                Ok(i) => json!(i),
                Err(e) => json!({ "error": format!("{e:#}") }),
            },
            "config": {
                "path": config::path()?.display().to_string(),
                "settings": cfg_ok,
                "error": cfg.as_ref().err().map(|e| format!("{e:#}")),
            },
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let p = Paint::new();
    let row = |label: &str, value: String| println!("  {:<13}{value}", p.dim(label));
    println!("{}", p.bold(&format!("mailidx {}", update::CURRENT)));
    row(
        "Update",
        match &upd {
            Ok(u) if u.available => p.warn(&format!("{} available, run `mailidx update`", u.latest)),
            Ok(u) => format!("{} {}", p.ok("up to date"), p.dim(&format!("(checked {})", ago(u.checked_at)))),
            Err(e) if e == "off" => p.dim("check is off (update_check = false)"),
            Err(e) => p.bad(&format!("check failed: {e}")),
        },
    );
    row("Binary", home_short(&bin));
    match &st {
        Ok(s) => {
            let hidden = s.hidden_mailbox_count();
            let total: i64 = s.accounts().iter().map(|a| a.messages).sum();
            row(
                "Mail",
                format!(
                    "{} messages in {} mailboxes{}  {}",
                    total,
                    s.mailboxes().len(),
                    if hidden > 0 { format!(", {hidden} hidden") } else { String::new() },
                    p.dim(&home_short(&s.mail_root.display().to_string()))
                ),
            );
            for a in s.accounts().iter().filter(|a| a.messages > 0) {
                row("", format!("{}  {} messages, {} unread", a.name, a.messages, a.unread));
            }
        }
        Err(e) => row("Mail", p.bad(&format!("{e:#}"))),
    }
    match &idx {
        Ok(i) => row(
            "Search index",
            format!(
                "{} documents, {}{}  {}",
                i.documents,
                mb(i.size_bytes),
                if i.without_body > 0 { format!(", {} without body", i.without_body) } else { String::new() },
                p.dim(&home_short(&i.path))
            ),
        ),
        Err(e) => row("Search index", p.bad(&format!("{e:#}"))),
    }
    let cfg_path = home_short(&config::path()?.display().to_string());
    match &cfg {
        Ok(_) => row("Settings", p.dim(&cfg_path)),
        Err(e) => row("Settings", p.bad(&format!("{e:#}"))),
    }
    let list = |v: &[String]| if v.is_empty() { p.dim("none") } else { v.join(", ") };
    row("", format!("update_check     {}", cfg_ok.update_check));
    row(
        "",
        format!(
            "send_locked      {}",
            if cfg_ok.send_locked { p.warn("true (drafts only)") } else { "false".into() }
        ),
    );
    row("", format!("hidden_accounts  {}", list(&cfg_ok.hidden_accounts)));
    row("", format!("hidden_mailboxes {}", list(&cfg_ok.hidden_mailboxes)));
    row("", format!("index_path       {}", cfg_ok.index_path.clone().unwrap_or_else(|| p.dim("default"))));
    Ok(())
}

pub fn update(check_only: bool) -> Result<()> {
    let p = Paint::new();
    let u = update::check(true)?;
    if !u.available {
        println!("{} mailidx {} is the latest release.", p.ok("✓"), u.current);
        return Ok(());
    }
    println!("mailidx {} is available (installed: {}).", p.bold(&u.latest), u.current);
    println!("  {}", p.dim(&u.release_url));
    if check_only {
        println!("Run `mailidx update` to install it.");
        return Ok(());
    }
    let path = update::install(&u.latest)?;
    println!("{} Installed mailidx {} to {}", p.ok("✓"), u.latest, home_short(&path.display().to_string()));
    println!("Reconnect mailidx in your MCP client to load it (Claude Code: /mcp → mail → Reconnect).");
    Ok(())
}

pub fn config(args: &[String]) -> Result<()> {
    let p = Paint::new();
    let restart = || println!("{}", p.dim("Reconnect mailidx in your MCP client to apply it."));
    match args.first().map(String::as_str) {
        None | Some("show") | Some("list") => {
            let c = Config::load()?;
            println!("{}", p.dim(&home_short(&config::path()?.display().to_string())));
            for (k, help) in KEYS {
                let v = c.get(k)?;
                println!("  {:<17}{:<24} {}", k, if v.is_empty() { "-".to_string() } else { v }, p.dim(help));
            }
        }
        Some("path") => println!("{}", config::path()?.display()),
        Some("get") => {
            let Some(k) = args.get(1) else { bail!("usage: mailidx config get <key>") };
            println!("{}", Config::load()?.get(k)?);
        }
        Some("set") => {
            let (Some(k), true) = (args.get(1), args.len() > 2) else {
                bail!("usage: mailidx config set <key> <value>")
            };
            let mut c = Config::load()?;
            c.set(k, &args[2..].join(" "))?;
            c.save()?;
            println!("{} {k} = {}", p.ok("✓"), c.get(k)?);
            restart();
        }
        Some("unset" | "reset") => {
            let Some(k) = args.get(1) else { bail!("usage: mailidx config unset <key>") };
            let mut c = Config::load()?;
            c.reset(k)?;
            c.save()?;
            println!("{} {k} = {}", p.ok("✓"), c.get(k)?);
            restart();
        }
        Some(other) => bail!("unknown config command '{other}' (show, get, set, unset, path)"),
    }
    Ok(())
}
