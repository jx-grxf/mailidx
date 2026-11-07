# Recipes

Common jobs and the tool calls behind them. You ask your agent in plain words; this page shows what it does, so you know what to expect and can check it.

## Did the contact form mail arrive?

You submit a form on your website and want to know whether the notification reached you, and where.

```json
wait_for { "subject": "New contact form message", "since": "2m", "timeout_seconds": 120 }
```

`since` covers the case where the mail came in before the call started. The result names the mailbox, so a mail in `Junk` shows that your sending domain needs work, not the form.

## Confirm a sign-up or log in with a code

```json
wait_for { "from": "example.com", "extract": true }
```

The result carries `codes` (for example `["482913"]`) and `verify_links`. The agent enters the code or opens the link; for an approval link (a guestbook entry, a review request) it can follow it the same way.

For a mail that is already there:

```json
get_message { "id": 2827, "extract": true }
```

## Clean up newsletters

1. See what you are subscribed to:

   ```json
   unsubscribe {}
   ```

   Each sender comes with its message count, unread count, latest date and whether one-click unsubscribe works.

2. Leave the ones you no longer read:

   ```json
   unsubscribe { "action": "unsubscribe", "sender": "hello@shop.example" }
   ```

3. Clear what they already sent. The first call only reports what matches:

   ```json
   update_messages { "action": "trash", "filter": { "from": "hello@shop.example" } }
   update_messages { "action": "trash", "filter": { "from": "hello@shop.example" }, "dry_run": false }
   ```

Promotions in bulk work the same way, with `"filter": { "category": "promotions", "until": "30d" }` for everything older than 30 days.

## Write to someone by name

```json
contacts { "query": "anna" }
compose { "to": ["anna.muster@example.com"], "subject": "…", "body": "…" }
```

The draft lands in Mail; it is only sent when you say so.

## What needs an answer?

```json
overview {}
triage { "mode": "needs_reply" }
triage { "mode": "awaiting_reply", "since": "30d" }
```

## Meeting invitations

```json
search { "has_invite": true, "since": "14d" }
get_message { "id": 2901 }
```

`get_message` returns the invitation with time, place, organizer and who has answered.

## Archive a project

```json
search { "query": "\"Projekt Linz\"", "since": "1y" }
export_messages { "query": "\"Projekt Linz\"", "format": "md", "directory": "~/Documents/Projekt Linz" }
update_messages { "action": "move", "mailbox": "Archive", "filter": { "query": "\"Projekt Linz\"" } }
```
