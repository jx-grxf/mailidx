<div align="center">

# mailidx

An Apple Mail MCP server for macOS. It lets Claude, Codex or any other MCP client search, read and write your Apple Mail, and it answers in milliseconds because it reads Mail's own index directly.

[![Release](https://img.shields.io/github/v/release/jx-grxf/mailidx?label=release)](https://github.com/jx-grxf/mailidx/releases/latest)
![Platform](https://img.shields.io/badge/platform-macOS-111111)
![Rust](https://img.shields.io/badge/rust-2024-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

[Install](#install) · [Settings](#settings) · [Tools](#tools) · [Benchmark](#benchmark) · [How it works](#how-it-works)

</div>

> [!TIP]
> Built for the moment you ask your agent "did the invoice from last week ever arrive?" and it has an answer before you finish reading the question.

## What it does

**Reading** never touches Mail.app

- Lists, searches and reads mail straight from Mail's SQLite "Envelope Index" and the `.emlx` files on disk. Mail can be busy, closed or syncing; reads still work.
- Full-text search over subject, sender, recipients, body and attachment names with a local SQLite FTS5 index. Prefix matching, `"exact phrases"`, `-exclusions`, and umlauts match with or without accents (`Muller` finds `Müller`).
- If no message contains every word, the search retries with any word and says so in the result.
- The index stays current on its own: FSEvents reports every change to Mail's index, and new mail is searchable about a second later.
- Whole threads across mailboxes, raw source for header checks, attachments saved to disk, export to `.eml`, Markdown or JSON.
- Apple Intelligence categories (primary, transactions, updates, promotions) and the "important" mark, as Mail assigned them on this Mac.
- `triage` lists mail from real people you have not answered, and mail you sent that got no reply.

**Writing** goes through Mail itself

- New mail, replies and forwards open Mail's own compose window, so your signature, the quoted original and forwarded attachments look exactly like they do when you click the button yourself.
- Everything is saved as a draft unless the caller passes `send: true`.
- Mark read or unread, flag with a color, move, junk, trash, create mailboxes, send or discard drafts, fetch new mail.
- There is no permanent delete. Trash only.

Your mail never leaves the Mac. The only network request is a check for a new mailidx release on GitHub, at most once a day, and you can turn it off.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/jx-grxf/mailidx/main/install.sh | sh
```

The script puts a universal binary (Apple silicon and Intel) into `~/.local/bin`, checks permissions with `mailidx doctor` and prints the command to register the server.

Register it with Claude Code:

```sh
claude mcp add --scope user mail -- ~/.local/bin/mailidx
```

Any other MCP client: run `~/.local/bin/mailidx` as a stdio server with no arguments.

### Permissions

- **Full Disk Access** for the app that starts the server (your terminal, Claude desktop, your editor): System Settings → Privacy & Security → Full Disk Access. Needed to read `~/Library/Mail`.
- **Automation → Mail**, asked for on the first write.
- **Accessibility** for the same app, used to paste text into Mail's compose window.

`mailidx doctor` shows which of these is missing.

### For agents installing mailidx

1. Run the install command above.
2. Run `~/.local/bin/mailidx doctor`. If Full Disk Access is missing, tell the user which app needs it and wait.
3. Register the server with the client you run in, using the absolute path to the binary.
4. Call the `overview` tool to confirm it works.

## Settings

Settings live in `~/.config/mailidx/config.toml`. Change them with `mailidx config set <key> <value>` and reconnect the server in your MCP client.

| Key | Default | What it does |
|---|---|---|
| `send_locked` | `false` | Tools can only save drafts. `send: true` and `drafts send` are refused. |
| `hidden_accounts` | none | Account names no tool can see, comma-separated. |
| `hidden_mailboxes` | none | Mailbox paths such as `Privat` or `iCloud/Business/Clients`, including their subfolders. Hidden mail is not indexed and cannot be opened by id. |
| `update_check` | `true` | Ask GitHub at most once a day whether a newer release exists. |
| `index_path` | `~/Library/Caches/mailidx/index.sqlite` | Where the search index lives. |

`mailidx status` shows the version, update state, mail and index counts and the current settings in one view.

## Updates

`mailidx update` downloads the latest release, checks it against the published SHA-256 checksum, makes sure it starts and replaces the binary in place. `mailidx update --check` only looks.

When a newer release exists, the first tool result of a session carries a short note, so the agent can tell you. The `update` tool installs it from inside the MCP client if you agree. In both cases the client has to reconnect mailidx afterwards.

## Tools

| Tool | What it does |
|---|---|
| `accounts`, `mailboxes` | Accounts with addresses, mailboxes with roles and counts |
| `list_messages` | Messages in a mailbox, newest first, with filters |
| `search` | Full-text search plus filters for sender, recipient, date, unread, flagged, attachments, category |
| `get_message`, `get_thread`, `get_source` | One message, a whole conversation, or the raw RFC 822 source |
| `save_attachments`, `export_messages` | Attachments to disk, messages to `.eml`, `.md` or `.json` |
| `stats`, `overview`, `triage` | Volume and top senders, an inbox dashboard, who waits for a reply |
| `index_status` | State of the search index, with optional rebuild |
| `compose`, `reply`, `forward` | Write mail through Mail's own compose window, draft by default |
| `update_messages` | Read, unread, flag, flag color, move, trash, junk |
| `drafts` | List, send or trash drafts |
| `create_mailbox`, `check_mail`, `open_in_mail` | New folder, fetch mail, show a message in Mail |
| `update` | Check for a newer mailidx release and install it |

## Benchmark

Measured on an M5 Pro with macOS 27.2 and 694 messages in one iCloud account, against [`mcp-apple-mail`](https://pypi.org/project/mcp-apple-mail/), which goes through AppleScript for every call. Median of three runs.

| | mailidx | mcp-apple-mail |
|---|---:|---:|
| Server start | 23 ms | 587 ms |
| Latest 20 inbox messages | 0.4 ms | 1,018 ms |
| Subject search | 0.6 ms | 440 ms |
| Body search | 1.3 ms | 175 s, then an error |
| Statistics | 1.4 ms | 6,883 ms |

Rebuilding the whole search index for those messages takes well under a second.

## How it works

- Mail keeps every message's metadata in `~/Library/Mail/V*/MailData/Envelope Index`, a SQLite database. mailidx opens it read-only and checks at startup that the columns it needs exist, so a changed schema in a future macOS fails with a clear message.
- Message bodies come from the `.emlx` files next to it. Messages Mail has not downloaded completely are flagged and indexed once they are.
- The search index lives in `~/Library/Caches/mailidx/index.sqlite` (mode 0600) and syncs incrementally.
- Writes run AppleScript through `osascript`. Values are passed as arguments, never pasted into the script text. The clipboard is saved before a paste and restored afterwards, all items and types.

## Command line

```
mailidx                    run the MCP server on stdio
mailidx status [--json]    version, update, mail and index state, settings
mailidx update [--check]   install the latest release, or only look
mailidx config             show settings; also get, set, unset, path
mailidx index              update the search index and print stats
mailidx index --rebuild    rebuild it from scratch
mailidx search <query>     search from the terminal (JSON lines)
mailidx doctor             check permissions and paths
```

## Development

```sh
cargo build --release
cargo test
scripts/build-universal.sh   # dist/mailidx for arm64 and x86_64
```

Test write tools only with mail addressed to yourself, and clean up through the trash.

## License

[MIT](LICENSE)
