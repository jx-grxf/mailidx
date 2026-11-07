<div align="center">

# mailidx

An Apple Mail MCP server for macOS. It lets Claude, Codex or any other MCP client search, read and write your Apple Mail, and it answers in milliseconds because it reads Mail's own index directly.

[![Release](https://img.shields.io/github/v/release/jx-grxf/mailidx?label=release)](https://github.com/jx-grxf/mailidx/releases/latest)
![Platform](https://img.shields.io/badge/platform-macOS-111111)
![Rust](https://img.shields.io/badge/rust-2024-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

[Install](#install) · [Tools](docs/tools.md) · [Recipes](docs/recipes.md) · [How it works](docs/architecture.md) · [Settings](#settings) · [Benchmark](#benchmark)

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
- Calendar invitations are read from the message: title, time, place, organizer and who has answered.
- Signature logos and other small inline images do not count as attachments.

**Testing** your own forms and sign-ups

- `wait_for` waits until a matching mail arrives and tells you which mailbox it landed in, junk included.
- One-time codes and confirm links are pulled out of the message, so an agent can finish a sign-up or approve an entry without reading the whole mail.

**Writing** goes through Mail itself

- New mail, replies and forwards open Mail's own compose window, so your signature, the quoted original and forwarded attachments look exactly like they do when you click the button yourself.
- Everything is saved as a draft unless the caller passes `send: true`.
- Mark read or unread, flag with a color, move, junk, trash, create mailboxes, send or discard drafts, fetch new mail.
- Rename mailboxes. There is no permanent delete. Trash only.

**Cleaning up**

- `unsubscribe` lists every newsletter you get, grouped by sender with counts and unread mail, and leaves one on request: one-click where the sender supports it (RFC 8058), else by mail, else it hands you the link.
- `update_messages` takes a filter for bulk work, such as everything from one sender or all promotions older than a month. It shows the matches first and changes nothing until it is called again with `dry_run: false`.
- `contacts` finds the people you actually write with, by name or address, without newsletters and no-reply senders.

Your mail never leaves the Mac. mailidx itself only talks to GitHub to check for a new release, at most once a day and only if you leave that on, and to a newsletter's unsubscribe address when you ask it to leave that list.

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
| `send_locked` | `false` | Tools can only save drafts. `send: true`, `drafts send` and `unsubscribe` are refused. |
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
| `overview`, `accounts`, `mailboxes` | Inbox dashboard, accounts with addresses, mailboxes with roles and counts |
| `list_messages`, `search` | Messages in a mailbox; full-text search with filters for sender, recipient, date, unread, flagged, attachments, invitations, category |
| `get_message`, `get_thread`, `get_source` | One message with invitations and extracted codes, a whole conversation, or the raw source |
| `wait_for` | Wait for a new matching mail, with its code and confirm link |
| `contacts` | People you write with, by name or address |
| `save_attachments`, `export_messages` | Attachments to disk, messages to `.eml`, `.md` or `.json` |
| `stats`, `triage` | Volume and top senders; who waits for a reply |
| `compose`, `reply`, `forward`, `drafts` | Write through Mail's own compose window, draft by default |
| `update_messages` | Read, unread, flag, move, trash, junk, by ids or by filter in bulk |
| `unsubscribe` | List newsletters and leave them |
| `create_mailbox`, `rename_mailbox`, `check_mail`, `open_in_mail` | Folders, fetch mail, show a message in Mail |
| `index_status`, `update` | Search index state and rebuild; install a newer mailidx |

Parameters and details for each tool: [docs/tools.md](docs/tools.md). Typical jobs step by step: [docs/recipes.md](docs/recipes.md).

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

mailidx opens Mail's SQLite "Envelope Index" read-only, reads message bodies from the `.emlx` files and keeps its own full-text index in `~/Library/Caches/mailidx/index.sqlite`. Writes run AppleScript through `osascript`, with every value passed as an argument. The details, including how inline images and codes are recognized and what Mail's scripting cannot do, are in [docs/architecture.md](docs/architecture.md).

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
