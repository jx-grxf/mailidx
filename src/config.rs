use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Ask GitHub at most once a day whether a newer release exists.
    pub update_check: bool,
    /// Refuse every tool call that would send mail; drafts still work.
    pub send_locked: bool,
    /// Account names whose mail no tool may see or touch.
    pub hidden_accounts: Vec<String>,
    /// Mailbox paths ("Privat", "Business/Kunden") or "Account/Path"; children are hidden too.
    pub hidden_mailboxes: Vec<String>,
    /// Where the full-text index lives. MAILIDX_INDEX still wins.
    pub index_path: Option<String>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            update_check: true,
            send_locked: false,
            hidden_accounts: Vec::new(),
            hidden_mailboxes: Vec::new(),
            index_path: None,
        }
    }
}

pub const KEYS: &[(&str, &str)] = &[
    ("update_check", "true/false: check GitHub once a day for a newer release"),
    ("send_locked", "true/false: tools may only save drafts, never send or unsubscribe"),
    ("hidden_accounts", "comma-separated account names that no tool sees"),
    ("hidden_mailboxes", "comma-separated mailbox paths (or Account/Path) that no tool sees"),
    ("index_path", "path of the search index file (empty = default)"),
];

pub fn path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("MAILIDX_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var("HOME").context("HOME not set")?).join(".config"),
    };
    Ok(base.join("mailidx/config.toml"))
}

impl Config {
    pub fn load() -> Result<Config> {
        let p = path()?;
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("cannot read settings in {}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", p.display())),
        }
    }

    /// The server must keep running with a broken settings file, so it falls back
    /// to the defaults and says why on stderr.
    pub fn load_or_default() -> Config {
        Config::load().unwrap_or_else(|e| {
            eprintln!("mailidx: {e:#}; using default settings");
            Config::default()
        })
    }

    pub fn save(&self) -> Result<PathBuf> {
        let p = path()?;
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&p, toml::to_string_pretty(self)?)?;
        Ok(p)
    }

    pub fn get(&self, key: &str) -> Result<String> {
        Ok(match key {
            "update_check" => self.update_check.to_string(),
            "send_locked" => self.send_locked.to_string(),
            "hidden_accounts" => self.hidden_accounts.join(", "),
            "hidden_mailboxes" => self.hidden_mailboxes.join(", "),
            "index_path" => self.index_path.clone().unwrap_or_default(),
            _ => bail!("unknown setting '{key}'"),
        })
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let list = || {
            value.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>()
        };
        match key {
            "update_check" => self.update_check = parse_bool(value)?,
            "send_locked" => self.send_locked = parse_bool(value)?,
            "hidden_accounts" => self.hidden_accounts = list(),
            "hidden_mailboxes" => self.hidden_mailboxes = list(),
            "index_path" => {
                let v = value.trim();
                self.index_path = (!v.is_empty()).then(|| v.to_string());
            }
            _ => bail!("unknown setting '{key}'"),
        }
        Ok(())
    }

    pub fn reset(&mut self, key: &str) -> Result<()> {
        let d = Config::default();
        match key {
            "update_check" => self.update_check = d.update_check,
            "send_locked" => self.send_locked = d.send_locked,
            "hidden_accounts" => self.hidden_accounts = d.hidden_accounts,
            "hidden_mailboxes" => self.hidden_mailboxes = d.hidden_mailboxes,
            "index_path" => self.index_path = d.index_path,
            _ => bail!("unknown setting '{key}'"),
        }
        Ok(())
    }

    pub fn hides_account(&self, account: &str) -> bool {
        let account = fold(account);
        self.hidden_accounts.iter().any(|a| fold(a) == account)
    }

    pub fn hides_mailbox(&self, account: &str, path: &str) -> bool {
        if self.hides_account(account) {
            return true;
        }
        let (account, path) = (fold(account), fold(path));
        let under = |rule: &str| path == rule || path.strip_prefix(rule).is_some_and(|r| r.starts_with('/'));
        self.hidden_mailboxes.iter().any(|rule| {
            let rule = fold(rule.trim_matches('/'));
            under(&rule) || rule.split_once('/').is_some_and(|(acc, rest)| acc == account && under(rest))
        })
    }
}

/// Terminals and Mail may hand over the same name in NFC or NFD.
fn fold(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfc().collect::<String>().to_lowercase()
}

fn parse_bool(v: &str) -> Result<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        other => Err(anyhow!("'{other}' is not true or false")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_mailboxes_with_children_and_account_prefix() {
        let c = Config {
            hidden_accounts: vec!["Work".into()],
            hidden_mailboxes: vec!["Privat".into(), "iCloud/Business/Kunden".into()],
            ..Config::default()
        };
        assert!(c.hides_mailbox("iCloud", "Privat"));
        assert!(c.hides_mailbox("iCloud", "privat/Familie"));
        assert!(!c.hides_mailbox("iCloud", "Privatsachen"));
        assert!(c.hides_mailbox("iCloud", "Business/Kunden/2026"));
        assert!(!c.hides_mailbox("Gmail", "Business/Kunden"));
        assert!(c.hides_mailbox("work", "INBOX"));
        let umlaut = Config { hidden_mailboxes: vec!["O\u{308}ffiGo".into()], ..Config::default() };
        assert!(umlaut.hides_mailbox("iCloud", "öffigo"));
    }

    #[test]
    fn set_parses_values_and_rejects_unknown_keys() {
        let mut c = Config::default();
        c.set("send_locked", "on").unwrap();
        c.set("hidden_mailboxes", " Privat , Business/Kunden ,").unwrap();
        assert!(c.send_locked);
        assert_eq!(c.hidden_mailboxes, ["Privat", "Business/Kunden"]);
        assert!(c.set("nope", "1").is_err());
        assert!(c.set("update_check", "maybe").is_err());
    }

    #[test]
    fn round_trips_through_toml() {
        let c = Config { send_locked: true, index_path: Some("/tmp/i.sqlite".into()), ..Config::default() };
        let back: Config = toml::from_str(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert!(back.send_locked && back.update_check);
        assert_eq!(back.index_path.as_deref(), Some("/tmp/i.sqlite"));
    }
}
