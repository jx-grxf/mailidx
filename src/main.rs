mod cli;
mod config;
mod emlx;
mod extract;
mod fts;
mod ical;
mod script;
mod signature;
mod store;
mod time;
mod tools;
mod update;

use std::time::Instant;

use anyhow::Result;
use rmcp::ServiceExt;

const HELP: &str = "mailidx — fast Apple Mail MCP server

Usage:
  mailidx                 run the MCP server on stdio (what MCP clients call)
  mailidx index           update the full-text index and print stats
  mailidx index --rebuild rebuild the index from scratch
  mailidx search <query>  search from the terminal (JSON lines)
  mailidx status [--json] version, update, Mail and index state, settings
  mailidx update [--check] install the latest release (--check only looks)
  mailidx config          show settings; also: get <key>, set <key> <value>, unset <key>, path
  mailidx doctor          check permissions and paths
  mailidx --version
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("serve") => serve().await,
        Some("index") => index(args.iter().any(|a| a == "--rebuild")),
        Some("search") => search(&args[1..].join(" ")),
        Some("doctor") => doctor().await,
        Some("status") => cli::status(args.iter().any(|a| a == "--json")),
        Some("update") => cli::update(args.iter().any(|a| a == "--check")),
        Some("config") => cli::config(&args[1..]),
        Some("-V" | "--version" | "version") => {
            println!("mailidx {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("-h" | "--help" | "help") => {
            print!("{HELP}");
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command '{other}'\n\n{HELP}");
            std::process::exit(2);
        }
    }
}

async fn serve() -> Result<()> {
    let index = fts::Index::open()?;
    let store = store::Store::open()?;
    let server = tools::Server::new(store, index);
    server.warm_up();
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

fn index(rebuild: bool) -> Result<()> {
    let mut idx = fts::Index::open()?;
    let mut store = store::Store::open()?;
    let stats = if rebuild { idx.rebuild(&mut store)? } else { idx.sync(&mut store)? };
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "sync": stats, "index": idx.status()? }))?);
    Ok(())
}

fn search(q: &str) -> Result<()> {
    let t0 = Instant::now();
    let mut idx = fts::Index::open()?;
    let mut store = store::Store::open()?;
    idx.sync(&mut store)?;
    let hits = idx.search(q, false, false, 50)?;
    let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
    let rows = store.list(&store::Filter { ids: Some(ids.clone()), ..Default::default() })?;
    for id in ids {
        if let Some(r) = rows.iter().find(|r| r.id == id) {
            println!("{}", serde_json::to_string(r)?);
        }
    }
    eprintln!("{} hits in {:.1} ms", hits.len(), t0.elapsed().as_secs_f64() * 1000.0);
    Ok(())
}

async fn doctor() -> Result<()> {
    let mut ok = true;
    match store::Store::open() {
        Ok(s) => {
            let (_, n) = s.fingerprint()?;
            println!("✓ Mail index: {} ({n} messages, {} mailboxes)", s.mail_root.display(), s.mailboxes().len());
        }
        Err(e) => {
            ok = false;
            println!("✗ Mail index: {e:#}");
            println!("  Give Full Disk Access to your terminal or MCP client: System Settings → Privacy & Security → Full Disk Access.");
        }
    }
    match fts::Index::open().and_then(|i| i.status()) {
        Ok(st) => println!("✓ Search index: {} ({} documents)", st.path, st.documents),
        Err(e) => {
            ok = false;
            println!("✗ Search index: {e:#}");
        }
    }
    match script::account_addresses().await {
        Ok(a) => println!("✓ Mail automation: {} account(s)", a.len()),
        Err(e) => {
            ok = false;
            println!("✗ Mail automation: {e:#}");
        }
    }
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}
