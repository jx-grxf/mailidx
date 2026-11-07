use std::process::Stdio;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(45);

/// A message as Mail's scripting dictionary addresses it. `account` is empty for
/// "On My Mac" mailboxes, which live at the application level.
#[derive(Debug, Clone)]
pub struct MsgRef {
    pub account: String,
    pub mailbox: String,
    pub id: i64,
}

impl MsgRef {
    fn push(&self, args: &mut Vec<String>) {
        args.push(self.account.clone());
        args.push(self.mailbox.clone());
        args.push(self.id.to_string());
    }
}

const PRELUDE: &str = r#"
on mbx(acct, p)
    tell application "Mail"
        if acct is "" then return mailbox p
        return mailbox p of account acct
    end tell
end mbx

on msg(acct, p, mid)
    tell application "Mail"
        set b to my mbx(acct, p)
        return first message of b whose id is (mid as integer)
    end tell
end msg

on splitLines(s)
    if s is "" then return {}
    return paragraphs of s
end splitLines
"#;

pub async fn run(body: &str, args: &[String]) -> Result<String> {
    run_full("", "", body, args).await
}

async fn run_full(header: &str, handlers: &str, body: &str, args: &[String]) -> Result<String> {
    let script = format!("{header}\n{PRELUDE}\n{handlers}\non run argv\n{body}\nend run\n");
    let mut child = Command::new("/usr/bin/osascript")
        .arg("-")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes()).await?;
    }
    let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("Mail did not answer within {}s; nothing was retried", TIMEOUT.as_secs()))??;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        bail!("{}", explain(&err));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn explain(err: &str) -> String {
    if err.contains("-1743") || err.contains("Not authorized") {
        return "macOS blocked automation of Mail. Allow it in System Settings → Privacy & Security → Automation for the app running mailidx.".into();
    }
    if err.contains("-1719") || err.contains("-1728") || err.contains("Can’t get") {
        return format!("Mail could not find that message or mailbox (it may have moved). Details: {err}");
    }
    if err.contains("-609") || err.contains("Connection is invalid") {
        return "Mail quit while handling the request. Nothing was retried.".into();
    }
    err.to_string()
}

pub async fn set_status(refs: &[MsgRef], field: &str, value: &str) -> Result<String> {
    let mut args = vec![field.to_string(), value.to_string()];
    for r in refs {
        r.push(&mut args);
    }
    run(
        r#"
    set fld to item 1 of argv
    set val to item 2 of argv
    set n to 0
    set errs to ""
    tell application "Mail"
        repeat with i from 3 to (count of argv) by 3
            try
                set m to my msg(item i of argv, item (i + 1) of argv, item (i + 2) of argv)
                if fld is "read" then
                    set read status of m to (val is "true")
                else if fld is "flagged" then
                    set flagged status of m to (val is "true")
                else if fld is "flag_color" then
                    set flag index of m to (val as integer)
                else if fld is "junk" then
                    set junk mail status of m to (val is "true")
                end if
                set n to n + 1
            on error e
                set errs to errs & (item (i + 2) of argv) & ": " & e & linefeed
            end try
        end repeat
    end tell
    return (n as text) & linefeed & errs
"#,
        &args,
    )
    .await
}

pub async fn move_to(refs: &[MsgRef], account: &str, mailbox: &str) -> Result<String> {
    let mut args = vec![account.to_string(), mailbox.to_string()];
    for r in refs {
        r.push(&mut args);
    }
    run(
        r#"
    set n to 0
    set errs to ""
    tell application "Mail"
        set target to my mbx(item 1 of argv, item 2 of argv)
        repeat with i from 3 to (count of argv) by 3
            try
                set m to my msg(item i of argv, item (i + 1) of argv, item (i + 2) of argv)
                move m to target
                set n to n + 1
            on error e
                set errs to errs & (item (i + 2) of argv) & ": " & e & linefeed
            end try
        end repeat
    end tell
    return (n as text) & linefeed & errs
"#,
        &args,
    )
    .await
}

/// Moves to the account's trash, exactly like pressing Delete in Mail.
/// Permanent deletion is deliberately not offered: setting `deleted status`
/// through AppleScript crashed Mail in testing.
pub async fn trash(refs: &[MsgRef]) -> Result<String> {
    let mut args = Vec::new();
    for r in refs {
        r.push(&mut args);
    }
    run(
        r#"
    set n to 0
    set errs to ""
    tell application "Mail"
        repeat with i from 1 to (count of argv) by 3
            try
                set m to my msg(item i of argv, item (i + 1) of argv, item (i + 2) of argv)
                delete m
                set n to n + 1
            on error e
                set errs to errs & (item (i + 2) of argv) & ": " & e & linefeed
            end try
        end repeat
    end tell
    return (n as text) & linefeed & errs
"#,
        &args,
    )
    .await
}

