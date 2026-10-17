//! Android Messages: Google Messages on the phone, with this computer paired to it the way
//! messages.google.com is. The phone stays the account and has to be online; texts (SMS and RCS)
//! go through it. Photos are shown. Other files are named and can be opened. Reactions can be
//! read and sent. Calls stay on the phone.

mod bugle;
mod fake;
mod media;
pub mod signin;

use super::{keep_connected, serve_commands, Command, Context, Failure, Refusal, Signed};
use crate::blobs;
use crate::session;
use crate::store::{ConversationRow, PersonRow};
use bugle::Chat;
use protocol::{
    Attachment, Capabilities, Change, ConversationKind, MediaFile, MediaKind, Message, MessageKind,
    Reaction, ReactionSet, Relation,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

/// Others are shown as typing for this long after the phone says they started.
const TYPING_SHOWN: i64 = 15;

fn scripted() -> bool {
    !session::enabled() && std::env::var_os("TINYCLIENT_MESSAGES_FAKE").is_some()
}

pub fn supported() -> bool {
    true
}

#[derive(Clone, Serialize, Deserialize)]
struct Saved {
    account: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    aes: String,
    #[serde(default)]
    hmac: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    ttl: i64,
    #[serde(default)]
    mobile_user: i64,
    #[serde(default)]
    mobile: String,
    #[serde(default)]
    browser_user: i64,
    #[serde(default)]
    browser: String,
    #[serde(default)]
    key: String,
}

pub enum Event {
    Ready,
    Chats(Vec<Chat>),
    Message(bugle::Note),
    Typing {
        conversation: String,
        by: String,
        name: String,
        started: bool,
    },
    Closed(String),
    Ended(String),
}

trait Drive: Send + Sync {
    fn events(&self) -> Option<std::sync::mpsc::Receiver<Event>>;
    fn me(&self) -> (String, String);
    fn refresh(&self) -> Result<(), String>;
    fn history(&self, conversation: &str) -> Result<Vec<bugle::Note>, String>;
    fn send(&self, conversation: &str, text: &str) -> Result<String, String>;
    fn send_file(
        &self,
        conversation: &str,
        caption: &str,
        path: &str,
        name: &str,
    ) -> Result<String, String>;
    fn react(&self, message: &str, emoji: &str, on: bool) -> Result<(), String>;
    fn fetch(
        &self,
        media_id: &str,
        key: &[u8],
        message: &str,
        action: &str,
    ) -> Result<Vec<u8>, String>;
    fn mark_read(&self, conversation: &str, message: &str) -> Result<(), String>;
    fn typing(&self, conversation: &str) -> Result<(), String>;
    fn close(&self);
}

struct LiveDrive {
    inner: bugle::Session,
}

impl Drive for LiveDrive {
    fn events(&self) -> Option<std::sync::mpsc::Receiver<Event>> {
        self.inner.events()
    }
    fn me(&self) -> (String, String) {
        self.inner.me()
    }
    fn refresh(&self) -> Result<(), String> {
        self.inner.refresh()
    }
    fn history(&self, conversation: &str) -> Result<Vec<bugle::Note>, String> {
        self.inner.history(conversation)
    }
    fn send(&self, conversation: &str, text: &str) -> Result<String, String> {
        self.inner.send(conversation, text)
    }
    fn send_file(
        &self,
        conversation: &str,
        caption: &str,
        path: &str,
        name: &str,
    ) -> Result<String, String> {
        self.inner.send_file(conversation, caption, path, name)
    }
    fn react(&self, message: &str, emoji: &str, on: bool) -> Result<(), String> {
        self.inner.react(message, emoji, on)
    }
    fn fetch(
        &self,
        media_id: &str,
        key: &[u8],
        message: &str,
        action: &str,
    ) -> Result<Vec<u8>, String> {
        self.inner.fetch(media_id, key, message, action)
    }
    fn mark_read(&self, conversation: &str, message: &str) -> Result<(), String> {
        self.inner.mark_read(conversation, message)
    }
    fn typing(&self, conversation: &str) -> Result<(), String> {
        self.inner.typing(conversation)
    }
    fn close(&self) {
        self.inner.close();
    }
}

fn parse(session: &str) -> Result<Saved, String> {
    let saved: Saved = serde_json::from_str(session)
        .map_err(|_| "the Messages session is unreadable".to_owned())?;
    if saved.account.is_empty() {
        return Err("the Messages session names no account".to_owned());
    }
    Ok(saved)
}

pub fn accounts(session: &str) -> Result<Vec<Signed>, String> {
    let saved = parse(session)?;
    Ok(vec![Signed {
        account: saved.account.clone(),
        name: if saved.name.is_empty() {
            "Messages".to_owned()
        } else {
            saved.name
        },
        session: session.to_owned(),
    }])
}

pub fn without(_session: &str, _account: &str) -> Option<String> {
    None
}

fn open(saved: &Saved) -> Result<Box<dyn Drive>, String> {
    if scripted() {
        return Ok(Box::new(fake::Fake::open(saved)));
    }
    let real = bugle::Saved {
        account: saved.account.clone(),
        name: saved.name.clone(),
        aes: saved.aes.clone(),
        hmac: saved.hmac.clone(),
        token: saved.token.clone(),
        ttl: saved.ttl,
        mobile_user: saved.mobile_user,
        mobile: if saved.mobile.is_empty() {
            saved.account.clone()
        } else {
            saved.mobile.clone()
        },
        browser_user: saved.browser_user,
        browser: saved.browser.clone(),
        key: saved.key.clone(),
    };
    Ok(Box::new(LiveDrive {
        inner: bugle::Session::open(real)?,
    }))
}

pub fn reaction_rules() -> protocol::ReactionRules {
    protocol::ReactionRules {
        set: ReactionSet::Any,
        mine: 1,
        replaces: true,
        ..protocol::ReactionRules::default()
    }
}

/// A file the shell may ask for once it is on screen. The bytes are still on the phone or in
/// Google's content store.
struct Held {
    media_id: String,
    key: Vec<u8>,
    message: String,
    action: String,
}

struct Account {
    ctx: Context,
    drive: Box<dyn Drive>,
    me: String,
    name: String,
    held: Mutex<HashMap<String, Held>>,
}

pub fn start(ctx: Context, session: &str, inbox: Receiver<Command>) -> Result<(), String> {
    let saved = parse(session)?;
    if saved.account != ctx.account.account {
        return Err("the Messages session is for another account".to_owned());
    }
    let current: Arc<Mutex<Option<Arc<Account>>>> = Arc::default();
    let (socket_ctx, held, socket_saved) = (ctx.clone(), current.clone(), saved.clone());
    std::thread::Builder::new()
        .name("messages-link".into())
        .spawn(move || {
            keep_connected(&socket_ctx, || connect(&socket_ctx, &socket_saved, &held));
            held.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
        })
        .map_err(|err| err.to_string())?;
    std::thread::Builder::new()
        .name("messages-commands".into())
        .spawn(move || {
            let account = || {
                current
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
            };
            serve_commands(
                &ctx,
                inbox,
                std::time::Duration::from_millis(200),
                |id| {
                    if let Some(account) = account() {
                        backfill(&account, id);
                    }
                },
                |command| match (account(), command) {
                    (Some(account), command) => serve(&account, command),
                    (None, Command::SignOut(reply)) => {
                        let _ = reply.send(Ok(()));
                    }
                    (None, command) => offline(&ctx, command),
                },
            );
            if let Some(account) = account() {
                account.drive.close();
            }
        })
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn connect(ctx: &Context, saved: &Saved, current: &Mutex<Option<Arc<Account>>>) -> Failure {
    let drive = match open(saved) {
        Ok(drive) => drive,
        Err(why) => return Failure::Dropped(why),
    };
    let Some(events) = drive.events() else {
        return Failure::Dropped("Messages has no events".to_owned());
    };
    let (me, name) = drive.me();
    let account = Arc::new(Account {
        ctx: ctx.clone(),
        drive,
        me,
        name,
        held: Mutex::default(),
    });
    ctx.capabilities(Capabilities {
        max_file_bytes: 100 * 1024 * 1024,
        max_files: 1,
        ..Capabilities::default()
    });
    let _ = ctx.describe(
        if account.name.is_empty() {
            "Messages"
        } else {
            &account.name
        },
        &account.me,
    );
    *current
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(account.clone());
    if ctx.stopped() {
        account.drive.close();
        return Failure::Dropped("stopped".to_owned());
    }
    ctx.link(protocol::Link::Connected);
    let ended = loop {
        match events.recv() {
            Ok(Event::Ready) => {
                if let Err(why) = account.drive.refresh() {
                    break Failure::Dropped(why);
                }
            }
            Ok(Event::Closed(why)) => break Failure::Dropped(why),
            Ok(Event::Ended(why)) => break Failure::SignedOut(why),
            Ok(event) => apply(&account, event),
            Err(_) => break Failure::Dropped("the connection closed".to_owned()),
        }
        if ctx.stopped() {
            break Failure::Dropped("stopped".to_owned());
        }
    };
    current
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    account.drive.close();
    ended
}

fn row(chat: &Chat) -> ConversationRow {
    let kind = if chat.group {
        ConversationKind::Group
    } else {
        ConversationKind::Direct
    };
    let mut row = ConversationRow::new(chat.id.clone(), kind, chat.title.clone());
    row.listed = true;
    row.private = !chat.group;
    // A call stays on the phone. The row opens messages.google.com.
    row.call = protocol::CallSupport::Handoff;
    if !chat.group && !chat.mine.is_empty() {
        row.peer = Some(chat.id.clone());
    }
    row
}

fn apply(account: &Account, event: Event) {
    match event {
        Event::Chats(chats) => {
            let mut people = Vec::new();
            for chat in &chats {
                let _ = account.ctx.conversation(&row(chat));
                if chat.unread {
                    let _ = account.ctx.counts(&chat.id, 1, 0, None, 0);
                }
                if !chat.group {
                    people.push(PersonRow {
                        id: chat.id.clone(),
                        name: chat.title.clone(),
                        handle: String::new(),
                        avatar: None,
                        bot: false,
                        status: String::new(),
                        link: String::new(),
                    });
                }
            }
            if !people.is_empty() {
                let _ = account.ctx.people(&people);
                let owned: Vec<(String, Relation)> = people
                    .iter()
                    .map(|person| (person.id.clone(), Relation::Contact))
                    .collect();
                let _ = account.ctx.relations(&owned);
            }
        }
        Event::Message(note) => {
            if account
                .ctx
                .known(&note.conversation)
                .ok()
                .flatten()
                .is_none()
            {
                let chat = Chat {
                    id: note.conversation.clone(),
                    title: note.name.clone(),
                    group: false,
                    unread: false,
                    mine: String::new(),
                };
                let _ = account.ctx.conversation(&row(&chat));
            }
            let _ = account
                .ctx
                .message(&note.conversation, &message(account, &note));
        }
        Event::Typing {
            conversation,
            by,
            name,
            started,
        } => {
            if started {
                let name = if name.is_empty() {
                    "Someone".into()
                } else {
                    name
                };
                account.ctx.typing(&conversation, &by, &name, TYPING_SHOWN);
            } else {
                account.ctx.typing_stopped(&conversation, &by);
            }
        }
        Event::Ready | Event::Closed(_) | Event::Ended(_) => {}
    }
}

fn message(account: &Account, note: &bugle::Note) -> Message {
    let outgoing = note.outgoing || note.author == account.me;
    let name = if outgoing {
        account.name.clone()
    } else if note.name.is_empty() {
        "Messages".into()
    } else {
        note.name.clone()
    };
    let author = if outgoing {
        account.me.as_str()
    } else {
        note.author.as_str()
    };
    let mut message = Message::plain(
        note.id.clone(),
        name,
        note.text.clone(),
        note.sent,
        outgoing,
    );
    message.sender.id = author.to_owned();
    let mut shown = Vec::new();
    for (index, piece) in note.pieces.iter().enumerate() {
        if piece.inline.is_empty() && piece.media_id.is_empty() {
            continue;
        }
        let id = format!("messages:{}:{index}", note.id);
        let mut file = MediaFile::new(id.clone(), kind_of(&piece.mime));
        file.width = piece.width;
        file.height = piece.height;
        file.bytes = if piece.size == 0 {
            piece.inline.len() as u64
        } else {
            piece.size
        };
        if !piece.inline.is_empty() {
            if let Some(path) = land(account, &file, &piece.inline) {
                file.path = Some(path);
                file.bytes = piece.inline.len() as u64;
            }
        } else {
            account
                .held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    id,
                    Held {
                        media_id: piece.media_id.clone(),
                        key: piece.key.clone(),
                        message: note.id.clone(),
                        action: piece.action_id.clone(),
                    },
                );
        }
        let label = if piece.name.is_empty() {
            "File".into()
        } else {
            piece.name.clone()
        };
        shown.push(Attachment {
            name: label,
            file,
            thumb: None,
            mime: piece.mime.clone(),
            duration: 0,
            spoiler: false,
            sticker: false,
            waveform: Vec::new(),
            round: false,
        });
    }
    message.attachments = shown;
    message.reactions = note
        .reactions
        .iter()
        .map(|mark| Reaction {
            emoji: mark.emoji.clone(),
            file: None,
            count: mark.who.len().max(1) as u32,
            mine: mark.who.iter().any(|who| who == &account.me),
        })
        .collect();
    if message.attachments.is_empty() && note.file {
        message.kind = MessageKind::Unsupported {
            link: format!(
                "https://messages.google.com/web/conversations/{}",
                note.conversation
            ),
        };
        if message.text.is_empty() {
            message.text = "File".into();
        }
    }
    message
}

fn kind_of(mime: &str) -> MediaKind {
    if mime == "image/gif" {
        MediaKind::Gif
    } else if mime.starts_with("image/") {
        MediaKind::Image
    } else {
        MediaKind::File
    }
}

fn media_root() -> std::path::PathBuf {
    protocol::support_dir().join("media")
}

/// Writes opened bytes where the cache keeps them. The address is a name; nothing fetches it.
fn land(account: &Account, file: &MediaFile, bytes: &[u8]) -> Option<String> {
    let ctx = &account.ctx;
    let path = blobs::path_of(&media_root(), &ctx.account, &file.id);
    let part = path.with_extension("part");
    std::fs::create_dir_all(blobs::dir_under(&media_root(), &ctx.account)).ok()?;
    std::fs::write(&part, bytes).ok()?;
    std::fs::rename(&part, &path).ok()?;
    let address = format!("messages://file/{}", file.id);
    let mut store = ctx.hub.store();
    store
        .register_media(&ctx.account, &[(file.clone(), address)])
        .ok()?;
    let landed = store
        .media_landed(
            &ctx.account,
            &file.id,
            &path.to_string_lossy(),
            bytes.len() as u64,
            crate::hub::now(),
        )
        .ok()??;
    drop(store);
    ctx.hub.changed(Change::Media {
        account: ctx.account.clone(),
        file: landed,
    });
    Some(path.to_string_lossy().into_owned())
}

fn wanted(account: &Account, ids: &[String]) {
    for id in ids {
        let held = account
            .held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .map(|held| Held {
                media_id: held.media_id.clone(),
                key: held.key.clone(),
                message: held.message.clone(),
                action: held.action.clone(),
            });
        let Some(held) = held else {
            continue;
        };
        match account
            .drive
            .fetch(&held.media_id, &held.key, &held.message, &held.action)
        {
            Ok(bytes) => {
                let mut file = MediaFile::new(id.clone(), MediaKind::File);
                file.bytes = bytes.len() as u64;
                let _ = land(account, &file, &bytes);
            }
            Err(why) => {
                let file = MediaFile::new(id.clone(), MediaKind::File);
                let mut store = account.ctx.hub.store();
                let _ = store.register_media(
                    &account.ctx.account,
                    &[(file, format!("messages://file/{id}"))],
                );
                if let Ok(Some(file)) = store.media_failed(&account.ctx.account, id, &why) {
                    drop(store);
                    account.ctx.hub.changed(Change::Media {
                        account: account.ctx.account.clone(),
                        file,
                    });
                }
            }
        }
    }
}

fn offline(ctx: &Context, command: Command) {
    let why = "Messages is offline. Keep your phone online.".to_owned();
    match command {
        Command::Send {
            conversation_id,
            client_id,
            ..
        }
        | Command::SendWith {
            conversation_id,
            client_id,
            ..
        } => {
            ctx.sent(&conversation_id, &client_id, Err(why));
        }
        Command::React { reply, .. } => drop(reply.send(Err(Refusal::Refused(why)))),
        Command::ReactionRules { reply, .. } => drop(reply.send(Ok(reaction_rules()))),
        other => other.unsupported(ctx),
    }
}

fn serve(account: &Arc<Account>, command: Command) {
    let ctx = &account.ctx;
    match command {
        Command::Backfill(id) | Command::Warm(id) => backfill(account, &id),
        Command::History {
            conversation_id, ..
        }
        | Command::HistoryAfter {
            conversation_id, ..
        }
        | Command::HistoryAround {
            conversation_id, ..
        } => {
            backfill(account, &conversation_id);
        }
        Command::MarkRead {
            conversation_id,
            message_id,
        } => {
            if let Err(why) = account.drive.mark_read(&conversation_id, &message_id) {
                eprintln!("chatd: Messages: {why}");
            }
        }
        Command::Send {
            conversation_id,
            client_id,
            text,
            ..
        } => {
            let outcome = account
                .drive
                .send(&conversation_id, &text)
                .map(|id| outgoing(account, id, text));
            ctx.sent(&conversation_id, &client_id, outcome);
        }
        Command::SendWith {
            conversation_id,
            client_id,
            text,
            with,
            ..
        } => {
            let outcome = match with.files.as_slice() {
                [file] => {
                    let name = if file.name.is_empty() {
                        std::path::Path::new(&file.path)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("file")
                            .to_owned()
                    } else {
                        file.name.clone()
                    };
                    let bytes = std::fs::read(&file.path).unwrap_or_default();
                    account
                        .drive
                        .send_file(&conversation_id, &text, &file.path, &name)
                        .map(|id| {
                            let mut message = outgoing(account, id.clone(), text);
                            if !bytes.is_empty() {
                                let mut media = MediaFile::new(
                                    format!("messages:{id}:0"),
                                    kind_of(if name.ends_with(".png") {
                                        "image/png"
                                    } else {
                                        "application/octet-stream"
                                    }),
                                );
                                media.bytes = bytes.len() as u64;
                                if let Some(path) = land(account, &media, &bytes) {
                                    media.path = Some(path);
                                }
                                message.attachments = vec![Attachment {
                                    name: name.clone(),
                                    file: media,
                                    thumb: None,
                                    mime: String::new(),
                                    duration: 0,
                                    spoiler: false,
                                    sticker: false,
                                    waveform: Vec::new(),
                                    round: false,
                                }];
                            }
                            message
                        })
                }
                [] => account
                    .drive
                    .send(&conversation_id, &text)
                    .map(|id| outgoing(account, id, text)),
                _ => Err("Messages takes one file at a time.".into()),
            };
            ctx.sent(&conversation_id, &client_id, outcome);
        }
        Command::React {
            message_id,
            emoji,
            on,
            reply,
            ..
        } => {
            let answer = account
                .drive
                .react(&message_id, &emoji, on)
                .map_err(Refusal::Refused);
            let _ = reply.send(answer);
        }
        Command::Wanted(ids) => wanted(account, &ids),
        Command::Typing(conversation_id) => {
            let _ = account.drive.typing(&conversation_id);
        }
        Command::ReactionRules { reply, .. } => drop(reply.send(Ok(reaction_rules()))),
        Command::Handoff { reply, .. } => {
            let _ = reply.send(Ok("https://messages.google.com/web/".to_owned()));
        }
        Command::SignOut(reply) => {
            account.drive.close();
            let _ = reply.send(Ok(()));
        }
        other => other.unsupported(ctx),
    }
}

fn outgoing(account: &Account, id: String, text: String) -> Message {
    let mut message = Message::plain(id, account.name.clone(), text, crate::hub::now(), true);
    message.sender.id = account.me.clone();
    message
}

fn backfill(account: &Account, conversation: &str) {
    match account.drive.history(conversation) {
        Ok(notes) => {
            for note in notes {
                let _ = account.ctx.message(conversation, &message(account, &note));
            }
        }
        Err(why) => eprintln!("chatd: Messages: {why}"),
    }
}
