# How mailidx works

## Reading

Apple Mail keeps the metadata of every message in a SQLite database, `~/Library/Mail/V*/MailData/Envelope Index`: subject, sender, recipients, dates, flags, mailbox, thread, and on recent macOS the Apple Intelligence category and summary. mailidx opens it read-only. At startup it checks that every column it reads exists, so a schema change in a future macOS stops with a clear message instead of wrong results.

Message bodies live next to it as `.emlx` files: the RFC 822 source followed by a small property list. For large messages Mail writes a `.partial.emlx` and keeps the attachments in a separate folder; mailidx reads both forms.

Nothing in `~/Library/Mail` is ever opened for writing.

## The search index

mailidx keeps its own SQLite file in `~/Library/Caches/mailidx/index.sqlite` (mode 0600):

- An FTS5 table with subject, sender, recipients, body and attachment names, tokenized with `unicode61` and diacritics removed. Ranking uses bm25 with subject weighted highest.
- One row per message with what the Envelope Index does not know: how many attachments are inline images, the `List-Unsubscribe` target and whether it supports one-click, and whether the message carries a calendar invitation.

The index syncs incrementally. A cheap fingerprint of the Envelope Index (highest row id and message count) tells whether anything changed. FSEvents reports writes to the Envelope Index, and about a second after Mail goes quiet the new messages are indexed in the background. Messages whose body Mail has not downloaded yet are indexed from their metadata and read again later.

The index file is attached to the read-only Envelope Index connection, so one query can filter by both, for example `has_attachments` without counting signature logos. An index from an older mailidx without those columns is simply not attached, and the affected filters fall back or say that the index needs a rebuild.

### Inline images

An image counts as inline when it is at most 64 KB and either the HTML part shows it through a `cid:` reference or, as Apple Mail sends it, it sits between the HTML parts marked `Content-Disposition: inline`. That covers signature logos and icons. Larger embedded images are usually photos the sender meant to send, so they stay attachments.

### Codes and confirm links

`extract` looks for numbers of four to eight digits, or six to eight characters of capitals and digits, near words like "code", "Bestätigung", "verification" or "PIN". A bare number counts in the subject line, where senders like to put it. Dates, times, amounts, years and anything inside a URL are ignored.

Links are ranked by words such as verify, confirm, activate, login, reset, review or approve in the URL. Unsubscribe and privacy links and images are dropped.

## Writing

Every change goes through Mail with AppleScript, run by `/usr/bin/osascript`. Values are handed over as arguments, never pasted into the script text.

- A message is addressed by mailbox and id (`first message of <mailbox> whose id is N`). mailidx never scans a whole mailbox with `whose content contains …`; such scans made Mail restart.
- `compose`, `reply` and `forward` open Mail's own window, so signature, quote and forwarded attachments are Mail's. The text goes in through the clipboard, which is saved before and restored afterwards with all its items and types. This is the only step that needs Accessibility, and it needs Mail to be the front app for about two seconds.
- Bulk changes are sent to Mail in blocks of 50 messages, so no single AppleScript call runs into mailidx's 45-second timeout.
- Nothing is deleted for good. Setting the `deleted status` crashed Mail, so trash is the only way out.
- Mailboxes can be created and renamed. Mail's scripting refuses to delete them.

## Network

mailidx itself makes two kinds of requests:

- At most once a day it asks GitHub whether a newer release exists. `update_check = false` turns that off.
- `unsubscribe` with a one-click sender sends one HTTPS POST to the address in that sender's `List-Unsubscribe` header, as RFC 8058 defines. Only when an agent calls it with `action: "unsubscribe"`, and never with `send_locked` on. Plain `http` is refused.

Sending mail is Mail's job and uses your accounts as configured there.