pub struct Outgoing {
    pub subject: String,
    pub body: String,
    pub html: Option<String>,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub attachments: Vec<String>,
    pub send: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    New,
    Reply,
    ReplyAll,
    Forward,
}

static GUI_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const OBJC_HEADER: &str = r#"use framework "Foundation"
use framework "AppKit"
use scripting additions
"#;

/// Mail's scripting `content` is detached from the compose editor: setting it
/// drops the quoted original, the attachments of a forward and the signature
/// layout. So the message is built the way a person would: Mail opens its own
/// reply/forward/new window, and the text is pasted at the cursor. The whole
/// clipboard (every item and type) is restored afterwards.
const GUI_HANDLERS: &str = r#"
on snapshotPB(pb)
    set saved to {}
    set pbItems to pb's pasteboardItems()
    if pbItems is missing value then return saved
    repeat with anItem in (pbItems as list)
        set ni to current application's NSPasteboardItem's alloc()'s init()
        repeat with t in ((anItem's types()) as list)
            set d to (anItem's dataForType:t)
            if d is not missing value then (ni's setData:d forType:t)
        end repeat
        set end of saved to ni
    end repeat
    return saved
end snapshotPB

on restorePB(pb, saved)
    pb's clearContents()
    if (count of saved) > 0 then pb's writeObjects:saved
end restorePB

on loadBody(pb, bodyPath, isHtml)
    set str to current application's NSString's stringWithContentsOfFile:bodyPath encoding:(current application's NSUTF8StringEncoding) |error|:(missing value)
    if str is missing value then error "could not read message body"
    pb's clearContents()
    if isHtml is "true" then
        set bytes to str's dataUsingEncoding:(current application's NSUTF8StringEncoding)
        set keys to {current application's NSDocumentTypeDocumentAttribute, current application's NSCharacterEncodingDocumentAttribute}
        set vals to {current application's NSHTMLTextDocumentType, current application's NSUTF8StringEncoding}
        set opts to current application's NSDictionary's dictionaryWithObjects:vals forKeys:keys
        set attr to current application's NSAttributedString's alloc()'s initWithData:bytes options:opts documentAttributes:(missing value) |error|:(missing value)
        if attr is missing value then error "could not render the HTML body"
        set rtf to attr's RTFFromRange:(current application's NSMakeRange(0, attr's |length|())) documentAttributes:(current application's NSDictionary's dictionary())
        pb's setData:rtf forType:(current application's NSPasteboardTypeRTF)
        pb's setString:(attr's |string|()) forType:(current application's NSPasteboardTypeString)
    else
        pb's setString:str forType:(current application's NSPasteboardTypeString)
    end if
end loadBody

on fill(m, sndr, toL, ccL, bccL, atts)
    tell application "Mail"
        if sndr is not "" then set sender of m to sndr
        repeat with a in my splitLines(toL)
            if a as text is not "" then make new to recipient at end of to recipients of m with properties {address:(a as text)}
        end repeat
        repeat with a in my splitLines(ccL)
            if a as text is not "" then make new cc recipient at end of cc recipients of m with properties {address:(a as text)}
        end repeat
        repeat with a in my splitLines(bccL)
            if a as text is not "" then make new bcc recipient at end of bcc recipients of m with properties {address:(a as text)}
        end repeat
        repeat with p in my splitLines(atts)
            if p as text is not "" then
                tell content of m
                    make new attachment with properties {file name:(POSIX file (p as text))} at after last paragraph
                end tell
            end if
        end repeat
    end tell
end fill

on findWebArea(el, depth)
    tell application "System Events"
        repeat with c in (UI elements of el)
            if role of c is "AXWebArea" then return contents of c
        end repeat
        if depth < 3 then
            repeat with c in (UI elements of el)
                if role of c is in {"AXScrollArea", "AXGroup", "AXSplitGroup"} then
                    set found to my findWebArea(c, depth + 1)
                    if found is not missing value then return found
                end if
            end repeat
        end if
    end tell
    return missing value
end findWebArea

on pasteIntoMail(subj)
    tell application "Mail" to activate
    tell application "System Events" to tell process "Mail"
        set cw to missing value
        repeat 50 times
            try
                set frontmost to true
                set cw to (first window whose name is subj)
                perform action "AXRaise" of cw
                if value of attribute "AXMain" of cw is true then exit repeat
            end try
            delay 0.1
        end repeat
        if cw is missing value then error "compose window not visible to Accessibility"
        if value of attribute "AXMain" of cw is not true then error "compose window did not become Mail's main window; nothing was pasted"
        -- Mail focuses the To field when recipients are set; the paste must land in the body.
        set webArea to my findWebArea(cw, 0)
        if webArea is missing value then error "message body not found in the compose window"
        -- Edit is the 4th menu (Apple, Mail, File, Edit) in every language; Paste is the item bound to plain Cmd+V.
        set pasteItem to missing value
        repeat with mi in (menu items of menu 1 of menu bar item 4 of menu bar 1)
            try
                if (value of attribute "AXMenuItemCmdChar" of mi) is "V" and (value of attribute "AXMenuItemCmdModifiers" of mi) is 0 then
                    set pasteItem to contents of mi
                    exit repeat
                end if
            end try
        end repeat
        if pasteItem is missing value then error "Mail's Paste menu item was not found"
        -- While Mail is still laying out attachments or the signature, the body is not yet editable and Paste stays disabled.
        repeat 15 times
            set value of attribute "AXFocused" of webArea to true
            delay 0.3
            if enabled of pasteItem then exit repeat
        end repeat
        if not (enabled of pasteItem) then error "Mail's Paste menu item is disabled; nothing was pasted"
        perform action "AXPress" of pasteItem
    end tell
    delay 1.0
end pasteIntoMail

on closeAsDraft(subj)
    tell application "System Events" to tell process "Mail"
        set cw to (first window whose name is subj)
        perform action "AXPress" of (value of attribute "AXCloseButton" of cw)
        repeat 50 times
            if (count of sheets of cw) > 0 then exit repeat
            if not (exists (first window whose name is subj)) then return
            delay 0.1
        end repeat
        if (count of sheets of cw) = 0 then error "compose window did not close"
        -- The default button of "Save this message as a draft?" is Save, in every language.
        perform action "AXPress" of (value of attribute "AXDefaultButton" of sheet 1 of cw)
    end tell
    delay 0.5
end closeAsDraft

on composeWindow(subj)
    tell application "Mail"
        repeat 30 times
            set ws to (every window whose name is subj)
            if (count of ws) > 0 then return item 1 of ws
            delay 0.1
        end repeat
    end tell
    error "compose window not found"
end composeWindow
"#;

const GUI_BODY: &str = r#"
    set composeMode to item 1 of argv
    set pb to current application's NSPasteboard's generalPasteboard()
    set saved to my snapshotPB(pb)
    try
        my loadBody(pb, item 6 of argv, item 7 of argv)
        tell application "Mail"
            if composeMode is "new" then
                set m to make new outgoing message with properties {subject:(item 5 of argv), visible:true}
            else
                set orig to my msg(item 2 of argv, item 3 of argv, item 4 of argv)
                if composeMode is "reply" then
                    set m to reply orig with opening window
                else if composeMode is "replyall" then
                    set m to reply orig with opening window and reply to all
                else
                    set m to forward orig with opening window
                end if
            end if
        end tell
        my fill(m, item 8 of argv, item 9 of argv, item 10 of argv, item 11 of argv, item 12 of argv)
        tell application "Mail" to set subj to subject of m
        set w to my composeWindow(subj)
        my pasteIntoMail(subj)
        my restorePB(pb, saved)
        tell application "Mail"
            if (item 13 of argv) is "true" then
                send m
                set res to "sent"
            else
                set res to "draft"
            end if
        end tell
        if res is "draft" then my closeAsDraft(subj)
        return res & linefeed & subj
    on error e
        my restorePB(pb, saved)
        error e
    end try
"#;

/// With the screen locked macOS brings no window to the front and Accessibility
/// sees none, so the paste would fail after Mail had already saved an empty draft.
async fn screen_locked() -> bool {
    let Ok(out) = Command::new("/usr/sbin/ioreg").args(["-n", "Root", "-d1", "-a"]).output().await else {
        return false;
    };
    let xml = String::from_utf8_lossy(&out.stdout);
    xml.split_once("<key>CGSSessionScreenIsLocked</key>")
        .is_some_and(|(_, rest)| rest.trim_start().starts_with("<true/>"))
}

pub async fn compose_window(kind: Kind, orig: Option<&MsgRef>, o: &Outgoing) -> Result<(String, String)> {
    let _guard = GUI_LOCK.lock().await;
    if screen_locked().await {
        bail!("the Mac's screen is locked, so Mail cannot open its compose window. Nothing was created; try again once the user has unlocked it.");
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let body_path = std::env::temp_dir().join(format!("mailidx-body-{}-{nanos}", std::process::id()));
    let (content, is_html) = match &o.html {
        Some(h) => (h.clone(), true),
        None => (o.body.clone(), false),
    };
    write_private(&body_path, content.as_bytes())?;
    let (acct, mbx, id) = match orig {
        Some(r) => (r.account.clone(), r.mailbox.clone(), r.id.to_string()),
        None => (String::new(), String::new(), String::new()),
    };
    let args = vec![
        match kind {
            Kind::New => "new",
            Kind::Reply => "reply",
            Kind::ReplyAll => "replyall",
            Kind::Forward => "forward",
        }
        .to_string(),
        acct,
        mbx,
        id,
        o.subject.clone(),
        body_path.display().to_string(),
        is_html.to_string(),
        o.from.clone(),
        o.to.join("\n"),
        o.cc.join("\n"),
        o.bcc.join("\n"),
        o.attachments.join("\n"),
        o.send.to_string(),
    ];
    let res = run_full(OBJC_HEADER, GUI_HANDLERS, GUI_BODY, &args).await;
    let _ = std::fs::remove_file(&body_path);
    let out = res?;
    let (status, subject) = out.split_once('\n').unwrap_or((&out, ""));
    Ok((status.trim().to_string(), subject.trim().to_string()))
}

fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(data)?;
    Ok(())
}

pub async fn rename_mailbox(account: &str, path: &str, new_name: &str) -> Result<String> {
    run(
        r#"
    set acc to item 1 of argv
    set p to item 2 of argv
    set nm to item 3 of argv
    tell application "Mail"
        if acc is "" then
            set mb to mailbox p
        else
            set mb to mailbox p of account acc
        end if
        set name of mb to nm
    end tell
    return "ok"
"#,
        &[account.to_string(), path.to_string(), new_name.to_string()],
    )
    .await
}

pub async fn create_mailbox(account: &str, name: &str) -> Result<String> {
    run(
        r#"
    set acc to item 1 of argv
    set nm to item 2 of argv
    tell application "Mail"
        try
            if acc is "" then
                get name of mailbox nm
            else
                get name of mailbox nm of account acc
            end if
            return "exists"
        end try
        if acc is "" then
            make new mailbox with properties {name:nm}
        else
            tell account acc to make new mailbox with properties {name:nm}
        end if
        -- Mail returns before an IMAP server has created the folder, and a nested
        -- iCloud folder once showed up only minutes later.
        repeat 10 times
            try
                if acc is "" then
                    get name of mailbox nm
                else
                    get name of mailbox nm of account acc
                end if
                return "ok"
            end try
            delay 0.3
        end repeat
    end tell
    return "missing"
"#,
        &[account.to_string(), name.to_string()],
    )
    .await
}

pub async fn check_mail(account: &str) -> Result<String> {
    run(
        r#"
    tell application "Mail"
        if (item 1 of argv) is "" then
            check for new mail
            repeat with a in every account
                try
                    synchronize with a
                end try
            end repeat
        else
            check for new mail for account (item 1 of argv)
            synchronize with account (item 1 of argv)
        end if
    end tell
    return "ok"
"#,
        &[account.to_string()],
    )
    .await
}

pub async fn account_addresses() -> Result<Vec<(String, Vec<String>)>> {
    let out = run(
        r#"
    set res to ""
    tell application "Mail"
        set accts to every account
        repeat with i from 1 to count of accts
            set a to item i of accts
            set addrs to email addresses of a
            set res to res & (name of a)
            repeat with j from 1 to count of addrs
                set res to res & tab & (item j of addrs)
            end repeat
            set res to res & linefeed
        end repeat
    end tell
    return res
"#,
        &[],
    )
    .await?;
    Ok(out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split('\t');
            let name = it.next().unwrap_or("").to_string();
            (name, it.map(|s| s.to_lowercase()).collect())
        })
        .collect())
}

pub async fn open_in_mail(r: &MsgRef) -> Result<String> {
    let mut args = Vec::new();
    r.push(&mut args);
    run(
        r#"
    set m to my msg(item 1 of argv, item 2 of argv, item 3 of argv)
    tell application "Mail"
        open m
        activate
    end tell
    return "ok"
"#,
        &args,
    )
    .await
}

pub async fn save_attachment(r: &MsgRef, index: usize, dest: &str) -> Result<String> {
    let mut args = Vec::new();
    r.push(&mut args);
    args.push((index + 1).to_string());
    args.push(dest.to_string());
    run(
        r#"
    set m to my msg(item 1 of argv, item 2 of argv, item 3 of argv)
    tell application "Mail"
        set a to item ((item 4 of argv) as integer) of (mail attachments of m)
        save a in (POSIX file (item 5 of argv))
    end tell
    return "ok"
"#,
        &args,
    )
    .await
}
