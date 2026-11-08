# Tools

mailidx has 26 tools. Read tools answer from Mail's index and message files and never wait for Mail.app. Write tools go through Mail with AppleScript, so Mail has to be running.

Message ids are Mail's own row ids. They work in every tool. Moving a message gives it a new id; `update_messages` returns the new ones as `new_ids`.

Dates accept `2026-11-01`, `2026-11-01 08:00`, `today`, `yesterday`, or a span back from now: `30m`, `12h`, `7d`, `2w`, `1y`.

- [Read](#read)
- [Find and wait](#find-and-wait)
- [Write](#write)
- [Clean up](#clean-up)
- [Mailboxes](#mailboxes)
- [Maintenance](#maintenance)

## Read

### `overview`

Unread count per account, the latest inbox messages, flagged messages and how many people wait for your reply. A good first call.

### `accounts`, `mailboxes`

Accounts with their addresses and counts. Mailboxes with role (`inbox`, `sent`, `drafts`, `trash`, `junk`, `archive`, `outbox` or `other`), message count and unread count.

### `list_messages`

Messages in one mailbox, newest first. Without `mailbox` it lists the inbox of every account.

| Parameter | |
|---|---|
| `mailbox` | Role, folder path (`Projects/2026`) or mailbox id |
| `unread`, `flagged`, `important` | `true` or `false` |
| `category` | `primary`, `transactions`, `updates`, `promotions`, as Apple Intelligence sorted them on this Mac |
| `has_invite` | Only mail with a calendar invitation |
| `since`, `until` | Date range on the received date |
| `limit`, `offset`, `oldest_first`, `preview` | Paging and output |

### `get_message`

One message with headers, recipients, body text and attachments.

- Inline images, such as the logo in a signature, are listed under `inline_images` and do not count as attachments.
- A calendar invitation comes back as `invites`: title, start, end, location, organizer, attendees with their answer, and the method (`REQUEST`, `CANCEL`, …).
- `links: true` adds every link in the message.
- `extract: true` adds `codes` (one-time codes such as `482913`) and `verify_links` (confirm, verify, log-in or review links, best match first). Unsubscribe links are never in that list.
- `format: "html"` returns the HTML part, `all_headers: true` the raw headers.

### `get_thread`

The whole conversation of a message across all mailboxes, including your sent replies, oldest first.

### `get_source`

The raw RFC 822 source, or only its headers. For DKIM, SPF and MIME questions.

### `save_attachments`, `export_messages`

Save one attachment or all of them, by default to `~/Downloads`; existing files are never overwritten. Export messages as `.eml`, Markdown or one JSON file, picked by ids or by query, mailbox and date.

### `stats`

Volume per mailbox and day, unread, top senders and domains, attachments and sent count for a time window (default 30 days).

### `triage`

`needs_reply`: mail from real people that you have not answered. `awaiting_reply`: mail you sent that got no answer. Newsletters and automated senders are left out.

## Find and wait

### `search`

Full-text search over subject, sender, recipients, body and attachment names, combined with the filters of `list_messages` plus `from`, `to`, `subject`, `has_attachments` and `has_invite`.

- All words must match, each as a prefix. `"exact phrase"`, `-word` to exclude, `OR` between words.
- Accents do not matter: `Muller` finds `Müller`.
- If no message has every word, the search tries any word and the result says `"matched": "any"`.
- Trash and junk are skipped unless `include_trash` is set or `mailbox` names them.
- `sort: "date"` orders by date instead of relevance. `raw_query` passes the query to SQLite FTS5 unchanged.

### `wait_for`

Waits until a new message that matches arrives, then returns it. Built for tests like "did the contact form mail come through?".

- Only mail that arrives after the call counts; a message moved or trashed meanwhile is not new. With `since` (for example the time you submitted the form), mail that arrived a moment earlier counts too.
- It asks Mail to fetch first (`fetch: false` turns that off) and then checks every two seconds until `timeout_seconds` (default 60, at most 600).
- It looks at incoming mail only. Sent, outbox and drafts are skipped, junk and trash are included, and every result names its mailbox. A form mail that landed in spam comes back with `"mailbox": "Junk"`.
- `extract: true` adds the one-time code and the confirm link, as in `get_message`.
- On a timeout the result is `"arrived": false`; call it again or loosen the filters.

Filters: `query`, `from`, `to`, `subject`, `mailbox`, `account`.

### `contacts`

People you write with: address, name, how many mails each side sent and when the last one was. Use it to turn "write to Anna" into an address.

- `query` matches name or address, accents ignored. Without it you get your most frequent contacts.
- Ranked by how often you wrote to them, then how often they wrote to you.
- Newsletters, mailing lists and no-reply senders are left out unless you wrote to them yourself or pass `include_automated: true`.

## Write

Write tools need the Automation permission for Mail. `compose`, `reply` and `forward` also need Accessibility, because the text is pasted into Mail's own compose window.

### `compose`, `reply`, `forward`

New mail, a reply (with `reply_all`) or a forward, through Mail's compose window. Your signature, the quoted original and forwarded attachments look as they do when you click the button yourself. Mail comes to the front for about two seconds.

Everything is saved as a draft unless the call passes `send: true`. With `send_locked` in the settings, sending is refused. While the Mac's screen is locked these tools refuse too, because macOS shows no window then.

### `drafts`

`list` shows the drafts of every account, `send` sends one, `trash` discards one.

### `update_messages`

Actions: `mark_read`, `mark_unread`, `flag`, `unflag`, `flag_color` (with `color`), `move` (with `mailbox`), `trash`, `junk`, `not_junk`.

Pick messages by `ids`, or by `filter` for bulk changes. See [Clean up](#clean-up).

### `check_mail`, `open_in_mail`

Ask Mail to fetch new mail now. Open a message in a Mail window so the user can look at it.

## Clean up

### `unsubscribe`

`action: "list"` (default) shows every sender whose mail carries a `List-Unsubscribe` header in the last 180 days, grouped by sender: number of messages, unread, latest date, category, mailboxes, and how to leave.

`action: "unsubscribe"` with an `id` or a `sender` leaves that list:

1. One-click (RFC 8058): if the sender supports it, mailidx sends the single HTTPS POST the standard defines. Nothing else is needed.
2. Mail: otherwise, if the sender offers a `mailto:` address, mailidx sends the unsubscribe mail through Mail.
3. Web: if there is only a web page, the result contains the link for the user to open.

With `send_locked` on, unsubscribing is refused, one-click included. Agents only unsubscribe when the user asked for it.

### `update_messages` with `filter`

The filter takes `query`, `from`, `to`, `subject`, `mailbox`, `account`, `since`, `until`, `unread`, `flagged` and `category`. Without `mailbox`, trash, junk, sent mail, drafts and the outbox are left out; name one as `mailbox` to include it.

- A filter call is a dry run unless `dry_run: false` is passed. It returns the number of matches and the first ten, so the agent can show them before anything changes.
- More matches than `max` (default 200, at most 2000) and nothing changes.
- A filter that selects all mail is refused.
- Mail gets the messages in blocks of 50.

Example: trash everything from one newsletter after leaving it.

```json
{ "action": "trash", "filter": { "from": "news@shop.example" } }
{ "action": "trash", "filter": { "from": "news@shop.example" }, "dry_run": false }
```

## Mailboxes

### `create_mailbox`

New folder in an account; `Parent/Child` nests it.

### `rename_mailbox`

Gives a folder a new name in the same place. Mail's own folders (inbox, sent, drafts, trash, junk, archive) keep their names.

Deleting a mailbox is not possible. Mail's scripting refuses it for every form of reference, so the user deletes folders in Mail.

## Maintenance

### `index_status`

Documents in the search index, its size and messages whose body Mail has not downloaded yet. `rebuild: true` builds it from scratch.

### `update`

Looks for a newer mailidx release on GitHub. `install: true` downloads it, checks the SHA-256 checksum and replaces the binary. Only with the user's agreement. The MCP client reconnects afterwards.
