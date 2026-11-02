# mailidx

Apple Mail MCP server in Rust. Reads come straight from Mail's own files: the SQLite
"Envelope Index" (read-only) and the `.emlx` messages, plus our own FTS5 index in
`~/Library/Caches/mailidx/`. Writes (compose, reply, move, flags …) go through Mail via
AppleScript (`osascript`).

## Commands

- `cargo build --release` · `cargo test` · `cargo clippy`
- `./target/release/mailidx doctor` checks Full Disk Access, the index and Mail automation.
- `./target/release/mailidx index --rebuild` · `./target/release/mailidx search "<query>"`
- The server speaks MCP on stdio when run without arguments.

## Map

- `src/store.rs` Envelope Index: mailboxes, filters, message rows, `.emlx` paths, schema check.
- `src/emlx.rs` `.emlx` parsing, body text, links, attachments (incl. `.partial.emlx`).
- `src/fts.rs` own full-text index, incremental sync, query builder.
- `src/script.rs` every AppleScript call. `src/tools.rs` the MCP tools. `src/time.rs` dates.

## Rules that protect the user's mail

- Never open the Envelope Index or anything under `~/Library/Mail` writable.
- No permanent deletion through AppleScript. `set deleted status` crashed Mail; trash only.
- No mailbox-wide AppleScript scans (`every message whose content contains …`); they made
  Mail restart. Address one message by id: `first message of <mailbox> whose id is N`.
- compose, reply and forward save drafts unless the caller passes `send: true`.
- Test write paths only with mail addressed to the developer, and clean up via trash.
- AppleScript values go in through `argv`, never by string interpolation.
- Mail's index schema is undocumented. Keep `REQUIRED_COLUMNS` in `store.rs` in sync with
  any new column you read.

## Style

- Code, comments, commits and README in English.
- Comments only for the why. No banner comments, no restating the code.
