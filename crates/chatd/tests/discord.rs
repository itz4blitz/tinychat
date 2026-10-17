//! The Discord adapter against a local fake of Discord: a tiny HTTP server and a gateway
//! websocket that replay hand-written payloads shaped like the documented ones. Each test runs
//! the real chatd binary in a scratch directory and asserts on what its socket then answers.

use base64::Engine;
use protocol::{
    read_frame, write_frame, AccountKey, Call, CallState, CallSupport, Change, Conversation, ConversationKind,
    ErrorCode, Key, Link, MediaKind, MentionKind, Message, MessageKind, Network, Page, Person,
    Presence, ReactionSet, EmojiScope, RelateAction, Relation, Request, Response, SendState, Style, Watching,
};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use protocol::ChatStream;
use std::path::PathBuf;
use std::process::{Child, Command};

fn process_rss_kb(pid: u32) -> u64 {
    #[cfg(windows)]
    {
        let out = Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("(Get-Process -Id {pid}).WorkingSet64")])
            .output()
            .expect("working set");
        let bytes: u64 = String::from_utf8(out.stdout).unwrap().trim().parse().unwrap();
        return bytes / 1024;
    }
    #[cfg(not(windows))]
    {
        let out = Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().unwrap();
        String::from_utf8(out.stdout).unwrap().trim().parse().unwrap()
    }
}
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::CloseFrame;

/// The first part is base64 for the user id 1000, as in a real token.
const TOKEN: &str = "MTAwMA.Zx9aBc.not-a-real-signature";
const READY: &str = include_str!("fixtures/discord/ready.json");
const SUPPLEMENTAL: &str = include_str!("fixtures/discord/ready_supplemental.json");

fn history(channel: &str) -> &'static str {
    match channel {
        "3001" => include_str!("fixtures/discord/messages_3001.json"),
        "4101" => include_str!("fixtures/discord/messages_4101.json"),
        "4500" => include_str!("fixtures/discord/messages_4500.json"),
        _ => "[]",
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

fn now() -> i64 {
    (now_ms() / 1000) as i64
}

/// One HTTP request the adapter made.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Value,
    /// How many bytes the body was, for an upload, which is not JSON.
    bytes: usize,
    at: Instant,
}

/// An answer written out by hand: its status, its header lines, its body.
type Written = (u16, String, String);

enum Control {
    /// Dispatch an event to the connected session.
    Push(String, Value),
    /// Cut the TCP connection with no close frame, as a network drop does.
    Drop,
    /// Close with a gateway close code.
    Close(u16),
}

struct Shared {
    requests: Mutex<Vec<Seen>>,
    /// Every frame the adapter sent on the gateway, in order.
    frames: Mutex<Vec<Value>>,
    control: Mutex<Sender<Control>>,
    /// Answers queued for paths, instead of the usual success.
    scripted: Mutex<HashMap<String, VecDeque<(u16, Value)>>>,
    /// The same for answers that are not JSON: a status, header lines, a body.
    raw: Mutex<HashMap<String, VecDeque<Written>>>,
    /// Every request under these paths is answered 403.
    forbidden: Mutex<Vec<String>>,
    /// What an identify is answered with: READY as text.
    ready: Mutex<String>,
    /// Close codes the next identifies are answered with instead.
    closes: Mutex<VecDeque<u16>>,
    /// The page that names the web client's build: how long it takes, and its status.
    login: Mutex<(Duration, u16)>,
    /// How many times that page has been answered.
    logins: AtomicUsize,
    /// A path whose answer is held back this long.
    stall: Mutex<Option<(String, Duration)>>,
    /// The last sequence number dispatched.
    sequence: AtomicU64,
    connections: AtomicUsize,
    made: AtomicU64,
}

impl Shared {
    /// An id as Discord makes them: the time, then a counter.
    fn snowflake(&self) -> String {
        let id = ((now_ms() - 1_420_070_400_000) << 22) + self.made.fetch_add(1, Ordering::Relaxed);
        id.to_string()
    }

    fn push(&self, event: &str, data: Value) {
        self.control.lock().unwrap().send(Control::Push(event.to_owned(), data)).unwrap();
    }

    fn frames_of(&self, op: u64) -> Vec<Value> {
        self.frames.lock().unwrap().iter().filter(|frame| frame["op"] == op).cloned().collect()
    }

    fn requests_to(&self, method: &str, path: &str) -> Vec<Seen> {
        let seen = self.requests.lock().unwrap();
        seen.iter().filter(|seen| seen.method == method && seen.path.starts_with(path)).cloned().collect()
    }

    fn script(&self, path: &str, status: u16, body: Value) {
        self.scripted.lock().unwrap().entry(path.to_owned()).or_default().push_back((status, body));
    }

    fn script_raw(&self, path: &str, status: u16, headers: &str, body: &str) {
        let answer = (status, headers.to_owned(), body.to_owned());
        self.raw.lock().unwrap().entry(path.to_owned()).or_default().push_back(answer);
    }

    /// Every HTTP request so far, as `METHOD path`.
    fn asked(&self) -> Vec<String> {
        self.requests.lock().unwrap().iter().map(|seen| format!("{} {}", seen.method, seen.path)).collect()
    }
}

/// `%F0%9F%91%8D` back into the emoji it names.
fn unescaped(text: &str) -> String {
    let mut bytes = Vec::new();
    let mut rest = text.as_bytes();
    while let Some((&first, after)) = rest.split_first() {
        match (first, after.get(..2).and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok())) {
            (b'%', Some(byte)) => {
                bytes.push(byte);
                rest = &after[2..];
            }
            _ => {
                bytes.push(first);
                rest = after;
            }
        }
    }
    String::from_utf8(bytes).unwrap()
}

/// The emoji object of a reaction event, from how the path names it.
fn emoji_of(part: &str) -> Value {
    let named = unescaped(part);
    match named.split_once(':') {
        Some((name, id)) => json!({"id": id, "name": name}),
        None => json!({"id": null, "name": named}),
    }
}

fn me() -> Value {
    json!({"id": "1000", "username": "justin", "global_name": "Justin", "discriminator": "0", "avatar": "me0hash"})
}

/// What Discord's HTTP API answers. Anything not listed is a 404, so an unexpected request
/// shows up as a failure rather than passing silently.
fn respond(shared: &Shared, seen: &Seen) -> (u16, Value) {
    let path = seen.path.split('?').next().unwrap();
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    if let Some(answer) = shared.scripted.lock().unwrap().get_mut(path).and_then(VecDeque::pop_front) {
        return answer;
    }
    if shared.forbidden.lock().unwrap().iter().any(|under| path.starts_with(under.as_str())) {
        return (403, json!({"message": "Missing Access", "code": 50001}));
    }
    match (seen.method.as_str(), parts.as_slice()) {
        ("PUT", ["channels", channel, "messages", message, "reactions", emoji, "%40me"]) => {
            let event = json!({"user_id": "1000", "channel_id": channel, "message_id": message, "emoji": emoji_of(emoji), "type": 0});
            shared.push("MESSAGE_REACTION_ADD", event);
            (204, Value::Null)
        }
        ("DELETE", ["channels", channel, "messages", message, "reactions", emoji, "0", "%40me"]) => {
            let event = json!({"user_id": "1000", "channel_id": channel, "message_id": message, "emoji": emoji_of(emoji), "type": 0});
            shared.push("MESSAGE_REACTION_REMOVE", event);
            (204, Value::Null)
        }
        ("PATCH", ["channels", channel, "messages", message]) => {
            let edited = json!({
                "id": message, "channel_id": channel, "type": 0, "author": me(), "content": seen.body["content"],
                "timestamp": "2026-10-13T08:15:00.000000+00:00", "edited_timestamp": "2026-10-13T08:16:00.000000+00:00",
                "mentions": [], "mention_roles": [], "mention_everyone": false, "attachments": [], "embeds": [], "pinned": false,
            });
            shared.push("MESSAGE_UPDATE", edited.clone());
            (200, edited)
        }
        ("DELETE", ["channels", channel, "messages", message]) => {
            shared.push("MESSAGE_DELETE", json!({"id": message, "channel_id": channel}));
            (204, Value::Null)
        }
        ("PUT", ["users", "@me", "relationships", person]) => {
            let kind = seen.body["type"].as_u64().unwrap_or(1);
            let user = json!({"id": person, "username": "asker", "global_name": "Asker", "discriminator": "0", "avatar": null});
            shared.push("RELATIONSHIP_ADD", json!({"id": person, "type": kind, "nickname": null, "user": user}));
            (204, Value::Null)
        }
        ("DELETE", ["users", "@me", "relationships", person]) => {
            shared.push("RELATIONSHIP_REMOVE", json!({"id": person, "type": 1, "nickname": null}));
            (204, Value::Null)
        }
        ("PATCH", ["users", "@me", "settings-proto", "1"]) => (200, json!({"settings": seen.body["settings"]})),
        ("GET", ["users", person, "profile"]) => {
            let mut profile = json!({
                "user": {"id": person, "username": "sam", "global_name": "Sam Whitlock", "discriminator": "0", "avatar": "s4m", "bio": ""},
                "user_profile": {"bio": "Maintains the build. **Tea**, not coffee.", "pronouns": ""},
                "mutual_guilds": [{"id": "4000", "nick": null}, {"id": "7777", "nick": null}],
                "connected_accounts": [], "premium_since": null,
            });
            // Asked from inside a server: their membership there.
            if seen.path.contains("guild_id=4000") {
                profile["user_profile"]["pronouns"] = json!("they/them");
                profile["guild_member"] = json!({"nick": "Sammy", "roles": ["4010", "4000"], "joined_at": "2023-03-01T12:00:00+00:00"});
            }
            (200, profile)
        }
        ("PATCH", ["users", "@me", "guilds", guild, "settings"]) => {
            // The server's settings whole, with the override asked for, as Discord answers.
            let overrides = seen.body["channel_overrides"].as_object().cloned().unwrap_or_default();
            let overrides: Vec<Value> = overrides
                .into_iter()
                .map(|(id, mut setting)| {
                    setting["channel_id"] = json!(id);
                    setting
                })
                .collect();
            let guild_id = if *guild == "@me" { Value::Null } else { json!(guild) };
            (200, json!({"guild_id": guild_id, "muted": false, "suppress_everyone": false, "suppress_roles": false, "message_notifications": 0, "flags": 0, "channel_overrides": overrides}))
        }
        ("DELETE", ["channels", channel]) => (200, json!({"id": channel, "type": 1})),
        ("GET", ["channels", channel, "messages", "pins"]) => {
            let pinned = json!({
                "id": "1300000000000000700", "channel_id": channel, "type": 0, "author": me(), "content": "Read the rules first",
                "timestamp": "2026-10-13T08:15:00.000000+00:00", "edited_timestamp": null, "mentions": [], "mention_roles": [],
                "mention_everyone": false, "attachments": [], "embeds": [], "pinned": true,
            });
            (200, json!({"items": [{"pinned_at": "2026-10-13T09:00:00.000000+00:00", "message": pinned}], "has_more": false}))
        }
        ("GET", ["guilds", _, "messages", "search"]) | ("GET", ["channels", _, "messages", "search"]) => {
            let channel = if parts[0] == "channels" { parts[1].to_owned() } else { "4101".to_owned() };
            let hit = json!({
                "id": "1300000000000000600", "channel_id": channel, "type": 0, "author": me(), "content": "the map",
                "timestamp": "2026-10-13T08:15:00.000000+00:00", "edited_timestamp": null, "mentions": [], "mention_roles": [],
                "mention_everyone": false, "embeds": [], "pinned": false,
                "attachments": [{"id": "9001", "filename": "map.png", "size": 2048, "url": "https://cdn.discordapp.com/attachments/1/9001/map.png",
                                 "proxy_url": "https://media.discordapp.net/attachments/1/9001/map.png", "width": 640, "height": 480, "content_type": "image/png"}],
            });
            (200, json!({"analytics_id": "a", "doing_deep_historical_index": false, "total_results": 37, "messages": [[hit]]}))
        }
        ("GET", ["guilds", guild]) => (
            200,
            json!({"id": guild, "name": "Rustaceans", "description": "Crabs and their friends", "approximate_member_count": 1204, "approximate_presence_count": 230}),
        ),
        ("GET", ["gifs", "search" | "trending-gifs"]) => (
            200,
            json!([
                {"id": "1", "title": "cat typing", "url": "https://tenor.com/view/cat-typing-1", "src": "https://media.tenor.com/1AAAM/cat-typing.gif",
                 "gif_src": "https://media.tenor.com/1.gif", "preview": "https://media.tenor.com/1.png", "width": 220, "height": 160},
            ]),
        ),
        ("POST", ["channels", _, "attachments"]) => {
            let host = seen.headers.get("host").cloned().unwrap_or_default();
            let files = seen.body["files"].as_array().cloned().unwrap_or_default();
            let slots: Vec<Value> = files
                .iter()
                .map(|file| {
                    let name = format!("up/{}", file["filename"].as_str().unwrap());
                    json!({"id": file["id"], "upload_url": format!("http://{host}/upload/{name}"), "upload_filename": name})
                })
                .collect();
            (200, json!({"attachments": slots}))
        }
        ("PUT", ["upload", ..]) => (200, Value::Null),
        ("DELETE", ["attachments", ..]) => (204, Value::Null),
        // A fresh signature for each address asked about; one that names `lost` is not renewed.
        ("POST", ["attachments", "refresh-urls"]) => {
            let asked = seen.body["attachment_urls"].as_array().cloned().unwrap_or_default();
            let renewed = asked.iter().filter_map(Value::as_str).filter(|url| !url.contains("lost")).map(|url| {
                json!({"original": url, "refreshed": url.replace("ex=1&", "ex=7fffffff&")})
            });
            (200, json!({"refreshed_urls": renewed.collect::<Vec<_>>()}))
        }
        ("POST", ["auth", "logout"]) => (204, Value::Null),
        ("GET", ["channels", channel, "messages"]) => (200, serde_json::from_str(history(channel)).unwrap()),
        ("POST", ["channels", channel, "messages"]) => {
            let mut message = json!({
                "id": shared.snowflake(),
                "channel_id": channel,
                "type": 0,
                "author": me(),
                "content": seen.body["content"],
                "nonce": seen.body["nonce"],
                "timestamp": "2026-10-13T08:15:00.000000+00:00",
                "edited_timestamp": null,
                "mentions": [],
                "mention_roles": [],
                "mention_everyone": false,
                "attachments": [],
                "embeds": [],
                "pinned": false,
                "tts": false,
                "flags": 0,
            });
            if !seen.body["message_reference"].is_null() {
                message["type"] = json!(19);
                message["message_reference"] = seen.body["message_reference"].clone();
            }
            message["flags"] = seen.body["flags"].clone();
            // Uploaded files become attachments under the name they were sent as.
            let sent = seen.body["attachments"].as_array().cloned().unwrap_or_default();
            let attached: Vec<Value> = sent
                .iter()
                .enumerate()
                .map(|(at, file)| {
                    json!({"id": format!("880{at}"), "filename": file["filename"], "size": 3, "url": format!("https://cdn.discordapp.com/attachments/1/880{at}/f"),
                           "content_type": "text/plain"})
                })
                .collect();
            message["attachments"] = json!(attached);
            if let Some(stickers) = seen.body["sticker_ids"].as_array() {
                let items: Vec<Value> = stickers.iter().map(|id| json!({"id": id, "name": "wave", "format_type": 1})).collect();
                message["sticker_items"] = json!(items);
            }
            // The gateway tells every session, the sender's included.
            shared.push("MESSAGE_CREATE", message.clone());
            (200, message)
        }
        ("POST", ["channels", _, "messages", _, "ack"]) => (200, json!({"token": null})),
        ("POST", ["channels", _, "typing"]) => (204, Value::Null),
        ("POST", ["channels", _, "call", "stop-ringing"]) => (204, Value::Null),
        ("POST", ["users", "@me", "channels"]) => {
            let zoe = json!({"id": "2050", "username": "zoe", "global_name": "Zoe Lin", "discriminator": "0", "avatar": null});
            (200, json!({"id": "3050", "type": 1, "last_message_id": null, "recipients": [zoe], "flags": 0}))
        }
        _ => (404, json!({"message": "404: Not Found", "code": 0})),
    }
}

fn serve_http(shared: Arc<Shared>, stream: TcpStream) {
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut request = line.split_whitespace();
        let (method, path) = (request.next().unwrap().to_owned(), request.next().unwrap().to_owned());
        let mut headers = HashMap::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            let Some((name, value)) = header.trim_end().split_once(':') else {
                break;
            };
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        let length: usize = headers.get("content-length").map_or(0, |length| length.parse().unwrap());
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let seen = Seen {
            method,
            path,
            headers,
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
            bytes: body.len(),
            at: Instant::now(),
        };
        shared.requests.lock().unwrap().push(seen.clone());
        let bare = seen.path.split('?').next().unwrap().to_owned();
        // What is not Discord's JSON: a file from the media proxy, the page that names the
        // build, and whatever a test wrote out by hand.
        let other = if bare.starts_with("/media/") {
            // An address signed `ex=1` ran out long ago.
            Some(match seen.path.contains("ex=1&") || bare.contains("lost") {
                true => (404, String::new(), String::new()),
                false => (200, "Content-Type: image/webp\r\n".to_owned(), "x".repeat(64 + seen.path.len())),
            })
        } else if bare == "/login" {
            let (takes, status) = *shared.login.lock().unwrap();
            sleep(takes);
            shared.logins.fetch_add(1, Ordering::Relaxed);
            let page = r#"<script>window.GLOBAL_ENV = {"API_ENDPOINT":"//discord.com/api","BUILD_NUMBER":"987654","RELEASE_CHANNEL":"stable"}</script>"#;
            Some((status, "Content-Type: text/html\r\n".to_owned(), page.to_owned()))
        } else {
            shared.raw.lock().unwrap().get_mut(&bare).and_then(VecDeque::pop_front)
        };
        if let Some((status, lines, body)) = other {
            let head = format!("HTTP/1.1 {status} Fake\r\n{lines}Content-Length: {}\r\n\r\n", body.len());
            let stream = reader.get_mut();
            if stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(body.as_bytes())).is_err() {
                return;
            }
            continue;
        }
        let stalled = shared.stall.lock().unwrap().clone().filter(|(path, _)| *path == bare);
        if let Some((_, takes)) = stalled {
            sleep(takes);
        }
        let (status, answer) = respond(&shared, &seen);
        let answer = if status == 204 { String::new() } else { answer.to_string() };
        let head = format!(
            "HTTP/1.1 {status} Fake\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             X-RateLimit-Limit: 5\r\nX-RateLimit-Remaining: 4\r\nX-RateLimit-Reset-After: 1.000\r\n\
             X-RateLimit-Bucket: fake\r\n\r\n",
            answer.len()
        );
        let stream = reader.get_mut();
        if stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(answer.as_bytes())).is_err() {
            return;
        }
    }
}

/// One gateway connection: hello, then READY for an identify or RESUMED for a resume, a
/// heartbeat answered with an ack, and whatever the test pushes.
fn serve_gateway(shared: &Shared, stream: TcpStream, control: &Receiver<Control>, gateway: &str) {
    let mut socket = tungstenite::accept(stream).unwrap();
    socket.get_ref().set_read_timeout(Some(Duration::from_millis(15))).unwrap();
    shared.connections.fetch_add(1, Ordering::Relaxed);
    let dispatch = |socket: &mut tungstenite::WebSocket<TcpStream>, event: &str, data: Value| {
        let s = shared.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        socket.send(json!({"op": 0, "t": event, "s": s, "d": data}).to_string().into())
    };
    socket.send(json!({"op": 10, "d": {"heartbeat_interval": 1500}}).to_string().into()).unwrap();
    loop {
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                let mut frame: Value = serde_json::from_str(text.as_str()).unwrap();
                // When it arrived, for the tests that count how often something is sent.
                frame["_at"] = json!(now_ms());
                shared.frames.lock().unwrap().push(frame.clone());
                let sent = match frame["op"].as_u64() {
                    Some(1) => socket.send(json!({"op": 11}).to_string().into()),
                    Some(2) => {
                        assert_eq!(frame["d"]["token"], TOKEN, "identify carries the session's token");
                        // A queued 0 lets that identify through.
                        let close = shared.closes.lock().unwrap().pop_front().filter(|code| *code != 0);
                        if let Some(code) = close {
                            let _ = socket.close(Some(CloseFrame { code: CloseCode::from(code), reason: "Session timed out.".into() }));
                            let until = Instant::now() + Duration::from_secs(2);
                            while Instant::now() < until && !matches!(socket.read(), Err(tungstenite::Error::ConnectionClosed)) {}
                            return;
                        }
                        // Written out as text, so a READY of real size is sent as it is.
                        let ready = shared.ready.lock().unwrap().replace("{{GATEWAY}}", gateway);
                        let s = shared.sequence.fetch_add(1, Ordering::Relaxed) + 1;
                        socket
                            .send(format!(r#"{{"op":0,"t":"READY","s":{s},"d":{ready}}}"#).into())
                            .and_then(|()| dispatch(&mut socket, "READY_SUPPLEMENTAL", serde_json::from_str(SUPPLEMENTAL).unwrap()))
                    }
                    Some(6) => dispatch(&mut socket, "RESUMED", json!({"_trace": []})),
                    _ => Ok(()),
                };
                if sent.is_err() {
                    return;
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(err)) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return,
        }
        while let Ok(control) = control.try_recv() {
            match control {
                Control::Push(event, data) => {
                    if dispatch(&mut socket, &event, data).is_err() {
                        return;
                    }
                }
                Control::Drop => return,
                Control::Close(code) => {
                    let frame = CloseFrame {
                        code: CloseCode::from(code),
                        reason: "Authentication failed.".into(),
                    };
                    let _ = socket.close(Some(frame));
                    let until = Instant::now() + Duration::from_secs(2);
                    while Instant::now() < until && !matches!(socket.read(), Err(tungstenite::Error::ConnectionClosed)) {}
                    return;
                }
            }
        }
    }
}

/// mediad is another package's binary, so cargo does not build it for these tests.
fn mediad() -> PathBuf {
    let chatd = std::path::Path::new(env!("CARGO_BIN_EXE_chatd"));
    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let built = Command::new(cargo).args(["build", "-j", "6", "-p", "mediad", "--bin", "mediad"]).status();
        assert!(built.unwrap().success(), "could not build mediad");
    });
    chatd.with_file_name(format!("mediad{}", std::env::consts::EXE_SUFFIX))
}

/// The fake Discord and a real chatd pointed at it.
struct World {
    child: Child,
    dir: PathBuf,
    discord: Arc<Shared>,
    /// Where the fake's HTTP server is, for addresses a payload names.
    api: String,
    gateway: String,
    env: Vec<(String, String)>,
}

impl World {
    fn start() -> World {
        let world = World::launch(&[], |_| {});
        wait("the account to connect", || (world.link() == Link::Connected).then_some(()));
        world
    }

    fn spawn(&self) -> Child {
        Command::new(env!("CARGO_BIN_EXE_chatd"))
            .env("TINYCLIENT_DIR", &self.dir)
            .env("TINYCLIENT_DISCORD_API", &self.api)
            .env("TINYCLIENT_DISCORD_GATEWAY", format!("{}/?encoding=json&v=9", self.gateway))
            .env("TINYCLIENT_SESSION_DISCORD", json!({"token": TOKEN}).to_string())
            .env_remove("TINYCLIENT_SESSION_SLACK")
            // The fake stands in for the CDN and the media proxy too: nothing here reaches Discord.
            .env("TINYCLIENT_DISCORD_CDN", format!("{}/media", self.api))
            .env("TINYCLIENT_MEDIA_HOSTS", "127.0.0.1")
            .env("TINYCLIENT_MEDIAD", mediad())
            .envs(self.env.iter().map(|(name, value)| (name.as_str(), value.as_str())))
            .spawn()
            .unwrap()
    }

    /// Stops chatd and starts it again on the same directory.
    fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(self.dir.join("chat.sock"));
        self.child = self.spawn();
        wait("chatd to open its socket again", || ChatStream::connect(&self.dir.join("chat.sock")).ok());
    }

    /// The fake, prepared by `prepare` before chatd starts, and chatd with `env` added. Does
    /// not wait for the account to connect.
    fn launch(env: &[(&str, &str)], prepare: impl FnOnce(&Shared)) -> World {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // Short path: a unix socket address holds about 100 bytes. On Windows "/tmp" has no drive; chatd takes only absolute paths.
        let dir = std::path::absolute(format!("/tmp/tinyclient-dc-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (control, controlled) = channel();
        let shared = Arc::new(Shared {
            requests: Mutex::new(Vec::new()),
            frames: Mutex::new(Vec::new()),
            control: Mutex::new(control),
            scripted: Mutex::new(HashMap::new()),
            raw: Mutex::new(HashMap::new()),
            forbidden: Mutex::new(Vec::new()),
            ready: Mutex::new(READY.to_owned()),
            closes: Mutex::new(VecDeque::new()),
            login: Mutex::new((Duration::ZERO, 200)),
            logins: AtomicUsize::new(0),
            stall: Mutex::new(None),
            sequence: AtomicU64::new(0),
            connections: AtomicUsize::new(0),
            made: AtomicU64::new(0),
        });
        let http = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = format!("http://{}", http.local_addr().unwrap());
        let for_http = shared.clone();
        std::thread::spawn(move || {
            for stream in http.incoming().flatten() {
                let shared = for_http.clone();
                std::thread::spawn(move || serve_http(shared, stream));
            }
        });
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let gateway = format!("ws://{}", socket.local_addr().unwrap());
        let (for_gateway, address) = (shared.clone(), gateway.clone());
        std::thread::spawn(move || {
            for stream in socket.incoming().flatten() {
                serve_gateway(&for_gateway, stream, &controlled, &address);
            }
        });

        prepare(&shared);
        let env = env.iter().map(|(name, value)| ((*name).to_owned(), value.replace("{{API}}", &api))).collect();
        // A placeholder child until the real one is spawned from the finished world.
        let mut world = World {
            child: Command::new("true").spawn().unwrap(),
            dir,
            discord: shared,
            api,
            gateway,
            env,
        };
        let _ = world.child.wait();
        world.child = world.spawn();
        wait("chatd to open its socket", || ChatStream::connect(&world.dir.join("chat.sock")).ok());
        world
    }

    /// What the account says stands in the way, if anything.
    fn required(&self) -> String {
        match self.ask(Request::Status) {
            Response::Status { accounts, .. } => {
                accounts.into_iter().find(|account| account.key == self.account()).map_or_else(String::new, |account| account.required)
            }
            other => panic!("{other:?}"),
        }
    }

    /// Asks for files and answers what is known at once.
    fn fetch(&self, ids: &[&str], size: Option<u32>, retry: bool) -> Vec<protocol::MediaFile> {
        let ids = ids.iter().map(|id| (*id).to_owned()).collect();
        match self.ask(Request::Fetch { account: self.account(), ids, size, retry }) {
            Response::Media { files } => files,
            other => panic!("{other:?}"),
        }
    }

    /// The file once it has landed or failed.
    fn fetched(&self, id: &str, size: Option<u32>, retry: bool) -> protocol::MediaFile {
        self.fetch(&[id], size, retry);
        wait("the file to land or fail", || {
            self.fetch(&[id], size, false).pop().filter(|file| file.path.is_some() || file.failed.is_some())
        })
    }

    fn ask(&self, request: Request) -> Response {
        let mut stream = ChatStream::connect(&self.dir.join("chat.sock")).unwrap();
        write_frame(&mut stream, &request).unwrap();
        read_frame(&mut stream).unwrap()
    }

    fn account(&self) -> AccountKey {
        AccountKey {
            network: Network::Discord,
            account: "1000".to_owned(),
        }
    }

    fn key(&self, id: &str) -> Key {
        Key::new(&self.account(), id)
    }

    fn status(&self) -> (Link, String, u32, bool, u64) {
        match self.ask(Request::Status) {
            Response::Status { accounts, seq, .. } => {
                let ours = accounts.into_iter().find(|account| account.key == self.account());
                ours.map_or((Link::SignedOut, String::new(), 0, false, seq), |account| {
                    (account.link, account.detail, account.attention, account.unread, seq)
                })
            }
            other => panic!("{other:?}"),
        }
    }

    fn link(&self) -> Link {
        self.status().0
    }

    fn conversation(&self, id: &str) -> Conversation {
        match self.ask(Request::Conversation { conversation: self.key(id) }) {
            Response::Conversation { conversation } => conversation,
            other => panic!("conversation {id}: {other:?}"),
        }
    }

    /// The newest page as stored. Asking also makes chatd bring the conversation up to date.
    fn messages(&self, id: &str) -> Vec<Message> {
        let asked = Request::Messages {
            conversation: self.key(id),
            page: Page::Newest,
            limit: 100,
        };
        match self.ask(asked) {
            Response::Messages { messages, .. } => messages,
            other => panic!("messages {id}: {other:?}"),
        }
    }

    fn tree(&self, space: &str) -> (Vec<String>, Vec<Conversation>) {
        let asked = Request::Tree {
            space: self.key(space),
            offset: 0,
            limit: 500,
            peek: false,
        };
        match self.ask(asked) {
            Response::Tree { groups, conversations, .. } => (groups.into_iter().map(|group| group.name).collect(), conversations),
            other => panic!("tree {space}: {other:?}"),
        }
    }

    fn people(&self, relation: Option<Relation>, query: Option<&str>) -> Vec<Person> {
        let asked = Request::People {
            network: Some(Network::Discord),
            relation,
            query: query.map(str::to_owned),
            offset: 0,
            limit: 100,
        };
        match self.ask(asked) {
            Response::People { people, .. } => people,
            other => panic!("{other:?}"),
        }
    }

    fn send(&self, conversation: &str, client_id: &str, text: &str, reply_to: Option<&str>) -> Response {
        self.ask(Request::Send {
            conversation: self.key(conversation),
            client_id: client_id.to_owned(),
            text: text.to_owned(),
            reply_to: reply_to.map(str::to_owned), with: Default::default(),
        })
    }

    /// The message a send became, once it is no longer pending.
    fn outcome(&self, conversation: &str, client_id: &str) -> Message {
        wait("the send to settle", || {
            let messages = self.messages(conversation);
            let ours = messages.into_iter().find(|message| message.client_id.as_deref() == Some(client_id));
            ours.filter(|message| message.state != SendState::Pending)
        })
    }

    fn failure(&self, conversation: &str, client_id: &str) -> String {
        match self.outcome(conversation, client_id).state {
            SendState::Failed { reason } => reason,
            other => panic!("{client_id} was {other:?}"),
        }
    }

    /// A message from someone else, as the gateway dispatches one.
    fn incoming(&self, channel: &str, guild: Option<&str>, text: &str, extra: Value) -> String {
        let id = self.discord.snowflake();
        let mut message = json!({
            "id": id,
            "channel_id": channel,
            "type": 0,
            "author": {"id": "2006", "username": "sam", "global_name": "Sam Whitlock", "discriminator": "0", "avatar": "s4m"},
            "content": text,
            "timestamp": "2026-10-13T08:15:00.000000+00:00",
            "edited_timestamp": null,
            "mentions": [],
            "mention_roles": [],
            "mention_everyone": false,
            "attachments": [],
            "embeds": [],
            "pinned": false,
        });
        if let Some(guild) = guild {
            message["guild_id"] = json!(guild);
            message["member"] = json!({"nick": null, "roles": []});
        }
        for (name, value) in extra.as_object().into_iter().flatten() {
            message[name] = value.clone();
        }
        self.discord.push("MESSAGE_CREATE", message);
        id
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_dir_all(self.dir.with_extension("files"));
    }
}

/// Lets this account tell Discord that we type (P1, off by default).
fn allow_typing(world: &World, conversation: &Key) {
    let scope = protocol::settings::account_scope(&conversation.account());
    let set = Request::Set { scope, key: protocol::settings::TYPING.into(), value: Some("on".into()) };
    assert_eq!(world.ask(set), Response::Ok);
}

/// Polls until `probe` has an answer. Everything here is the adapter reacting to the fake, so
/// nothing is assumed to have happened by the time a request returns.
fn wait<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(20));
    }
}

fn spanned(message: &Message, wanted: impl Fn(&Style) -> bool) -> Vec<&str> {
    let matching = message.spans.iter().filter(|span| wanted(&span.style));
    matching.map(|span| &message.text[span.start as usize..span.end as usize]).collect()
}

fn ids(conversations: &[Conversation]) -> Vec<&str> {
    conversations.iter().map(|conversation| conversation.key.id.as_str()).collect()
}

#[test]
fn ready_fills_spaces_trees_people_and_unread_state() {
    let world = World::start();

    // The session says it is the web client for this OS, the same on the socket and over HTTP.
    let identify = world.discord.frames_of(2).remove(0);
    let properties = &identify["d"]["properties"];
    let agent = properties["browser_user_agent"].as_str().unwrap();
    assert_eq!(agent, chatd::adapters::net::browser());
    #[cfg(target_os = "macos")]
    {
        assert_eq!(properties["browser"], "Safari");
        assert_eq!(properties["os"], "Mac OS X");
    }
    #[cfg(target_os = "windows")]
    {
        assert_eq!(properties["browser"], "Chrome");
        assert_eq!(properties["os"], "Windows");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        assert_eq!(properties["browser"], "Chrome");
        assert_eq!(properties["os"], "Linux");
    }
    assert_eq!(identify["d"]["compress"], false);
    assert!(identify["d"]["capabilities"].as_u64().unwrap() > 0);

    match world.ask(Request::Status) {
        Response::Status { accounts, .. } => {
            assert_eq!(accounts.len(), 1);
            assert_eq!((accounts[0].name.as_str(), accounts[0].me.as_str()), ("Justin", "1000"));
        }
        other => panic!("{other:?}"),
    }

    // Servers in the user's own order (the settings put Zed first), each with its state.
    let spaces = match world.ask(Request::Spaces { network: Some(Network::Discord) }) {
        Response::Spaces { spaces } => spaces,
        other => panic!("{other:?}"),
    };
    let names: Vec<&str> = spaces.iter().map(|space| space.name.as_str()).collect();
    assert_eq!(names, ["Zed", "Rustaceans"]);
    let rust = &spaces[1];
    assert_eq!(rust.icon.as_ref().map(|icon| (icon.id.as_str(), icon.kind)), Some(("icon:4000:gu1ldicon", MediaKind::Icon)));
    assert_eq!((rust.attention, rust.unread), (1, true), "one mention in #general, unread channels beside it");
    // Zed is muted as a whole: its unread channel raises nothing.
    assert_eq!((spaces[0].attention, spaces[0].unread), (0, false));

    // The tree: categories and channels in Discord's order, hidden ones left out.
    let (groups, rows) = world.tree("4000");
    assert_eq!(groups, ["Text", "Voice"], "a category whose channels are all hidden is not listed");
    assert_eq!(ids(&rows), ["4105", "4101", "4500", "4102", "4104", "4106", "4201"]);
    let row = |id: &str| rows.iter().find(|row| row.key.id == id).unwrap();
    let kinds: Vec<ConversationKind> = rows.iter().map(|row| row.kind).collect();
    use ConversationKind::{Announcement, Channel, Forum, Thread, Voice};
    assert_eq!(kinds, [Channel, Channel, Thread, Announcement, Channel, Forum, Voice]);
    assert_eq!(row("4105").group, None);
    assert_eq!(row("4101").group.as_deref(), Some("4100"));
    assert_eq!(row("4500").group.as_deref(), Some("4100"), "a thread sits under its channel");
    assert_eq!(row("4201").group.as_deref(), Some("4200"));
    assert_eq!(row("4201").call, CallSupport::Native);
    // #staff is denied to everyone and allowed to a role we lack; #mods-only to one we hold.
    assert!(!ids(&rows).contains(&"4103") && !ids(&rows).contains(&"4301"));
    let state = |id: &str| (row(id).unread, row(id).mentions, row(id).muted);
    assert_eq!(state("4101"), (1, 1, false));
    assert_eq!(state("4102"), (0, 0, false), "read up to its newest message");
    assert_eq!(state("4104"), (1, 0, false));
    assert_eq!(state("4105"), (1, 0, true), "muted in Discord's own settings");
    assert_eq!(state("4500"), (0, 0, false));
    assert_eq!(row("4101").last_read.as_deref(), Some("1300000000000000090"));
    // We own Zed, so the overwrite that hides its channel from everyone does not apply.
    let (_, zed) = world.tree("5000");
    assert_eq!(ids(&zed), ["5001"]);
    assert_eq!((zed[0].unread, zed[0].muted), (1, true));

    // Direct and group conversations, and the channel that mentions us.
    let inbox = match world.ask(Request::Inbox { network: None, filter: Default::default(), offset: 0, limit: 50 }) {
        Response::Conversations { conversations, .. } => conversations,
        other => panic!("{other:?}"),
    };
    let inbox_row = |id: &str| inbox.iter().find(|row| row.key.id == id).unwrap_or_else(|| panic!("{id} not in the inbox"));
    let mara = inbox_row("3001");
    assert_eq!((mara.kind, mara.title.as_str(), mara.peer.as_deref()), (ConversationKind::Direct, "Mara O", Some("2001")));
    assert_eq!((mara.unread, mara.mentions, mara.muted), (2, 0, false));
    assert_eq!(mara.avatar.as_ref().map(|avatar| avatar.id.as_str()), Some("avatar:2001:a1b2c3"));
    assert_eq!(mara.call, CallSupport::Native);
    // Sam's last message is from 2023 and nothing is unread: quiet for over 30 days, so out of
    // the Inbox (polish rule I2), though the row is kept.
    assert!(inbox.iter().all(|row| row.key.id != "3002"), "a quiet conversation is in the inbox");
    assert_eq!((world.conversation("3002").title.as_str(), world.conversation("3002").unread), ("Sam Whitlock", 0));
    assert!(inbox.iter().all(|row| row.key.id != "3003"), "a quiet group is in the inbox");
    assert_eq!((world.conversation("3003").kind, world.conversation("3003").title.as_str()), (ConversationKind::Group, "Trip"));
    assert_eq!((inbox_row("3004").unread, inbox_row("3004").muted), (1, true));
    assert_eq!(inbox_row("4101").mentions, 1);
    // Two unread from Mara and one mention; the muted conversation adds nothing.
    let (_, _, attention, unread, _) = world.status();
    assert_eq!((attention, unread), (3, true));

    // Friends and requests, with presence from the frame after READY.
    wait("friends' presence", || {
        let people = world.people(Some(Relation::Friend), None);
        people.iter().any(|person| person.presence == Presence::Online && person.status == "shipping").then_some(())
    });
    let owned = world.people(None, None);
    let names: Vec<&str> = owned.iter().map(|person| person.name.as_str()).collect();
    assert_eq!(names, ["Asked Person", "asker", "Dev Patel", "Mara O"], "blocked people are not in the list");
    let friend = &owned[3];
    assert_eq!((friend.relation, friend.presence, friend.handle.as_str()), (Relation::Friend, Presence::Online, "mara"));
    assert_eq!(friend.avatar.as_ref().map(|avatar| avatar.id.as_str()), Some("avatar:2001:a1b2c3"));
    assert_eq!((owned[2].relation, owned[2].presence), (Relation::Friend, Presence::Idle));
    // No picture of their own: the default one Discord derives from the id.
    assert_eq!(owned[2].avatar.as_ref().map(|avatar| avatar.id.as_str()), Some("avatar:default:0"));
    assert_eq!((owned[1].relation, owned[0].relation), (Relation::Incoming, Relation::Outgoing));
    let blocked = world.people(Some(Relation::Blocked), None);
    assert_eq!(blocked.iter().map(|person| person.name.as_str()).collect::<Vec<_>>(), ["blockedone"]);

    // Pictures are registered, so the shell can ask for them.
    let wanted = vec!["avatar:2001:a1b2c3".to_owned(), "icon:4000:gu1ldicon".to_owned()];
    match world.ask(Request::Fetch { retry: false, size: None, account: world.account(), ids: wanted }) {
        Response::Media { files } => {
            assert_eq!(files.len(), 2);
            assert_eq!((files[0].kind, files[0].width), (MediaKind::Avatar, 80));
        }
        other => panic!("{other:?}"),
    }

    // Connecting fetches no history beyond the one unread, unmuted direct conversation.
    sleep(Duration::from_secs(4));
    let fetched = world.discord.requests_to("GET", "/channels/");
    let paths: Vec<&str> = fetched.iter().map(|seen| seen.path.as_str()).collect();
    assert_eq!(paths, ["/channels/3001/messages?limit=50"]);
    // The HTTP identity is the socket's.
    let headers = &fetched[0].headers;
    assert_eq!(headers["authorization"], TOKEN);
    assert_eq!(headers["user-agent"], agent);
    let decoded = base64::engine::general_purpose::STANDARD.decode(&headers["x-super-properties"]).unwrap();
    let sent: Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(&sent, properties);
}

#[test]
fn messages_map_to_the_one_model_and_follow_live_events() {
    let world = World::start();
    let dm = "3001";

    // The page Discord answers with, however it got asked for.
    let messages = wait("history of the conversation", || Some(world.messages(dm)).filter(|messages| messages.len() == 5));
    let rich = messages.last().unwrap();
    assert_eq!(rich.id, "1300000010000000010");
    assert_eq!(rich.text, "Deploy is green, see https://status.example/x and ask @Dev Patel :tada: :ferris:\nfn main() {}");
    assert_eq!(rich.sender.name, "Mara O", "a friend goes by the nickname we gave them");
    assert_eq!(rich.sender.id, "2001");
    assert_eq!(rich.sent, 1_730_014_555);
    assert!(!rich.outgoing && rich.pinned && !rich.edited);
    assert_eq!(spanned(rich, |style| *style == Style::Bold), ["Deploy"]);
    assert_eq!(spanned(rich, |style| *style == Style::Italic), ["green"]);
    assert_eq!(spanned(rich, |style| matches!(style, Style::Link { url } if url == "https://status.example/x")), ["https://status.example/x"]);
    let named = |style: &Style| matches!(style, Style::Mention { kind: MentionKind::User, id } if id == "2002");
    assert_eq!(spanned(rich, named), ["@Dev Patel"]);
    assert_eq!(spanned(rich, |style| matches!(style, Style::Emoji { file } if file.id == "emoji:7001")), [":ferris:"]);
    assert_eq!(spanned(rich, |style| matches!(style, Style::CodeBlock { language } if language == "rust")), ["fn main() {}"]);

    let picture = &rich.attachments[0];
    assert_eq!((picture.name.as_str(), picture.mime.as_str()), ("graph.png", "image/png"));
    assert_eq!((picture.file.kind, picture.file.width, picture.file.height, picture.file.bytes), (MediaKind::Image, 1600, 900, 48213));
    let thumb = picture.thumb.as_ref().unwrap();
    assert_eq!((thumb.width, thumb.height), (400, 225), "the inline picture is registered at the size a row draws it");
    let log = &rich.attachments[1];
    assert_eq!((log.file.kind, log.thumb.is_none(), log.mime.as_str()), (MediaKind::File, true, "text/plain"));

    let card = &rich.previews[0];
    assert_eq!((card.title.as_str(), card.site.as_str(), card.url.as_str()), ("Status", "Statuspage", "https://status.example/x"));
    assert_eq!(card.image.as_ref().map(|image| (image.width, image.height)), Some((400, 210)));

    let thumbs_up = &rich.reactions[0];
    assert_eq!((thumbs_up.emoji.as_str(), thumbs_up.count, thumbs_up.mine), ("👍", 3, true));
    let custom = &rich.reactions[1];
    assert_eq!((custom.emoji.as_str(), custom.count, custom.mine), (":ferris:", 1, false));
    assert_eq!(custom.file.as_ref().map(|file| file.kind), Some(MediaKind::Emoji));

    let quoted = rich.reply_to.as_ref().unwrap();
    assert_eq!((quoted.id.as_str(), quoted.sender.as_str(), quoted.snippet.as_str()), ("1300000000000000009", "Justin", "is the deploy done?"));

    // Our own message, a system line, a poll we cannot draw, a sticker.
    assert!(messages[3].outgoing);
    assert_eq!((messages[2].kind.clone(), messages[2].text.as_str()), (MessageKind::System, "Mara O started a call."));
    let poll_link = "https://discord.com/channels/@me/3001/1300000000000000007".to_owned();
    assert_eq!((messages[1].kind.clone(), messages[1].text.as_str()), (MessageKind::Unsupported { link: poll_link }, "Poll: Lunch?"));
    assert_eq!(messages[0].attachments[0].name, "Wave");
    assert_eq!(messages[0].attachments[0].file.id, "sticker:9001");

    // Everything the message refers to can be fetched by id; the addresses are Discord's own.
    let wanted: Vec<String> = rich.files().map(|file| file.id.clone()).collect();
    match world.ask(Request::Fetch { retry: false, size: None, account: world.account(), ids: wanted.clone() }) {
        Response::Media { files } => assert_eq!(files.len(), wanted.len()),
        other => panic!("{other:?}"),
    }

    // An edit arrives as only the fields that changed.
    let edit = json!({"id": rich.id, "channel_id": dm, "content": "edited **now**", "edited_timestamp": "2024-10-27T17:00:00+00:00"});
    world.discord.push("MESSAGE_UPDATE", edit);
    let edited = wait("the edit", || world.messages(dm).pop().filter(|message| message.edited));
    assert_eq!(edited.text, "edited now");
    assert_eq!(spanned(&edited, |style| *style == Style::Bold), ["now"]);
    assert_eq!((edited.attachments.len(), edited.reactions.len(), edited.pinned), (2, 2, true), "what the edit did not name stays");

    // Reactions, one person at a time.
    let reaction = |user: &str| json!({"user_id": user, "channel_id": dm, "message_id": rich.id, "emoji": {"id": null, "name": "👍"}, "type": 0});
    world.discord.push("MESSAGE_REACTION_ADD", reaction("2002"));
    wait("the reaction", || world.messages(dm).pop().filter(|message| message.reactions[0].count == 4));
    world.discord.push("MESSAGE_REACTION_REMOVE", reaction("1000"));
    let after = wait("our reaction going", || world.messages(dm).pop().filter(|message| message.reactions[0].count == 3));
    assert!(!after.reactions[0].mine);
    world.discord.push("MESSAGE_REACTION_REMOVE_ALL", json!({"channel_id": dm, "message_id": rich.id}));
    wait("every reaction going", || world.messages(dm).pop().filter(|message| message.reactions.is_empty()));

    // A delete.
    world.discord.push("MESSAGE_DELETE", json!({"id": "1300000000000000006", "channel_id": dm}));
    let left = wait("the delete", || Some(world.messages(dm)).filter(|messages| messages.len() == 4));
    assert_eq!(left[0].id, "1300000000000000007");

    // Scrolling past the oldest cached message asks Discord for the page before it.
    let before = Request::Messages {
        conversation: world.key(dm),
        page: Page::Before { id: left[0].id.clone() },
        limit: 50,
    };
    assert!(matches!(world.ask(before), Response::Messages { messages, older: false, .. } if messages.is_empty()));
    wait("the older page being asked for", || {
        let asked = world.discord.requests_to("GET", "/channels/3001/messages?before=1300000000000000007");
        (!asked.is_empty()).then_some(())
    });

    // A new direct message is unread and previews on its row.
    let unread_before = world.conversation("3002").unread;
    let id = world.incoming("3002", None, "lunch at **noon**?", json!({}));
    let row = wait("the new message", || Some(world.conversation("3002")).filter(|row| row.unread == unread_before + 1));
    assert_eq!(row.preview, "lunch at noon?");
    assert_eq!(world.messages("3002").last().map(|message| message.id.clone()), Some(id));

    // A server channel: nickname, role and channel mentions by name, a thread, a join line, a bot.
    let general = wait("the channel's history", || Some(world.messages("4101")).filter(|messages| messages.len() == 3));
    let plan = &general[2];
    assert!(plan.text.starts_with("@mods the plan is in #announcements, starts 2024-10-2"), "{}", plan.text);
    assert_eq!(plan.sender.name, "sam (infra)");
    assert!(plan.edited && plan.mentions_me, "a role we hold was mentioned");
    assert_eq!(spanned(plan, |style| matches!(style, Style::Mention { kind: MentionKind::Role, id } if id == "4010")), ["@mods"]);
    assert_eq!(spanned(plan, |style| matches!(style, Style::Mention { kind: MentionKind::Channel, id } if id == "4102")), ["#announcements"]);
    let thread = plan.thread.as_ref().unwrap();
    assert_eq!((thread.id.as_str(), thread.replies), ("4500", 4));
    assert_eq!((general[1].kind.clone(), general[1].text.as_str()), (MessageKind::System, "Dev Patel joined the server."));
    let bot = &general[0];
    assert!(bot.sender.bot && bot.sender.id.is_empty() && bot.text.is_empty());
    // What a webhook composed is the message's card, not a link's preview.
    let composed = bot.card.as_ref().unwrap();
    assert_eq!((composed.title.as_str(), composed.text.as_str()), ("Build #812 passed", "main at 4f2a9c1"));
    assert_eq!(composed.fields, [("Duration".to_owned(), "3m 12s".to_owned())]);
    assert!(bot.previews.is_empty(), "a picture Discord does not proxy is never fetched");

    // The thread is a conversation of its own, opened like any other.
    assert_eq!(world.conversation(&thread.id).kind, ConversationKind::Thread);
    let replies = wait("the thread's messages", || Some(world.messages("4500")).filter(|messages| !messages.is_empty()));
    assert_eq!(replies[0].text, "cut the branch on Monday Tuesday");
    assert_eq!(spanned(&replies[0], |style| *style == Style::Strike), ["Monday"]);

    // A channel nobody opened: @everyone is suppressed there, so it turns unread and no more,
    // and its messages are not stored until it is opened.
    assert_eq!(world.conversation("4102").unread, 0);
    world.incoming("4102", Some("4000"), "@everyone release is out", json!({"mention_everyone": true}));
    let row = wait("the channel turning unread", || Some(world.conversation("4102")).filter(|row| row.unread == 1));
    assert_eq!(row.mentions, 0, "suppress @everyone is on for this server");
    assert!(world.messages("4102").is_empty());
    // A role we hold is not suppressed: that one counts, and is kept.
    let id = world.incoming("4102", Some("4000"), "<@&4010> please look", json!({"mention_roles": ["4010"]}));
    let row = wait("the mention", || Some(world.conversation("4102")).filter(|row| row.mentions == 1));
    assert_eq!(row.preview, "@mods please look");
    let kept = world.messages("4102");
    assert_eq!(kept.iter().map(|message| message.id.as_str()).collect::<Vec<_>>(), [id.as_str()]);
    assert!(kept[0].mentions_me);
}

#[test]
fn typing_appears_expires_and_is_paced_on_the_way_out() {
    let world = World::start();
    let dm = world.key("3001");
    let seq = world.status().4;

    let heard = now();
    world.discord.push("TYPING_START", json!({"channel_id": "3001", "user_id": "2001", "timestamp": heard}));
    let typists = wait("the typing line", || match world.ask(Request::Wait { since: seq }) {
        Response::Changes { changes, .. } => changes.into_iter().find_map(|change| match change {
            Change::Typing { conversation, typists } if conversation == dm && !typists.is_empty() => Some(typists),
            _ => None,
        }),
        other => panic!("{other:?}"),
    });
    assert_eq!((typists[0].id.as_str(), typists[0].name.as_str()), ("2001", "Mara O"));
    let lasts = typists[0].until - heard;
    assert!((9..=11).contains(&lasts), "typing lasts ten seconds, not {lasts}");

    let watching = || {
        let watch = Watching {
            conversation: Some(dm.clone()),
            ..Watching::default()
        };
        match world.ask(Request::Watch(watch)) {
            Response::Watching { typists, .. } => typists,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(watching().len(), 1, "a shell that opens the conversation now is told");
    // Our own typing is never shown to us.
    world.discord.push("TYPING_START", json!({"channel_id": "3001", "user_id": "1000", "timestamp": heard}));

    // Discord hears typing only once the user lets it (Settings › Privacy, off by default).
    allow_typing(&world, &dm);
    // The draft changing three times is one signal to Discord.
    for _ in 0..3 {
        assert_eq!(world.ask(Request::Typing { conversation: dm.clone() }), Response::Ok);
    }
    wait("the typing signal", || (!world.discord.requests_to("POST", "/channels/3001/typing").is_empty()).then_some(()));
    sleep(Duration::from_secs(1));
    assert_eq!(world.discord.requests_to("POST", "/channels/3001/typing").len(), 1);
    assert_eq!(watching().len(), 1);

    // Nothing more heard: the line is gone once its time is up.
    sleep(Duration::from_secs((typists[0].until - now() + 1).max(0) as u64));
    assert!(watching().is_empty());
}

#[test]
fn sending_echoes_once_and_failures_say_why() {
    let world = World::start();
    let posts = |channel: &str| world.discord.requests_to("POST", &format!("/channels/{channel}/messages")).len();

    // Sent: the pending row becomes the real message, once, though it arrives twice (the HTTP
    // answer and the gateway's echo carrying the nonce).
    let client_id = "1730000000000000001";
    match world.send("3002", client_id, "on it", None) {
        Response::Sent { message } => assert_eq!((message.state, message.id.as_str()), (SendState::Pending, client_id)),
        other => panic!("{other:?}"),
    }
    let sent = world.outcome("3002", client_id);
    assert_eq!(sent.state, SendState::Sent);
    assert!(sent.outgoing && sent.id != client_id && sent.text == "on it");
    let echoed = world.discord.sequence.load(Ordering::Relaxed);
    wait("the echo to be dispatched", || (world.discord.sequence.load(Ordering::Relaxed) >= echoed).then_some(()));
    sleep(Duration::from_millis(500));
    let stored = world.messages("3002");
    assert_eq!(stored.iter().filter(|message| message.text == "on it").count(), 1);
    assert_eq!(world.conversation("3002").unread, 0, "our own message is not unread");
    let request = &world.discord.requests_to("POST", "/channels/3002/messages")[0];
    assert_eq!((&request.body["nonce"], &request.body["content"]), (&json!(client_id), &json!("on it")));

    // Into a thread, and as a reply.
    world.send("4500", "1730000000000000002", "branch is cut", Some("1300000000000000800"));
    assert_eq!(world.outcome("4500", "1730000000000000002").state, SendState::Sent);
    let request = &world.discord.requests_to("POST", "/channels/4500/messages")[0];
    assert_eq!(request.body["message_reference"]["message_id"], "1300000000000000800");
    assert_eq!(request.body["message_reference"]["guild_id"], "4000");

    // Rate limited: waited out once, tried once more, then the user is told.
    let limited = json!({"message": "You are being rate limited.", "retry_after": 0.3, "global": false});
    world.discord.script("/channels/3003/messages", 429, limited.clone());
    world.discord.script("/channels/3003/messages", 429, limited);
    world.send("3003", "1730000000000000003", "anyone?", None);
    assert_eq!(world.failure("3003", "1730000000000000003"), "Rate limited: try again in 1 s.");
    assert_eq!(posts("3003"), 2);

    // Slow mode is the user's wait, not ours: no retry.
    world.discord.script("/channels/4101/messages", 429, json!({"message": "Slowmode", "code": 20016, "retry_after": 12.4}));
    world.send("4101", "1730000000000000004", "hello", None);
    assert_eq!(world.failure("4101", "1730000000000000004"), "Slow mode: wait 13 s.");
    assert_eq!(posts("4101"), 1);

    world.discord.script("/channels/4104/messages", 403, json!({"message": "Missing Permissions", "code": 50013}));
    world.send("4104", "1730000000000000005", "hello", None);
    assert_eq!(world.failure("4104", "1730000000000000005"), "You do not have permission to do this here.");

    // Too long never leaves the machine.
    world.send("3003", "1730000000000000006", &"x".repeat(2001), None);
    assert_eq!(world.failure("3003", "1730000000000000006"), "Message too long: Discord allows 2000 characters.");
    assert_eq!(posts("3003"), 2);

    // Reading here tells Discord, so the phone agrees.
    assert_eq!(world.conversation("3001").unread, 2);
    let read = Request::MarkRead {
        conversation: world.key("3001"),
        message_id: "1300000010000000010".to_owned(),
    };
    assert_eq!(world.ask(read), Response::Ok);
    assert_eq!(world.conversation("3001").unread, 0);
    let acked = wait("the acknowledgement", || {
        world.discord.requests_to("POST", "/channels/3001/messages/1300000010000000010/ack").pop()
    });
    assert!(acked.body["token"].is_null());
    // And reading on the phone clears it here.
    assert_eq!(world.conversation("4105").unread, 1);
    world.discord.push("MESSAGE_ACK", json!({"channel_id": "4105", "message_id": "1300000000000000400", "version": 813}));
    let row = wait("the other device's read", || Some(world.conversation("4105")).filter(|row| row.unread == 0));
    assert_eq!(row.last_read.as_deref(), Some("1300000000000000400"));

    // A captcha ends everything: the action fails with where to finish it, and nothing more is
    // asked of Discord.
    let captcha = json!({"captcha_key": ["captcha-required"], "captcha_sitekey": "a9b5fb07", "captcha_service": "hcaptcha"});
    world.discord.script("/channels/3002/messages", 400, captcha);
    world.send("3002", "1730000000000000007", "again", None);
    let why = world.failure("3002", "1730000000000000007");
    assert!(why.contains("Finish it in Discord"), "{why}");
    let asked = world.discord.requests.lock().unwrap().len();
    world.send("3002", "1730000000000000008", "and again", None);
    assert_eq!(world.failure("3002", "1730000000000000008"), why);
    assert_eq!(world.discord.requests.lock().unwrap().len(), asked, "a challenged account sends nothing");
}

#[test]
fn a_dropped_socket_resumes_and_a_refused_token_expires_the_account() {
    let world = World::start();
    wait("friends' presence", || world.people(Some(Relation::Friend), None).iter().any(|person| person.presence == Presence::Online).then_some(()));
    world.incoming("3002", None, "before the drop", json!({}));
    wait("the message before the drop", || world.messages("3002").pop());
    let heard = world.discord.sequence.load(Ordering::Relaxed);

    world.discord.control.lock().unwrap().send(Control::Drop).unwrap();
    wait("the link going down", || (world.link() == Link::Offline).then_some(()));
    wait("the link coming back", || (world.link() == Link::Connected).then_some(()));

    // The same session, picked up where it stopped: no second identify.
    assert_eq!(world.discord.frames_of(2).len(), 1);
    let resumes = world.discord.frames_of(6);
    assert_eq!(resumes.len(), 1);
    assert_eq!(resumes[0]["d"], json!({"token": TOKEN, "session_id": "session-one", "seq": heard}));
    assert_eq!(world.discord.connections.load(Ordering::Relaxed), 2);
    // Discord does not send presence again after a resume; the adapter still knows it.
    wait("friends' presence after the resume", || {
        world.people(Some(Relation::Friend), None).iter().any(|person| person.presence == Presence::Online).then_some(())
    });
    let id = world.incoming("3002", None, "after the resume", json!({}));
    wait("a message on the resumed session", || world.messages("3002").pop().filter(|message| message.id == id));
    // The heartbeat keeps going on the new socket.
    let beats = world.discord.frames_of(1).len();
    wait("a heartbeat", || (world.discord.frames_of(1).len() > beats).then_some(()));

    // 4004: Discord no longer accepts the token. Not signed out: the cache stays readable.
    world.discord.control.lock().unwrap().send(Control::Close(4004)).unwrap();
    wait("the account to expire", || (world.link() == Link::Expired).then_some(()));
    assert!(!world.status().1.is_empty(), "the reason is given");
    assert_eq!(world.messages("3002").len(), 2);
    let refused = world.send("3002", "1730000000000000009", "hello?", None);
    assert!(matches!(refused, Response::Error { code: ErrorCode::Offline, .. }), "{refused:?}");
    // Retrying cannot help, so it is not tried.
    sleep(Duration::from_secs(4));
    assert_eq!(world.discord.connections.load(Ordering::Relaxed), 2);
    assert_eq!(world.link(), Link::Expired);
}

#[test]
fn the_space_on_screen_is_the_only_one_subscribed() {
    let world = World::start();
    let subscriptions = || world.discord.frames_of(37);
    assert!(subscriptions().is_empty(), "nothing is subscribed until a Space is opened");

    // The quick switcher reads the tree without opening the Space, and that subscribes nothing.
    let peeked = world.ask(Request::Tree { space: world.key("4000"), offset: 0, limit: 500, peek: true });
    assert!(matches!(peeked, Response::Tree { ref conversations, .. } if !conversations.is_empty()), "{peeked:?}");
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(subscriptions().is_empty(), "a peek subscribed: {:?}", subscriptions());

    // Opening the Space subscribes to it: typing and threads, no activity stream.
    world.tree("4000");
    let opened = wait("the subscription", || subscriptions().pop());
    let ours = &opened["d"]["subscriptions"]["4000"];
    assert_eq!((&ours["typing"], &ours["threads"], &ours["activities"]), (&json!(true), &json!(true), &json!(false)));
    assert_eq!(ours["channels"], json!({}), "no member list was asked for");
    world.tree("4000");
    sleep(Duration::from_millis(300));
    assert_eq!(subscriptions().len(), 1, "looking again changes nothing");

    // The visible rows of a member list: asked for by range, answered from Discord's sync.
    let asking = {
        let (dir, conversation) = (world.dir.clone(), world.key("4101"));
        std::thread::spawn(move || {
            let mut stream = ChatStream::connect(&dir.join("chat.sock")).unwrap();
            write_frame(&mut stream, &Request::Members { conversation, offset: 0, limit: 50 }).unwrap();
            read_frame::<Response>(&mut stream).unwrap()
        })
    };
    let ranged = wait("the range request", || subscriptions().pop().filter(|frame| frame["d"]["subscriptions"]["4000"]["channels"]["4101"].is_array()));
    assert_eq!(ranged["d"]["subscriptions"]["4000"]["channels"]["4101"], json!([[0, 99]]));
    let member = |id: &str, username: &str, name: &str, nick: Value, status: &str| {
        json!({"member": {
            "user": {"id": id, "username": username, "global_name": name, "discriminator": "0", "avatar": null},
            "nick": nick, "roles": [],
            "presence": {"user": {"id": id}, "status": status, "activities": [], "client_status": {}},
        }})
    };
    world.discord.push("GUILD_MEMBER_LIST_UPDATE", json!({
        "guild_id": "4000",
        "id": "everyone",
        "member_count": 312,
        "online_count": 2,
        "groups": [{"id": "4010", "count": 1}, {"id": "online", "count": 1}, {"id": "offline", "count": 1}],
        "ops": [{"op": "SYNC", "range": [0, 99], "items": [
            {"group": {"id": "4010", "count": 1}},
            member("2001", "mara", "Mara Okafor", json!("mara-mod"), "dnd"),
            {"group": {"id": "online", "count": 1}},
            member("2006", "sam", "Sam Whitlock", Value::Null, "online"),
            {"group": {"id": "offline", "count": 1}},
            member("2002", "dev", "Dev Patel", Value::Null, "offline"),
        ]}],
    }));
    match asking.join().unwrap() {
        Response::Members { sections, members, offset, .. } => {
            let sections: Vec<(&str, u32)> = sections.iter().map(|section| (section.name.as_str(), section.count)).collect();
            assert_eq!(sections, [("mods", 1), ("Online", 1), ("Offline", 1)]);
            let names: Vec<&str> = members.iter().map(|person| person.name.as_str()).collect();
            assert_eq!(names, ["mara-mod", "Sam Whitlock", "Dev Patel"]);
            assert_eq!((members[0].relation, members[0].presence), (Relation::Friend, Presence::Dnd));
            assert_eq!((members[1].relation, members[2].presence), (Relation::None, Presence::Offline));
            assert_eq!(offset, 0);
        }
        other => panic!("{other:?}"),
    }
    // A window already held answers without asking Discord again.
    let asked = subscriptions().len();
    match world.ask(Request::Members { conversation: world.key("4101"), offset: 1, limit: 1 }) {
        Response::Members { members, .. } => assert_eq!(members[0].name, "Sam Whitlock"),
        other => panic!("{other:?}"),
    }
    assert_eq!(subscriptions().len(), asked);
    // A direct conversation has no member list.
    let none = world.ask(Request::Members { conversation: world.key("3001"), offset: 0, limit: 50 });
    assert!(matches!(none, Response::Error { code: ErrorCode::Unsupported, .. }), "{none:?}");

    // Searching people asks the server on screen by the typed prefix, never for everyone.
    assert!(world.people(None, Some("zo")).is_empty());
    let search = wait("the member search", || world.discord.frames_of(8).pop());
    assert_eq!((&search["d"]["query"], &search["d"]["limit"], &search["d"]["guild_id"]), (&json!("zo"), &json!(25), &json!(["4000"])));
    let zoe = json!({"user": {"id": "2050", "username": "zoe", "global_name": "Zoe Lin", "discriminator": "0", "avatar": null}, "roles": []});
    // A chunk nobody asked for is not stored.
    let stray = json!({"user": {"id": "2051", "username": "zora", "global_name": "Zora", "discriminator": "0", "avatar": null}, "roles": []});
    world.discord.push("GUILD_MEMBERS_CHUNK", json!({"guild_id": "4000", "members": [stray], "chunk_index": 0, "chunk_count": 1, "nonce": "someone-else"}));
    world.discord.push("GUILD_MEMBERS_CHUNK", json!({"guild_id": "4000", "members": [zoe], "chunk_index": 0, "chunk_count": 1, "nonce": search["d"]["nonce"]}));
    let found = wait("the search result", || Some(world.people(None, Some("zo"))).filter(|people| !people.is_empty()));
    assert_eq!(found.iter().map(|person| person.name.as_str()).collect::<Vec<_>>(), ["Zoe Lin"]);

    // Starting a conversation with a friend we already talk to asks Discord nothing; with
    // someone new it is one request.
    match world.ask(Request::StartDirect { person: world.key("2001") }) {
        Response::Conversation { conversation } => assert_eq!(conversation.key.id, "3001"),
        other => panic!("{other:?}"),
    }
    assert!(world.discord.requests_to("POST", "/users/@me/channels").is_empty());
    match world.ask(Request::StartDirect { person: world.key("2050") }) {
        Response::Conversation { conversation } => {
            assert_eq!((conversation.key.id.as_str(), conversation.title.as_str(), conversation.kind), ("3050", "Zoe Lin", ConversationKind::Direct));
        }
        other => panic!("{other:?}"),
    }
    let created = world.discord.requests_to("POST", "/users/@me/channels");
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].body, json!({"recipients": ["2050"]}));

    // Moving to a direct conversation ends the subscription.
    let watch = Watching {
        conversation: Some(world.key("3001")),
        ..Watching::default()
    };
    world.ask(Request::Watch(watch));
    let closed = wait("the subscription ending", || subscriptions().pop().filter(|frame| frame["d"]["subscriptions"]["4000"]["typing"] == false));
    assert_eq!(closed["d"]["subscriptions"]["4000"]["channels"], json!({}));
}

#[test]
fn servers_channels_people_and_settings_follow_live_events() {
    let world = World::start();
    let tree_ids = |space: &str| world.tree(space).1.iter().map(|row| row.key.id.clone()).collect::<Vec<_>>();
    let spaces = || match world.ask(Request::Spaces { network: Some(Network::Discord) }) {
        Response::Spaces { spaces } => spaces.into_iter().map(|space| space.name).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };

    // A channel created lands where Discord orders it; hidden from us, it goes; deleted, it goes.
    let channel = |overwrites: Value| {
        json!({"id": "4107", "type": 0, "name": "offtopic", "position": 5, "parent_id": "4100", "guild_id": "4000",
               "last_message_id": null, "permission_overwrites": overwrites})
    };
    world.discord.push("CHANNEL_CREATE", channel(json!([])));
    wait("the new channel", || tree_ids("4000").contains(&"4107".to_owned()).then_some(()));
    assert_eq!(tree_ids("4000"), ["4105", "4101", "4500", "4102", "4104", "4106", "4107", "4201"]);
    world.discord.push("CHANNEL_UPDATE", channel(json!([{"id": "4000", "type": 0, "allow": "0", "deny": "1024"}])));
    wait("the channel being hidden", || (!tree_ids("4000").contains(&"4107".to_owned())).then_some(()));
    world.discord.push("CHANNEL_UPDATE", json!({"id": "4102", "type": 5, "name": "news", "position": 1, "parent_id": "4100", "guild_id": "4000", "permission_overwrites": []}));
    wait("the rename", || (world.conversation("4102").title == "news").then_some(()));
    world.discord.push("CHANNEL_DELETE", json!({"id": "4104", "type": 0, "guild_id": "4000", "parent_id": "4100"}));
    wait("the delete", || (!tree_ids("4000").contains(&"4104".to_owned())).then_some(()));

    // A thread we join is listed under its channel; archived, it leaves the tree but stays readable.
    let thread = |archived: bool| {
        json!({"id": "4501", "type": 11, "name": "hotfix", "parent_id": "4102", "guild_id": "4000", "owner_id": "1000",
               "thread_metadata": {"archived": archived, "auto_archive_duration": 1440, "locked": false},
               "member": {"flags": 1, "join_timestamp": "2026-10-13T08:00:00+00:00"}, "newly_created": true})
    };
    world.discord.push("THREAD_CREATE", thread(false));
    wait("the thread", || tree_ids("4000").contains(&"4501".to_owned()).then_some(()));
    assert_eq!(tree_ids("4000"), ["4105", "4101", "4500", "4102", "4501", "4106", "4201"]);
    world.discord.push("THREAD_UPDATE", thread(true));
    wait("the thread leaving the tree", || (!tree_ids("4000").contains(&"4501".to_owned())).then_some(()));
    assert_eq!(world.conversation("4501").kind, ConversationKind::Thread);

    // A channel of a server nobody is looking at got a message: its row turns unread.
    assert_eq!(world.conversation("4102").unread, 0);
    let newest = world.discord.snowflake();
    world.discord.push("CHANNEL_UNREAD_UPDATE", json!({"guild_id": "4000", "channel_unread_updates": [{"id": "4102", "last_message_id": newest, "last_pin_timestamp": null}]}));
    wait("the unread dot", || (world.conversation("4102").unread == 1).then_some(()));

    // Mute and suppress settings changed in Discord apply here.
    let settings = |muted: bool, end: Value| {
        json!({"guild_id": "4000", "muted": muted, "mute_config": {"end_time": end, "selected_time_window": 900},
               "suppress_everyone": false, "suppress_roles": true, "message_notifications": 1, "channel_overrides": [], "version": 45})
    };
    world.discord.push("USER_GUILD_SETTINGS_UPDATE", settings(true, Value::Null));
    wait("the server being muted", || world.conversation("4101").muted.then_some(()));
    // @everyone is no longer suppressed and counts even while muted; a role mention now does not.
    world.incoming("4102", Some("4000"), "@everyone standup", json!({"mention_everyone": true}));
    wait("the @everyone mention", || (world.conversation("4102").mentions == 1).then_some(()));
    world.incoming("4102", Some("4000"), "<@&4010> standup", json!({"mention_roles": ["4010"]}));
    world.incoming("3002", None, "marker", json!({}));
    wait("the events before the marker to be handled", || world.messages("3002").pop());
    assert_eq!(world.conversation("4102").mentions, 1);
    // A timed mute whose time has passed is no mute; the channel's own override went with the update.
    world.discord.push("USER_GUILD_SETTINGS_UPDATE", settings(true, json!("2020-01-01T00:00:00+00:00")));
    wait("the mute running out", || (!world.conversation("4101").muted && !world.conversation("4105").muted).then_some(()));

    // Servers joined, renamed and left.
    world.discord.push("GUILD_CREATE", json!({
        "id": "6000", "name": "New Place", "icon": null, "owner_id": "2001", "joined_at": "2026-10-13T08:00:00+00:00",
        "roles": [{"id": "6000", "name": "@everyone", "permissions": "1024", "position": 0}],
        "channels": [{"id": "6001", "type": 0, "name": "lobby", "position": 0, "permission_overwrites": []}],
        "threads": [],
        "members": [{"user": me(), "roles": [], "joined_at": "2026-10-13T08:00:00+00:00"}],
    }));
    wait("the new server", || (spaces() == ["New Place", "Zed", "Rustaceans"]).then_some(()));
    assert_eq!(tree_ids("6000"), ["6001"]);
    world.discord.push("GUILD_UPDATE", json!({"id": "6000", "name": "Renamed Place", "icon": "n3w", "owner_id": "2001"}));
    wait("the rename", || (spaces()[0] == "Renamed Place").then_some(()));
    world.discord.push("GUILD_DELETE", json!({"id": "5000", "unavailable": true}));
    world.discord.push("GUILD_DELETE", json!({"id": "6000"}));
    wait("leaving the server", || (spaces() == ["Zed", "Rustaceans"]).then_some(()));

    // Friends added and removed, with their presence and status line.
    let sam = json!({"id": "2006", "username": "sam", "global_name": "Sam Whitlock", "discriminator": "0", "avatar": "s4m"});
    world.discord.push("RELATIONSHIP_ADD", json!({"id": "2006", "type": 1, "nickname": null, "user": sam, "should_notify": true}));
    world.discord.push("PRESENCE_UPDATE", json!({"user": {"id": "2006"}, "status": "dnd", "client_status": {"desktop": "dnd"},
        "activities": [{"type": 4, "name": "Custom Status", "state": "in a meeting"}]}));
    let friend = wait("the new friend", || {
        let friends = world.people(Some(Relation::Friend), None);
        friends.into_iter().find(|person| person.key.id == "2006" && person.presence == Presence::Dnd)
    });
    assert_eq!((friend.name.as_str(), friend.status.as_str()), ("Sam Whitlock", "in a meeting"));
    world.discord.push("RELATIONSHIP_REMOVE", json!({"id": "2006", "type": 1}));
    wait("the friend removed", || world.people(Some(Relation::Friend), None).iter().all(|person| person.key.id != "2006").then_some(()));

    // Someone who is no friend: their presence is dropped unless the shell is watching them.
    let stranger = |status: &str| json!({"user": {"id": "2004"}, "status": status, "activities": [], "client_status": {}});
    let watched = |people: Vec<Key>| match world.ask(Request::Watch(Watching { people, ..Watching::default() })) {
        Response::Watching { people, .. } => people.into_iter().map(|person| person.presence).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    world.discord.push("PRESENCE_UPDATE", stranger("online"));
    world.incoming("3002", None, "second marker", json!({}));
    wait("the presence to be handled", || world.messages("3002").pop().filter(|message| message.text == "second marker"));
    assert_eq!(watched(vec![world.key("2004")]), [Presence::Unknown], "nobody was watching when it arrived");
    world.discord.push("PRESENCE_UPDATE", stranger("idle"));
    wait("the watched person's presence", || (watched(vec![world.key("2004")]) == [Presence::Idle]).then_some(()));

    // Our own name changing.
    world.discord.push("USER_UPDATE", json!({"id": "1000", "username": "justin", "global_name": "Justin S", "discriminator": "0", "avatar": "me0hash"}));
    wait("the new name", || match world.ask(Request::Status) {
        Response::Status { accounts, .. } => (accounts[0].name == "Justin S").then_some(()),
        other => panic!("{other:?}"),
    });
}

#[test]
fn a_401_over_http_expires_the_account_too() {
    let world = World::start();
    world.discord.script("/channels/3002/messages", 401, json!({"message": "401: Unauthorized", "code": 0}));
    world.send("3002", "1730000000000000010", "still there?", None);
    assert_eq!(world.failure("3002", "1730000000000000010"), "Discord signed you out. Sign in again.");
    wait("the account to expire", || (world.link() == Link::Expired).then_some(()));
    // The socket is given up as well, and nothing reconnects.
    sleep(Duration::from_secs(3));
    assert_eq!(world.discord.connections.load(Ordering::Relaxed), 1);
}

#[test]
fn calls_ring_in_and_voice_channels_are_joined_on_the_gateway() {
    let world = World::start();
    let call = || match world.ask(Request::Status) {
        Response::Status { call, .. } => call,
        other => panic!("{other:?}"),
    };
    let voice_states = || world.discord.frames_of(4);

    // Someone calls: a ring, named, with no media process and nothing sent to Discord.
    let ringing = |ringing: Value| {
        json!({"channel_id": "3001", "message_id": "1300000000000009000", "region": "us-east", "ringing": ringing,
               "voice_states": [{"user_id": "2001", "channel_id": "3001", "session_id": "voice-mara", "self_mute": false, "self_deaf": false, "mute": false, "deaf": false}]})
    };
    world.discord.push("CALL_CREATE", ringing(json!(["1000"])));
    let ring: Call = wait("the ring", call);
    assert_eq!(ring.conversation, world.key("3001"));
    assert_eq!(ring.state, CallState::RingingIn { from: "Mara O".to_owned() });
    // The caller gave up.
    world.discord.push("CALL_UPDATE", ringing(json!([])));
    wait("the ring stopping", || call().is_none().then_some(()));
    // Declining tells Discord to stop ringing this account.
    world.discord.push("CALL_UPDATE", ringing(json!(["1000"])));
    wait("the second ring", call);
    world.ask(Request::CallDecline);
    wait("the decline", || world.discord.requests_to("POST", "/channels/3001/call/stop-ringing").pop());
    assert!(voice_states().is_empty(), "a ring never touches our voice state");
    world.discord.push("CALL_DELETE", json!({"channel_id": "3001"}));

    // Joining a voice channel is a voice state update for that server and channel.
    world.ask(Request::CallStart { conversation: world.key("4201") });
    let joined = wait("the join", || voice_states().pop());
    assert_eq!((&joined["d"]["guild_id"], &joined["d"]["channel_id"]), (&json!("4000"), &json!("4201")));
    assert_eq!((&joined["d"]["self_mute"], &joined["d"]["self_deaf"]), (&json!(false), &json!(false)));
    // Who is already in the channel came with the server's voice states at connect.
    let there = wait("the roster", || call().filter(|call| !call.participants.is_empty()));
    let seen: Vec<(&str, &str, bool)> = there.participants.iter().map(|person| (person.id.as_str(), person.name.as_str(), person.muted)).collect();
    assert_eq!(seen, [("2002", "Dev Patel", true)]);
    // Someone else joining, and then leaving, moves the roster.
    let other = |channel: Value| {
        json!({"guild_id": "4000", "channel_id": channel, "user_id": "2006", "session_id": "voice-sam",
               "self_mute": false, "self_deaf": true, "mute": false, "deaf": false,
               "member": {"user": {"id": "2006", "username": "sam", "global_name": "Sam Whitlock"}, "nick": "sam (infra)", "roles": []}})
    };
    world.discord.push("VOICE_STATE_UPDATE", other(json!("4201")));
    let both = wait("the newcomer", || call().filter(|call| call.participants.len() == 2));
    let newcomer = both.participants.iter().find(|person| person.id == "2006").unwrap();
    assert_eq!((newcomer.name.as_str(), newcomer.deafened), ("sam (infra)", true));
    world.discord.push("VOICE_STATE_UPDATE", other(Value::Null));
    wait("the newcomer leaving", || call().filter(|call| call.participants.len() == 1));

    // Leaving is the same frame with no channel.
    world.ask(Request::CallEnd);
    let left = wait("the leave", || voice_states().pop().filter(|frame| frame["d"]["channel_id"].is_null()));
    assert_eq!(left["d"]["guild_id"], "4000");
}

/// Waits until everything pushed so far has been handled: a marker message sent after it is
/// stored only once the events before it were.
fn settled(world: &World) {
    let marker = world.incoming("3004", None, "marker", json!({}));
    wait("the events before the marker", || world.messages("3004").iter().any(|message| message.id == marker).then_some(()));
}

#[test]
fn reactions_edits_and_deletes_go_to_discord_and_the_store_follows_its_word() {
    let world = World::start();
    let dm = "3001";
    wait("history of the conversation", || Some(world.messages(dm)).filter(|messages| messages.len() == 5));
    let (rich, mine) = ("1300000010000000010", "1300000000000000009");
    let react = |message: &str, emoji: &str, on: bool| {
        world.ask(Request::React { conversation: world.key(dm), message_id: message.to_owned(), emoji: emoji.to_owned(), on })
    };
    let reaction = |message: &str, emoji: &str| {
        let stored = world.messages(dm).into_iter().find(|stored| stored.id == message).unwrap();
        stored.reactions.into_iter().find(|reaction| reaction.emoji == emoji).map(|reaction| (reaction.count, reaction.mine))
    };
    let reactions = format!("/channels/{dm}/messages");

    // An emoji nobody used yet. Discord's own event for it follows, and it still counts once.
    assert_eq!(react(mine, "🎉", true), Response::Ok);
    let put = world.discord.requests_to("PUT", &reactions);
    assert_eq!(put.len(), 1);
    assert_eq!(put[0].path, format!("{reactions}/{mine}/reactions/%F0%9F%8E%89/%40me?location=Message%20Reaction%20Picker&type=0"));
    settled(&world);
    assert_eq!(reaction(mine, "🎉"), Some((1, true)));

    // A custom emoji is named to Discord as `name:id`.
    assert_eq!(react(rich, ":ferris:", true), Response::Ok);
    let put = world.discord.requests_to("PUT", &reactions);
    assert!(put[1].path.starts_with(&format!("{reactions}/{rich}/reactions/ferris%3A7001/%40me")), "{}", put[1].path);
    settled(&world);
    assert_eq!(reaction(rich, ":ferris:"), Some((2, true)));

    // Taking one back.
    assert_eq!(react(rich, "👍", false), Response::Ok);
    let deleted = world.discord.requests_to("DELETE", &reactions);
    assert_eq!(deleted[0].path, format!("{reactions}/{rich}/reactions/%F0%9F%91%8D/0/%40me?location=Message%20Inline%20Button&burst=false"));
    settled(&world);
    assert_eq!(reaction(rich, "👍"), Some((2, false)));

    // Refused: it shows while Discord is being asked, and is taken back when Discord says no.
    let refused_path = format!("{reactions}/{mine}/reactions/%F0%9F%91%80/%40me");
    world.discord.script(&refused_path, 403, json!({"message": "Reaction blocked", "code": 90001}));
    *world.discord.stall.lock().unwrap() = Some((refused_path.clone(), Duration::from_millis(1500)));
    std::thread::scope(|scope| {
        let asking = scope.spawn(|| react(mine, "👀", true));
        wait("the request to reach Discord", || (!world.discord.requests_to("PUT", &refused_path).is_empty()).then_some(()));
        assert_eq!(reaction(mine, "👀"), Some((1, true)), "shown before Discord answered");
        match asking.join().unwrap() {
            Response::Error { code: ErrorCode::Refused, detail } => assert!(detail.contains("Reaction blocked"), "{detail}"),
            other => panic!("{other:?}"),
        }
    });
    assert_eq!(reaction(mine, "👀"), None);

    // An edit changes nothing until Discord accepts it; then the store has Discord's version.
    let edit = |text: &str| {
        world.ask(Request::Edit { conversation: world.key(dm), message_id: mine.to_owned(), text: text.to_owned() })
    };
    assert_eq!(edit("is the *deploy* done yet?"), Response::Ok);
    let patched = world.discord.requests_to("PATCH", &format!("{reactions}/{mine}"));
    assert_eq!((patched.len(), &patched[0].body), (1, &json!({"content": "is the *deploy* done yet?"})));
    let stored = world.messages(dm).into_iter().find(|stored| stored.id == mine).unwrap();
    assert_eq!((stored.text.as_str(), stored.edited, stored.outgoing), ("is the deploy done yet?", true, true));
    assert_eq!(spanned(&stored, |style| *style == Style::Italic), ["deploy"]);
    assert_eq!(stored.reactions.len(), 1, "what only the first arrival knew is kept");

    world.discord.script(&format!("{reactions}/{mine}"), 403, json!({"message": "Cannot edit this message", "code": 50005}));
    match edit("nope") {
        Response::Error { code: ErrorCode::Refused, detail } => assert!(detail.contains("Cannot edit this message"), "{detail}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(world.messages(dm).into_iter().find(|stored| stored.id == mine).unwrap().text, "is the deploy done yet?");

    // A delete is Discord's to accept too.
    let delete = || world.ask(Request::Delete { conversation: world.key(dm), message_id: "1300000000000000007".to_owned() });
    world.discord.script(&format!("{reactions}/1300000000000000007"), 403, json!({"message": "Missing Permissions", "code": 50013}));
    assert!(matches!(delete(), Response::Error { code: ErrorCode::Refused, .. }));
    assert_eq!(world.messages(dm).len(), 5);
    assert_eq!(delete(), Response::Ok);
    assert_eq!(world.discord.requests_to("DELETE", &format!("{reactions}/1300000000000000007")).len(), 2);
    assert!(world.messages(dm).iter().all(|stored| stored.id != "1300000000000000007"));
}

/// READY where #rules does not let everyone add reactions and #announcements does not let
/// them read its history.
fn reacting_ready() -> String {
    let mut ready: Value = serde_json::from_str(READY).unwrap();
    for channel in ready["guilds"][0]["channels"].as_array_mut().unwrap() {
        let deny: u64 = match channel["id"].as_str() {
            Some("4105") => 1 << 6,
            Some("4102") => 1 << 16,
            _ => continue,
        };
        channel["permission_overwrites"] = json!([{"id": "4000", "type": 0, "allow": "0", "deny": deny.to_string()}]);
    }
    ready.to_string()
}

#[test]
fn reactions_follow_the_channels_permissions_and_what_they_refuse_never_reaches_discord() {
    let world = World::launch(&[], |shared| *shared.ready.lock().unwrap() = reacting_ready());
    wait("the account to connect", || (world.link() == Link::Connected).then_some(()));
    let known = |id: &str| matches!(world.ask(Request::Conversation { conversation: world.key(id) }), Response::Conversation { .. });
    wait("the server's tree", || known("4105").then_some(()));
    let before = world.discord.asked().len();
    let rules = |id: &str| match world.ask(Request::Reactions { conversation: world.key(id) }) {
        Response::Reactions { rules, .. } => rules,
        other => panic!("no rules for {id}: {other:?}"),
    };
    // Without Nitro: any emoji, the server's own custom ones in it and none in a DM, nothing
    // animated, 20 different a message. A thread goes by its channel.
    let dm = rules("3001");
    assert_eq!((dm.set, dm.custom, dm.animated, dm.distinct, dm.join_only), (ReactionSet::Any, EmojiScope::None, false, 20, false));
    assert_eq!((rules("4101").custom, rules("4500").custom), (EmojiScope::Space, EmojiScope::Space));
    assert!(rules("4105").join_only, "no Add Reactions: only what is there can be added to");
    assert_eq!(rules("4102").set, ReactionSet::Off);
    // History being warmed meanwhile is the only thing asked.
    let asked = world.discord.asked();
    assert!(asked[before..].iter().all(|asked| asked.contains("/messages?")), "worked out from READY, nothing asked: {:?}", &asked[before..]);

    let fruit = "🍎🍐🍊🍋🍌🍉🍇🍓🫐🍈🍒🍑🥭🍍🥥🥝🍅🍆🥑🥦";
    let twenty: Vec<Value> = fruit.chars().map(|one| json!({"count": 1, "me": false, "emoji": {"id": null, "name": one.to_string()}})).collect();
    // Each names a role we hold, so it is kept though the channel was never opened.
    let full = world.incoming("4101", Some("4000"), "every fruit", json!({"reactions": twenty, "mention_roles": ["4010"]}));
    let thumbs = json!({"reactions": [{"count": 2, "me": false, "emoji": {"id": null, "name": "👍"}}], "mention_roles": ["4010"]});
    let said = world.incoming("4105", Some("4000"), "be kind", thumbs);
    let news = world.incoming("4102", Some("4000"), "release is out", json!({"mention_roles": ["4010"]}));
    let held = |channel: &str, id: &str| world.messages(channel).iter().any(|message| message.id == id);
    wait("the messages", || (held("4101", &full) && held("4105", &said) && held("4102", &news)).then_some(()));
    let react = |channel: &str, message: &str, emoji: &str| {
        world.ask(Request::React { conversation: world.key(channel), message_id: message.to_owned(), emoji: emoji.to_owned(), on: true })
    };
    let refused = |answer: Response| match answer {
        Response::Error { code: ErrorCode::Refused, detail } => detail,
        other => panic!("not refused: {other:?}"),
    };
    assert_eq!(refused(react("4101", &full, "🦑")), "This message has 20 different reactions, as many as allowed.");
    assert_eq!(refused(react("4105", &said, "🎉")), "You can only add to reactions already on this message.");
    assert_eq!(refused(react("4102", &news, "👍")), "You can't react in this channel.");
    let reacted = || world.discord.requests_to("PUT", "/channels/").into_iter().filter(|seen| seen.path.contains("/reactions/")).count();
    assert_eq!(reacted(), 0, "{:?}", world.discord.asked());
    // What is already there can be added to.
    assert_eq!(react("4101", &full, "🍎"), Response::Ok);
    assert_eq!(react("4105", &said, "👍"), Response::Ok);
    assert_eq!(reacted(), 2);
}

#[test]
fn friendships_presence_and_profiles_are_asked_of_discord_only_when_the_user_does() {
    let world = World::start();
    let relate = |person: &str, action: RelateAction| world.ask(Request::Relate { person: world.key(person), action });
    let named = |relation: Relation| world.people(Some(relation), None).into_iter().map(|person| person.key.id).collect::<Vec<_>>();
    let relationships = "/users/@me/relationships";

    // Connecting touched none of this.
    settled(&world);
    assert!(world.discord.asked().iter().all(|asked| !asked.contains("/users/")), "{:?}", world.discord.asked());
    assert!(world.discord.frames_of(3).is_empty());

    assert_eq!(relate("2003", RelateAction::Accept), Response::Ok);
    let accepted = world.discord.requests_to("PUT", relationships);
    assert_eq!((accepted[0].path.as_str(), &accepted[0].body), ("/users/@me/relationships/2003", &json!({})));
    let context = base64::engine::general_purpose::STANDARD.decode(&accepted[0].headers["x-context-properties"]).unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&context).unwrap(), json!({"location": "Friends"}));
    assert!(named(Relation::Friend).contains(&"2003".to_owned()) && named(Relation::Incoming).is_empty());

    assert_eq!(relate("2004", RelateAction::Cancel), Response::Ok);
    assert_eq!(world.discord.requests_to("DELETE", relationships)[0].path, "/users/@me/relationships/2004");
    assert!(named(Relation::Outgoing).is_empty());

    assert_eq!(relate("2002", RelateAction::Block), Response::Ok);
    assert_eq!(world.discord.requests_to("PUT", relationships)[1].body, json!({"type": 2}));
    assert!(named(Relation::Blocked).contains(&"2002".to_owned()) && !named(Relation::Friend).contains(&"2002".to_owned()));

    assert_eq!(relate("2001", RelateAction::Remove), Response::Ok);
    assert!(!named(Relation::Friend).contains(&"2001".to_owned()));

    // Refused: the list stays as it was, and the reason is Discord's.
    world.discord.script("/users/@me/relationships/2005", 400, json!({"message": "Cannot do that", "code": 80000}));
    match relate("2005", RelateAction::Remove) {
        Response::Error { code: ErrorCode::Refused, detail } => assert!(detail.contains("Cannot do that"), "{detail}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(named(Relation::Blocked).iter().filter(|id| *id == "2005").count(), 1);
    assert_eq!(world.discord.requests_to("PUT", relationships).len() + world.discord.requests_to("DELETE", relationships).len(), 5);

    // Presence and the status line are saved as settings and told to the gateway.
    let present = |presence: Presence, status: Option<&str>| {
        world.ask(Request::SetPresence { account: world.account(), presence, status: status.map(str::to_owned) })
    };
    let saved = || {
        let saved = world.discord.requests_to("PATCH", "/users/@me/settings-proto/1");
        let last = saved.last().unwrap().body["settings"].as_str().unwrap().to_owned();
        (saved.len(), String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD.decode(last).unwrap()).into_owned())
    };
    let shown = || match world.ask(Request::Status) {
        Response::Status { accounts, .. } => (accounts[0].presence, accounts[0].status.clone()),
        other => panic!("{other:?}"),
    };
    assert_eq!(present(Presence::Dnd, Some("heads down")), Response::Ok);
    let (count, settings) = saved();
    assert!(count == 1 && settings.contains("dnd") && settings.contains("heads down"), "{settings:?}");
    let told = wait("the gateway to be told", || world.discord.frames_of(3).pop());
    assert_eq!(told["d"]["status"], "dnd");
    assert_eq!((&told["d"]["activities"][0]["type"], &told["d"]["activities"][0]["state"]), (&json!(4), &json!("heads down")));
    assert_eq!(shown(), (Presence::Dnd, "heads down".to_owned()));

    // Presence alone leaves the line as it is; an empty line clears it.
    assert_eq!(present(Presence::Online, None), Response::Ok);
    let (count, settings) = saved();
    assert!(count == 2 && settings.contains("online") && settings.contains("heads down"), "{settings:?}");
    assert_eq!(present(Presence::Online, Some("")), Response::Ok);
    assert!(!saved().1.contains("heads down"));
    wait("the gateway to hear the line is gone", || {
        world.discord.frames_of(3).pop().filter(|told| told["d"]["activities"] == json!([]))
    });
    assert_eq!(shown(), (Presence::Online, String::new()));

    // A profile is one request, when the card is opened.
    let profile = match world.ask(Request::Profile { person: world.key("2006"), space: None }) {
        Response::Profile { profile } => profile,
        other => panic!("{other:?}"),
    };
    assert_eq!(profile.bio, "Maintains the build. **Tea**, not coffee.");
    assert_eq!(profile.mutual_spaces, ["Rustaceans"], "only servers we are in are named");
    let asked = world.discord.requests_to("GET", "/users/2006/profile");
    assert!(asked.len() == 1 && asked[0].path.contains("with_mutual_guilds=true"), "{asked:?}");
    // The card draws the picture at 80 points: on a 2x screen that is the CDN's 256.
    let before = world.discord.requests_to("GET", "/media/avatars/2006/").len();
    let picture = world.fetched("avatar:2006:s4m", Some(160), false);
    assert!(picture.path.is_some(), "{picture:?}");
    let sizes: Vec<String> = world.discord.requests_to("GET", "/media/avatars/2006/").into_iter().skip(before).map(|seen| seen.path).collect();
    assert_eq!(sizes, ["/media/avatars/2006/s4m.webp?size=256"]);
}

#[test]
fn reading_forward_and_jumping_ask_discord_once_and_what_was_deleted_meanwhile_goes() {
    let world = World::start();
    let dm = "3001";
    wait("history of the conversation", || Some(world.messages(dm)).filter(|messages| messages.len() == 5));
    let page = |page: Page| match world.ask(Request::Messages { conversation: world.key(dm), page, limit: 50 }) {
        Response::Messages { messages, .. } => messages.into_iter().map(|message| message.id).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    let asked = |with: &str| {
        let seen = world.discord.requests_to("GET", "/channels/3001/messages");
        seen.into_iter().filter(|seen| seen.path.contains(with)).map(|seen| seen.path).collect::<Vec<_>>()
    };

    // Reading on from inside what was just fetched asks nothing: the newest page reaches now.
    page(Page::After { id: "1300000000000000006".to_owned() });
    settled(&world);
    assert_eq!(asked("after="), Vec::<String>::new());

    // From before it, Discord is asked for the page after, once. The page it answers with no
    // longer holds a message that is stored: it was deleted while we were away.
    let history: Vec<Value> = serde_json::from_str(history(dm)).unwrap();
    let without: Vec<Value> = history.into_iter().filter(|message| message["id"] != "1300000000000000008").collect();
    world.discord.script("/channels/3001/messages", 200, json!(without));
    page(Page::After { id: "1200000000000000000".to_owned() });
    wait("the page after", || (!asked("after=").is_empty()).then_some(()));
    assert_eq!(asked("after="), ["/channels/3001/messages?after=1200000000000000000&limit=50"]);
    wait("the deleted message to go", || world.messages(dm).iter().all(|message| message.id != "1300000000000000008").then_some(()));
    assert_eq!(world.messages(dm).len(), 4);
    page(Page::After { id: "1200000000000000000".to_owned() });
    settled(&world);
    assert_eq!(asked("after=").len(), 1);

    // A jump to a message that is not stored fetches the page around it, once.
    let old = |id: &str, text: &str| {
        json!({"id": id, "channel_id": dm, "type": 0, "author": me(), "content": text, "mentions": [], "mention_roles": [],
               "mention_everyone": false, "attachments": [], "embeds": [], "pinned": false, "edited_timestamp": null})
    };
    let around = json!([old("1100000000000000003", "after"), old("1100000000000000002", "the one"), old("1100000000000000001", "before")]);
    world.discord.script("/channels/3001/messages", 200, around);
    let jump = || page(Page::Around { id: "1100000000000000002".to_owned() });
    jump();
    wait("the page around", || jump().contains(&"1100000000000000002".to_owned()).then_some(()));
    assert_eq!(asked("around="), ["/channels/3001/messages?around=1100000000000000002&limit=50"]);
    jump();
    settled(&world);
    assert_eq!(asked("around=").len(), 1);
}

#[test]
fn pictures_come_at_the_size_drawn_from_renewed_addresses_as_the_same_browser() {
    let world = World::start();
    let dm = "3002";
    let picture = |id: &str, signed: &str| {
        let url = format!("{}/media/attachments/1/{id}/a.png?ex={signed}&is=1&hm=abc", world.api);
        json!({"id": id, "filename": "a.png", "size": 48213, "content_type": "image/png", "width": 1600, "height": 900,
               "url": url, "proxy_url": url})
    };
    let got = |id: &str| world.discord.requests_to("GET", &format!("/media/attachments/1/{id}/")).into_iter().map(|seen| seen.path).collect::<Vec<_>>();

    // One with a good address: fetched at the size a row shows, as the session's browser,
    // and with nothing of the session.
    world.incoming(dm, None, "look", json!({"attachments": [picture("9001", "7fffffff")]}));
    let message = wait("the message", || world.messages(dm).pop().filter(|message| !message.attachments.is_empty()));
    let thumb = message.attachments[0].thumb.clone().unwrap();
    assert_eq!((thumb.id.as_str(), thumb.width, thumb.height), ("att:9001:thumb", 400, 225));
    let small = world.fetched("att:9001:thumb", None, false);
    assert_eq!(got("9001"), ["/media/attachments/1/9001/a.png?ex=7fffffff&is=1&hm=abc&format=webp&width=400&height=225"]);
    let fetch = world.discord.requests_to("GET", "/media/attachments/1/9001/").remove(0);
    let identify = world.discord.frames_of(2).remove(0);
    assert_eq!(fetch.headers["user-agent"], identify["d"]["properties"]["browser_user_agent"].as_str().unwrap());
    assert!(!fetch.headers.contains_key("authorization") && !fetch.headers.contains_key("x-super-properties"));

    // A 2x screen draws the same 400 points with 800 pixels: the next size up is fetched, to
    // another path, and the small file goes.
    let small_path = small.path.unwrap();
    assert_eq!(world.fetch(&["att:9001:thumb"], Some(800), false)[0].path, None, "not fetched at that size yet");
    let large = wait("the larger size", || world.fetch(&["att:9001:thumb"], Some(800), false).pop().and_then(|file| file.path));
    assert_ne!(large, small_path);
    assert!(got("9001")[1].ends_with("&format=webp&width=800&height=450"), "{:?}", got("9001"));
    wait("the smaller file to go", || (!std::path::Path::new(&small_path).exists()).then_some(()));
    // Asked for small again, it stays large: no third fetch.
    assert_eq!(world.fetch(&["att:9001:thumb"], Some(400), false)[0].path.as_deref(), Some(large.as_str()));
    assert_eq!(got("9001").len(), 2);

    // Six whose addresses ran out, asked for together: renewed in a batch, not one by one,
    // and the expired addresses themselves are never asked.
    let expired: Vec<String> = (9101..9107).map(|id| id.to_string()).collect();
    for id in &expired {
        world.incoming(dm, None, "old", json!({"attachments": [picture(id, "1")]}));
    }
    settled(&world);
    let thumbs: Vec<String> = expired.iter().map(|id| format!("att:{id}:thumb")).collect();
    let thumbs: Vec<&str> = thumbs.iter().map(String::as_str).collect();
    world.fetch(&thumbs, None, false);
    wait("all six to land", || world.fetch(&thumbs, None, false).iter().all(|file| file.path.is_some()).then_some(()));
    let renewals = world.discord.requests_to("POST", "/attachments/refresh-urls");
    let mut renewed: Vec<String> = renewals
        .iter()
        .flat_map(|seen| seen.body["attachment_urls"].as_array().unwrap().iter().map(|url| url.as_str().unwrap().to_owned()))
        .collect();
    assert!(renewals.len() <= 2, "{} requests to renew six addresses", renewals.len());
    assert!(renewed.iter().all(|url| !url.contains("format=webp")), "the attachment's own address is what is renewed");
    renewed.sort();
    let mut distinct = renewed.clone();
    distinct.dedup();
    assert_eq!((renewed.len(), distinct.len()), (6, 6), "{renewed:?}");
    for id in &expired {
        assert_eq!(got(id), [format!("/media/attachments/1/{id}/a.png?ex=7fffffff&is=1&hm=abc&format=webp&width=400&height=225")]);
    }

    // The renewed address was kept: the file gone from disk is fetched again without asking.
    let path = world.fetch(&[thumbs[0]], None, false)[0].path.clone().unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(world.fetch(&[thumbs[0]], None, false)[0].path, None);
    wait("the file again", || world.fetch(&[thumbs[0]], None, false).pop().and_then(|file| file.path));
    assert_eq!(world.discord.requests_to("POST", "/attachments/refresh-urls").len(), renewals.len());

    // One that cannot be renewed fails, says why, and is asked for once however often the
    // shell asks. Only the user's retry asks again.
    world.incoming(dm, None, "gone", json!({"attachments": [picture("lost", "1")]}));
    settled(&world);
    let requests = || world.discord.requests.lock().unwrap().len();
    let failed = world.fetched("att:lost:thumb", None, false);
    assert_eq!((failed.path, failed.failed.as_deref()), (None, Some("The file is no longer there (404).")));
    let asked = requests();
    for _ in 0..5 {
        assert!(world.fetch(&["att:lost:thumb"], None, false)[0].failed.is_some());
    }
    sleep(Duration::from_millis(500));
    assert_eq!(requests(), asked, "a failed file was asked for again");
    world.fetch(&["att:lost:thumb"], None, true);
    wait("the retry to fail too", || (requests() > asked && world.fetch(&["att:lost:thumb"], None, false)[0].failed.is_some()).then_some(()));
    assert_eq!(requests(), asked + 1, "a retry is one more request");
}

#[test]
fn a_run_of_refusals_stops_every_request_and_says_so() {
    let world = World::start();
    let dm = "3001";
    wait("history of the conversation", || Some(world.messages(dm)).filter(|messages| messages.len() == 5));
    let mine = "1300000000000000009";
    world.discord.forbidden.lock().unwrap().push(format!("/channels/{dm}/messages/{mine}/reactions"));
    let react = || world.ask(Request::React { conversation: world.key(dm), message_id: mine.to_owned(), emoji: "👀".to_owned(), on: true });

    // Discord bans an address at 10,000 refusals in ten minutes. This stops at 50.
    for _ in 0..49 {
        assert!(matches!(react(), Response::Error { code: ErrorCode::Refused, .. }));
    }
    assert_eq!(world.required(), "");
    for _ in 0..11 {
        assert!(matches!(react(), Response::Error { code: ErrorCode::Refused, .. }));
    }
    assert_eq!(world.discord.requests_to("PUT", "/channels/3001/messages").len(), 50);
    assert!(world.required().contains("refusing"), "{:?}", world.required());

    // Nothing else is asked either, and what the user tries says why.
    let before = world.discord.requests.lock().unwrap().len();
    world.send("3002", "1730000000000000031", "anyone?", None);
    let why = world.failure("3002", "1730000000000000031");
    assert!(why.contains("refused too many requests") && why.contains("10 min"), "{why}");
    allow_typing(&world, &world.key("3002"));
    world.ask(Request::Typing { conversation: world.key("3002") });
    settled(&world);
    assert_eq!(world.discord.requests.lock().unwrap().len(), before);
}

#[test]
fn a_rate_limit_that_is_not_json_is_waited_out_from_its_headers_and_tried_once_more() {
    let world = World::start();
    // Cloudflare's page: a Retry-After and nothing Discord wrote. Twice.
    for _ in 0..2 {
        world.discord.script_raw("/channels/3002/typing", 429, "Retry-After: 2\r\nContent-Type: text/html\r\n", "<html>error code: 1015</html>");
    }
    allow_typing(&world, &world.key("3002"));
    world.ask(Request::Typing { conversation: world.key("3002") });
    let typed = wait("the second attempt", || Some(world.discord.requests_to("POST", "/channels/3002/typing")).filter(|typed| typed.len() == 2));
    let waited = typed[1].at.duration_since(typed[0].at);
    assert!(waited >= Duration::from_millis(1950), "tried again after {waited:?}");

    // Nobody said whose limit it was, so every route waits it out, not only that one.
    world.send("3001", "1730000000000000032", "after the limit", None);
    assert_eq!(world.outcome("3001", "1730000000000000032").state, SendState::Sent);
    let sent = world.discord.requests_to("POST", "/channels/3001/messages").remove(0);
    let held = sent.at.duration_since(typed[1].at);
    assert!(held >= Duration::from_millis(1900), "another route went after {held:?}");
    // Two attempts, never a third.
    assert_eq!(world.discord.requests_to("POST", "/channels/3002/typing").len(), 2);
}

#[test]
fn three_refused_sessions_in_a_row_stop_the_asking_until_the_user_does_something() {
    let world = World::start();
    let far = |what: &str, mut probe: Box<dyn FnMut() -> bool + '_>| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !probe() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            sleep(Duration::from_millis(50));
        }
    };

    // The session times out (4009), the next READY cannot be read, the one after is refused.
    *world.discord.ready.lock().unwrap() = r#"{"v": 9, "session_id": 7}"#.to_owned();
    world.discord.closes.lock().unwrap().extend([0, 4009]);
    world.discord.control.lock().unwrap().send(Control::Close(4009)).unwrap();
    far("the account to say what stands in the way", Box::new(|| !world.required().is_empty()));
    let required = world.required();
    assert!(required.contains("would not start a session") && required.contains("4009"), "{required}");
    assert_eq!(world.link(), Link::Offline);
    // One good session at the start, then one unreadable and one refused: three identifies.
    assert_eq!(world.discord.frames_of(2).len(), 3);
    let connections = world.discord.connections.load(Ordering::Relaxed);

    // And then nothing, however long it is left alone.
    sleep(Duration::from_secs(20));
    assert_eq!(world.discord.frames_of(2).len(), 3);
    assert_eq!(world.discord.connections.load(Ordering::Relaxed), connections);

    // The user opens a conversation: one more try, which works.
    *world.discord.ready.lock().unwrap() = READY.to_owned();
    world.messages("3002");
    far("the account to connect again", Box::new(|| world.link() == Link::Connected));
    assert_eq!(world.discord.frames_of(2).len(), 4);
    assert_eq!(world.required(), "");
}

#[test]
fn chatd_serves_before_the_build_is_read_and_remembers_the_last_one_read() {
    mediad();
    let build = |world: &World, nth: usize| {
        wait("an identify", || world.discord.frames_of(2).get(nth).map(|identify| identify["d"]["properties"]["client_build_number"].clone()))
    };

    // The page that names the build takes three seconds. chatd answers while it is still
    // waiting for it. That is what is checked, not a time: how long a busy machine takes to
    // start chatd is no part of it.
    let started = Instant::now();
    let mut world = World::launch(&[("TINYCLIENT_DISCORD_LOGIN", "{{API}}/login")], |shared| {
        *shared.login.lock().unwrap() = (Duration::from_secs(3), 200);
    });
    assert_eq!(world.ask(Request::Ping), Response::Pong);
    assert_eq!(world.discord.logins.load(Ordering::Relaxed), 0, "chatd answered only once it had the build");
    assert!(world.discord.frames_of(2).is_empty(), "nothing is said to Discord before the build is known");
    assert_eq!(build(&world, 0), json!(987_654));
    assert!(started.elapsed() >= Duration::from_secs(3));
    wait("the account to connect", || (world.link() == Link::Connected).then_some(()));

    // The next start the page is broken: the build read last time is used, not an older one.
    *world.discord.login.lock().unwrap() = (Duration::ZERO, 500);
    world.restart();
    assert_eq!(build(&world, 1), json!(987_654));

    // And when it hangs, five seconds is all it gets.
    *world.discord.login.lock().unwrap() = (Duration::from_secs(20), 200);
    let restarted = Instant::now();
    world.restart();
    assert_eq!(build(&world, 2), json!(987_654));
    let took = restarted.elapsed();
    assert!(took >= Duration::from_secs(5) && took < Duration::from_secs(9), "identified after {took:?}");
}

#[test]
fn who_may_see_a_channel_follows_roles_membership_and_categories_live() {
    let world = World::start();
    let server = "4000";
    let rows = || world.tree(server).1.iter().map(|row| row.key.id.clone()).collect::<Vec<_>>();
    let has = |id: &str| rows().iter().any(|row| row == id);
    let groups = || world.tree(server).0;
    let member = |roles: Value| json!({"guild_id": server, "user": {"id": "1000", "username": "justin"}, "roles": roles, "nick": null});
    let role = |id: &str, permissions: &str| json!({"guild_id": server, "role": {"id": id, "name": "role", "permissions": permissions}});
    let ready: Value = serde_json::from_str(READY).unwrap();
    let channel = |id: &str| {
        let mut channel = ready["guilds"][0]["channels"].as_array().unwrap().iter().find(|channel| channel["id"] == id).unwrap().clone();
        channel["guild_id"] = json!(server);
        channel
    };

    // As READY had it: we are a mod, not on the core team. Channels behind a role are locked.
    assert!(has("4104") && !has("4103") && !has("4301"));
    assert!(world.conversation("4104").private && !world.conversation("4101").private);
    assert_eq!(world.conversation("4101").mentions, 1);

    // We are given the core role: the channel behind it appears, and nothing else changes.
    world.discord.push("GUILD_MEMBER_UPDATE", member(json!(["4010", "4011"])));
    wait("the core team's channel", || has("4103").then_some(()));
    assert!(has("4104") && has("4500"), "{:?}", rows());
    assert_eq!(world.conversation("4101").mentions, 1, "unread state survives the tree being worked out again");
    // We lose the mod role: its channel goes.
    world.discord.push("GUILD_MEMBER_UPDATE", member(json!(["4011"])));
    wait("the mods' channel to go", || (!has("4104")).then_some(()));
    assert!(has("4103"));

    // A role we hold becomes administrator: everything is visible.
    world.discord.push("GUILD_ROLE_UPDATE", role("4011", "8"));
    wait("the hidden channels", || (has("4301") && has("4104")).then_some(()));
    // And stops being one.
    world.discord.push("GUILD_ROLE_UPDATE", role("4011", "0"));
    wait("them to go again", || (!has("4301") && !has("4104")).then_some(()));

    // A channel we may read but not write in says so; its topic is carried.
    let mut general = channel("4101");
    general["topic"] = json!("Anything  goes.\nBe kind.");
    general["permission_overwrites"] = json!([{"id": server, "type": 0, "allow": "0", "deny": "2048"}]);
    world.discord.push("CHANNEL_UPDATE", general);
    wait("the channel to be read only", || world.conversation("4101").read_only.then_some(()));
    assert_eq!(world.conversation("4101").topic, "Anything goes. Be kind.");

    // A category renamed is renamed at once.
    let mut text = channel("4100");
    text["name"] = json!("Chat");
    world.discord.push("CHANNEL_UPDATE", text);
    wait("the category's new name", || groups().contains(&"Chat".to_owned()).then_some(()));
    assert!(!groups().contains(&"Text".to_owned()), "{:?}", groups());

    // A new category shows with the first channel we may see in it, and goes when deleted.
    let events = json!({"id": "4400", "type": 4, "name": "Events", "position": 9, "guild_id": server, "permission_overwrites": []});
    world.discord.push("CHANNEL_CREATE", events.clone());
    world.discord.push("CHANNEL_CREATE", json!({"id": "4401", "type": 0, "name": "meetups", "position": 0, "parent_id": "4400",
        "guild_id": server, "last_message_id": null, "permission_overwrites": []}));
    wait("the new category", || groups().contains(&"Events".to_owned()).then_some(()));
    let meetups = world.conversation("4401");
    assert_eq!((meetups.group.as_deref(), meetups.group_title.as_str()), (Some("4400"), "Events"));
    assert_eq!(rows().last().map(String::as_str), Some("4401"), "the last category's channel is the last row");
    world.discord.push("CHANNEL_DELETE", events);
    wait("the category to go", || (!groups().contains(&"Events".to_owned())).then_some(()));
}

#[test]
fn member_search_is_paced_per_server_and_remembered() {
    let world = World::start();
    let searches = || world.discord.frames_of(8);
    let search = |query: &str| world.people(None, Some(query));

    // With no server on screen there is nobody to ask, and one letter is never asked about.
    search("sa");
    sleep(Duration::from_secs(1));
    world.tree("4000");
    wait("the server to be subscribed", || world.discord.frames_of(37).pop());
    search("s");
    sleep(Duration::from_secs(2));
    assert_eq!(searches().len(), 0);

    search("sandy");
    let first = wait("the search", || searches().pop());
    assert_eq!((&first["d"]["query"], &first["d"]["guild_id"], &first["d"]["limit"]), (&json!("sandy"), &json!(["4000"]), &json!(25)));
    let sandy = json!({"id": "2099", "username": "sandy", "global_name": "Sandy Kim", "discriminator": "0", "avatar": null});
    world.discord.push("GUILD_MEMBERS_CHUNK", json!({"guild_id": "4000", "members": [{"user": sandy, "roles": []}], "presences": [],
        "chunk_index": 0, "chunk_count": 1, "nonce": first["d"]["nonce"]}));
    wait("the result", || search("sandy").into_iter().find(|person| person.key.id == "2099"));

    // Typed on: the server was searched a moment ago, so this one waits its thirty seconds.
    search("sandra");
    sleep(Duration::from_secs(3));
    assert_eq!(searches().len(), 1);
    let deadline = Instant::now() + Duration::from_secs(40);
    while searches().len() < 2 {
        assert!(Instant::now() < deadline, "the search that waited was never sent");
        sleep(Duration::from_millis(100));
    }
    let second = searches().pop().unwrap();
    assert_eq!(second["d"]["query"], "sandra");
    let apart = second["_at"].as_u64().unwrap() - first["_at"].as_u64().unwrap();
    assert!(apart >= 30_000, "the same server was searched again after {apart} ms");
}

#[test]
fn signing_out_ends_the_session_on_discord_before_it_is_forgotten() {
    let world = World::start();
    let answered = world.ask(Request::SignOut { account: world.account() });
    assert!(matches!(answered, Response::Status { .. }), "{answered:?}");
    let ended = world.discord.requests_to("POST", "/auth/logout");
    assert_eq!(ended.len(), 1, "the session was ended before the answer");
    assert_eq!(ended[0].headers["authorization"], TOKEN);
    assert_eq!(world.link(), Link::SignedOut);
}

/// A READY as a long-lived account's: 100 servers of 30 channels, 1,000 relationships, 200
/// direct conversations of which 40 are unread, and everything a real one carries that is never
/// read, so the text is megabytes.
fn large_ready() -> String {
    let user = |id: u64| json!({"id": id.to_string(), "username": format!("user{id}"), "global_name": format!("User {id}"),
        "discriminator": "0", "avatar": format!("{id:032x}"), "avatar_decoration_data": null, "public_flags": 64,
        "clan": null, "primary_guild": null, "banner": null, "accent_color": null});
    let users: Vec<Value> = (0..1200u64).map(|n| user(200_000 + n)).collect();
    let relationships: Vec<Value> = (0..1000u64)
        .map(|n| json!({"id": (200_000 + n).to_string(), "user_id": (200_000 + n).to_string(), "type": 1, "nickname": null,
            "since": "2023-01-05T10:00:00+00:00", "is_spam_request": false, "user_ignored": false}))
        .collect();
    let snowflake = |n: u64| ((1_730_000_000_000u64 - 1_420_070_400_000 + n) << 22).to_string();
    let private: Vec<Value> = (0..200u64)
        .map(|n| json!({"id": (300_000 + n).to_string(), "type": 1, "recipient_ids": [(200_000 + n).to_string()],
            "last_message_id": snowflake(n * 1000 + 500), "flags": 0, "is_spam": false, "safety_warnings": []}))
        .collect();
    let mut read: Vec<Value> = (0..200u64)
        .map(|n| json!({"id": (300_000 + n).to_string(), "mention_count": u64::from(n < 40),
            "last_message_id": snowflake(n * 1000 + if n < 40 { 0 } else { 500 }), "flags": 0, "last_viewed": 3600}))
        .collect();
    let mut guilds = Vec::new();
    let mut merged = Vec::new();
    for guild in 0..100u64 {
        let id = 1_000_000 + guild * 1000;
        let mut channels = Vec::new();
        for category in 0..3u64 {
            channels.push(json!({"id": (id + 900 + category).to_string(), "type": 4, "name": format!("Category {category}"),
                "position": category, "permission_overwrites": [], "flags": 0}));
        }
        for channel in 0..27u64 {
            let hidden = channel % 9 == 0;
            channels.push(json!({
                "id": (id + channel + 1).to_string(), "type": if channel % 10 == 9 { 2 } else { 0 },
                "name": format!("channel-{channel}"), "position": channel, "parent_id": (id + 900 + channel % 3).to_string(),
                "topic": "What this channel is for, in a sentence or two, as most servers write one.",
                "last_message_id": snowflake(guild * 100 + channel), "rate_limit_per_user": 0, "nsfw": false, "flags": 0,
                "icon_emoji": {"id": null, "name": "💬"}, "theme_color": null, "default_thread_rate_limit_per_user": 0,
                "permission_overwrites": [
                    {"id": id.to_string(), "type": 0, "allow": "0", "deny": if hidden { "1024" } else { "0" }},
                    {"id": (id + 500).to_string(), "type": 0, "allow": "1024", "deny": "0"},
                ],
            }));
            read.push(json!({"id": (id + channel + 1).to_string(), "mention_count": 0, "last_message_id": snowflake(0), "flags": 1}));
        }
        let roles: Vec<Value> = (0..20u64)
            .map(|role| json!({"id": if role == 0 { id.to_string() } else { (id + 500 + role).to_string() },
                "name": format!("role {role}"), "permissions": "3072", "position": role, "color": 3_447_003, "hoist": false,
                "managed": false, "mentionable": false, "flags": 0, "icon": null, "unicode_emoji": null}))
            .collect();
        // What READY carries for every server and nobody here reads.
        let emojis: Vec<Value> = (0..120u64)
            .map(|emoji| json!({"id": (id * 1000 + emoji).to_string(), "name": format!("emoji_{emoji}"), "roles": [],
                "require_colons": true, "managed": false, "animated": false, "available": true}))
            .collect();
        let stickers: Vec<Value> = (0..20u64)
            .map(|sticker| json!({"id": (id * 2000 + sticker).to_string(), "name": format!("sticker {sticker}"), "tags": "smile",
                "type": 2, "format_type": 1, "description": "A sticker somebody drew for this server.", "asset": "", "available": true}))
            .collect();
        guilds.push(json!({
            "id": id.to_string(), "joined_at": "2022-03-01T12:00:00+00:00", "large": true, "lazy": true, "member_count": 51234,
            "premium_subscription_count": 14, "data_mode": "full", "version": 1_729_000_000_000u64,
            "properties": {"name": format!("Server {guild}"), "icon": format!("{id:032x}"), "owner_id": "9999",
                "description": "A community for people who like this sort of thing.", "features": ["COMMUNITY", "NEWS", "THREADS_ENABLED"],
                "verification_level": 2, "preferred_locale": "en-US", "system_channel_id": (id + 1).to_string()},
            "roles": roles, "channels": channels, "threads": [], "emojis": emojis, "stickers": stickers,
            "stage_instances": [], "guild_scheduled_events": [], "activity_instances": [],
        }));
        merged.push(json!([{"user_id": "1000", "roles": [(id + 501).to_string()], "nick": null, "joined_at": "2022-03-01T12:00:00+00:00", "flags": 0}]));
    }
    json!({
        "v": 9, "user": me(), "session_id": "session-large", "session_type": "normal",
        "resume_gateway_url": "{{GATEWAY}}", "users": users, "relationships": relationships, "private_channels": private,
        "read_state": {"version": 1, "partial": false, "entries": read},
        "user_guild_settings": {"version": 1, "partial": false, "entries": []},
        "merged_members": merged, "guilds": guilds, "sessions": [], "experiments": [], "guild_experiments": [],
    })
    .to_string()
}

#[test]
fn a_ready_of_real_size_connects_quickly_within_the_memory_budget_and_is_not_kept() {
    mediad();
    let ready = large_ready();
    assert!(ready.len() > 3 << 20, "the fixture is only {} bytes", ready.len());
    let started = Instant::now();
    let world = World::launch(&[], |shared| *shared.ready.lock().unwrap() = ready.clone());
    let rss = || {
        process_rss_kb(world.child.id())
    };
    // Sampled through the whole connect: the peak is while READY is held as text and parsed.
    let mut peak = 0;
    let deadline = Instant::now() + Duration::from_secs(60);
    while world.link() != Link::Connected {
        peak = peak.max(rss());
        assert!(Instant::now() < deadline, "the large account never connected");
        sleep(Duration::from_millis(10));
    }
    let connected = started.elapsed();
    peak = peak.max(rss());

    // Everything in it is there to read.
    let spaces = match world.ask(Request::Spaces { network: Some(Network::Discord) }) {
        Response::Spaces { spaces } => spaces,
        other => panic!("{other:?}"),
    };
    assert_eq!(spaces.len(), 100);
    // 27 channels, three of them hidden from us.
    assert_eq!(world.tree("1000000").1.len(), 24);
    assert_eq!(world.people(Some(Relation::Friend), None).len(), 100, "a page of the thousand friends");

    // Settled: the megabytes of READY are gone again. macOS hands freed pages back, so RSS
    // falls by about the payload. glibc keeps them, so RSS stays near the peak instead.
    sleep(Duration::from_secs(5));
    let settled = rss();
    let payload = ready.len() as u64 / 1024;
    eprintln!("READY of {payload} KB: connected in {connected:?}, peak {peak} KB, settled {settled} KB");
    assert!(connected < Duration::from_secs(8), "connecting took {connected:?}");
    // The budget for the peak while connecting (docs/product/07-budgets-and-risk.md).
    assert!(peak < 64 * 1024, "peak RSS {peak} KB");
    let released = settled + payload < peak + 4 * 1024 || settled < 32 * 1024;
    let kept_by_allocator = settled < 64 * 1024 && settled < peak.saturating_add(8 * 1024);
    assert!(released || kept_by_allocator, "settled at {settled} KB after a peak of {peak} KB");

    // The first thirty seconds: five unread conversations fetched ahead, at a person's pace,
    // and nothing else asked of Discord at all.
    sleep(Duration::from_secs(30).saturating_sub(started.elapsed()));
    let requests = world.discord.requests.lock().unwrap().clone();
    let paths: Vec<&str> = requests.iter().map(|seen| seen.path.as_str()).collect();
    assert_eq!(requests.len(), 5, "{paths:?}");
    assert!(requests.iter().all(|seen| seen.method == "GET" && seen.path.starts_with("/channels/3000") && seen.path.ends_with("/messages?limit=50")), "{paths:?}");
    for pair in requests.windows(2) {
        let gap = pair[1].at.duration_since(pair[0].at);
        assert!(gap >= Duration::from_millis(1100), "two fetches {gap:?} apart");
    }
    let sent: Vec<u64> = world.discord.frames.lock().unwrap().iter().filter_map(|frame| frame["op"].as_u64()).filter(|op| *op != 1).collect();
    assert_eq!(sent, [2, 37], "one identify, heartbeats, and the one server this test opened");
}

#[test]
fn forwards_voice_messages_spoilers_stickers_and_replies_fill_the_model() {
    let world = World::start();
    let dm = "3002";
    let latest = |text: &str| wait("the message", || world.messages(dm).into_iter().rev().find(|message| message.text == text));
    let picture = |name: &str| {
        let url = format!("{}/media/attachments/1/7/{name}?ex=7fffffff&is=1&hm=abc", world.api);
        json!({"id": "9500", "filename": name, "size": 100, "content_type": "image/png", "width": 300, "height": 200, "url": url, "proxy_url": url})
    };

    // A forward: what was said is the text, where it was said rides beside it.
    let origin = json!({"type": 1, "channel_id": "4101", "guild_id": "4000", "message_id": "1300000000000000100"});
    let snapshot = json!([{"message": {"content": "**ship it** on Monday", "attachments": [], "embeds": [], "mentions": [], "type": 0}}]);
    world.incoming(dm, None, "", json!({"message_reference": origin, "message_snapshots": snapshot}));
    let forward = latest("ship it on Monday");
    let from = forward.forwarded.as_deref().unwrap();
    assert_eq!((from.from_title.as_str(), from.from_space.as_str()), ("#general", "Rustaceans"));
    assert_eq!(from.link, "https://discord.com/channels/4000/4101/1300000000000000100");
    assert_eq!(spanned(&forward, |style| *style == Style::Bold), ["ship it"]);
    assert_eq!(forward.kind, MessageKind::Normal);

    // A voice message: its bars and its length.
    let voice = json!({"id": "9501", "filename": "voice-message.ogg", "size": 9000, "content_type": "audio/ogg",
        "url": format!("{}/media/attachments/1/9501/voice-message.ogg?ex=7fffffff&is=1&hm=abc", world.api),
        "duration_secs": 3.4, "waveform": "AID/"});
    world.incoming(dm, None, "listen", json!({"attachments": [voice], "flags": 8192}));
    let spoken = latest("listen");
    assert_eq!((spoken.attachments[0].waveform.as_slice(), spoken.attachments[0].duration), (&[0u8, 128, 255][..], 3));
    assert_eq!(spoken.attachments[0].file.kind, MediaKind::Audio);

    // A spoiler is marked, by its flag or by its name; a sticker is a sticker.
    let mut flagged = picture("cat.png");
    flagged["flags"] = json!(8);
    world.incoming(dm, None, "careful", json!({"attachments": [picture("SPOILER_dog.png"), flagged, picture("plain.png")]}));
    let hidden: Vec<bool> = latest("careful").attachments.iter().map(|attachment| attachment.spoiler).collect();
    assert_eq!(hidden, [true, true, false]);
    world.incoming(dm, None, "hi", json!({"sticker_items": [{"id": "77", "name": "wave", "format_type": 1}]}));
    let waved = latest("hi");
    assert!(waved.attachments[0].sticker && !waved.attachments[0].spoiler);
    assert_eq!((waved.attachments[0].file.width, waved.attachments[0].name.as_str()), (160, "wave"));

    // A reply names who it answers, for their picture; one whose original is gone says so.
    let original = json!({"id": "1300000000000000020", "channel_id": dm, "type": 0, "content": "first",
        "author": {"id": "2006", "username": "sam", "global_name": "Sam Whitlock", "discriminator": "0", "avatar": "s4m"}});
    let answering = json!({"type": 0, "channel_id": dm, "message_id": "1300000000000000020"});
    world.incoming(dm, None, "second", json!({"type": 19, "message_reference": answering, "referenced_message": original}));
    let reply = latest("second").reply_to.unwrap();
    assert_eq!((reply.person, reply.deleted, reply.snippet.as_str()), (Some(world.key("2006")), false, "first"));
    let answering = json!({"type": 0, "channel_id": dm, "message_id": "1300000000000000021"});
    world.incoming(dm, None, "third", json!({"type": 19, "message_reference": answering, "referenced_message": null}));
    let reply = latest("third").reply_to.unwrap();
    assert_eq!((reply.id.as_str(), reply.person, reply.deleted, reply.snippet.as_str()), ("1300000000000000021", None, true, ""));

    // People open in Discord itself, and a thread says what it hangs off.
    let friends = world.people(Some(Relation::Friend), None);
    assert_eq!(friends.iter().find(|person| person.key.id == "2001").unwrap().link, "https://discord.com/users/2001");
    let thread = world.conversation("4500");
    assert_eq!((thread.parent.as_deref(), thread.root.as_deref(), thread.space.as_deref()), (Some("4101"), Some("4500"), Some("4000")));
}

#[test]
fn a_challenge_stops_every_request_and_shows_on_the_account() {
    let world = World::start();
    // Discord answers a send with a captcha. It is never retried, and nothing else is asked.
    world.discord.script("/channels/3002/messages", 400, json!({"captcha_key": ["captcha-required"], "captcha_sitekey": "abc", "captcha_service": "hcaptcha"}));
    world.send("3002", "1730000000000000041", "hello", None);
    assert!(world.failure("3002", "1730000000000000041").contains("Finish it in Discord"));
    assert!(world.required().contains("captcha"), "{:?}", world.required());
    let asked = world.discord.requests.lock().unwrap().len();
    world.send("3002", "1730000000000000042", "hello again", None);
    world.failure("3002", "1730000000000000042");
    allow_typing(&world, &world.key("3002"));
    world.ask(Request::Typing { conversation: world.key("3002") });
    world.messages("3004");
    settled(&world);
    assert_eq!(world.discord.requests.lock().unwrap().len(), asked);

    // Discord says the account is in order again, and then wants a phone number.
    world.discord.push("USER_REQUIRED_ACTION_UPDATE", json!({"required_action": null}));
    wait("the account to be clear", || world.required().is_empty().then_some(()));
    world.discord.push("USER_REQUIRED_ACTION_UPDATE", json!({"required_action": "REQUIRE_VERIFIED_PHONE"}));
    wait("the account to say what Discord wants", || world.required().contains("phone number").then_some(()));
}

/// READY as the polish round reads it: server folders, a server's hide-muted and opt-in
/// settings, a muted category, a message request, a bot's and Discord's own DM, and a server's
/// emoji and stickers. Everything else is the usual fixture.
fn polished_ready() -> String {
    let mut ready: Value = serde_json::from_str(READY).unwrap();
    ready["user"]["premium_type"] = json!(0);
    ready["user_settings"] = json!({"guild_folders": [
        {"id": 77, "name": "Games", "color": 0xff0000, "guild_ids": ["4000"]},
        {"id": null, "name": null, "color": null, "guild_ids": ["5000"]},
    ]});
    let rustaceans = &mut ready["user_guild_settings"]["entries"][0];
    rustaceans["hide_muted_channels"] = json!(true);
    // "Show all channels" off: #general and the Text category's #rules are opted in; the Voice
    // category is muted.
    rustaceans["flags"] = json!(1 << 14);
    let overrides = rustaceans["channel_overrides"].as_array_mut().unwrap();
    overrides.push(json!({"channel_id": "4101", "muted": false, "mute_config": null, "message_notifications": 3, "flags": 1 << 12}));
    overrides.push(json!({"channel_id": "4200", "muted": true, "mute_config": null, "message_notifications": 3, "flags": 0}));
    let users = ready["users"].as_array_mut().unwrap();
    users.push(json!({"id": "2007", "username": "stranger", "global_name": "A Stranger", "discriminator": "0", "avatar": null}));
    users.push(json!({"id": "2008", "username": "deploybot", "global_name": null, "discriminator": "0", "avatar": null, "bot": true}));
    users.push(json!({"id": "643945264868098049", "username": "discord", "global_name": "Discord", "discriminator": "0", "avatar": null, "system": true}));
    let private = ready["private_channels"].as_array_mut().unwrap();
    private.push(json!({"id": "3005", "type": 1, "recipient_ids": ["2007"], "last_message_id": "1300000000000000050", "is_message_request": true, "is_message_request_timestamp": "2026-10-13T08:00:00+00:00"}));
    private.push(json!({"id": "3006", "type": 1, "recipient_ids": ["2008"], "last_message_id": "1300000000000000060"}));
    private.push(json!({"id": "3007", "type": 1, "recipient_ids": ["643945264868098049"], "last_message_id": "1300000000000000070"}));
    let rustaceans = &mut ready["guilds"][0];
    rustaceans["emojis"] = json!([
        {"id": "5101", "name": "ferris", "animated": false, "available": true},
        {"id": "5102", "name": "lost_boost", "animated": false, "available": false},
        {"id": "5103", "name": "party_blob", "animated": true, "available": true},
        {"id": "5104", "name": "core_only", "animated": false, "available": true, "roles": ["4011"]},
    ]);
    rustaceans["stickers"] = json!([{"id": "5201", "name": "wave", "format_type": 1}, {"id": "5202", "name": "dance", "format_type": 3}]);
    let general = rustaceans["channels"].as_array_mut().unwrap().iter_mut().find(|channel| channel["id"] == "4101").unwrap();
    general["topic"] = json!("Anything goes.\nKeep it kind.");
    ready.to_string()
}

fn polished() -> World {
    let world = World::launch(&[], |shared| *shared.ready.lock().unwrap() = polished_ready());
    wait("the account to connect", || (world.link() == Link::Connected).then_some(()));
    world
}

#[test]
fn the_home_follows_discords_own_folders_mutes_opt_in_requests_threads_and_plan() {
    let world = polished();
    // The server folders as the user made them; a server alone is in none.
    let folders = match world.ask(Request::Folders { network: Network::Discord }) {
        Response::Folders { folders } => folders,
        other => panic!("{other:?}"),
    };
    let named: Vec<(&str, &str, Option<u32>, Vec<&str>)> =
        folders.iter().map(|folder| (folder.key.id.as_str(), folder.name.as_str(), folder.color, folder.spaces.iter().map(String::as_str).collect())).collect();
    assert_eq!(named, [("77", "Games", Some(0xff0000), vec!["4000"])]);
    let spaces = match world.ask(Request::Spaces { network: Some(Network::Discord) }) {
        Response::Spaces { spaces } => spaces,
        other => panic!("{other:?}"),
    };
    let space = |id: &str| spaces.iter().find(|space| space.key.id == id).unwrap().clone();
    assert_eq!((space("4000").folder.as_deref(), space("4000").muted, space("4000").hide_muted), (Some("77"), false, true));
    assert_eq!((space("5000").folder, space("5000").muted, space("5000").hide_muted), (None, true, false));

    // The tree marks what is not opted in, and the muted category; our thread is followed.
    let asked = Request::Tree { space: world.key("4000"), offset: 0, limit: 500, peek: false };
    let Response::Tree { groups, conversations, .. } = world.ask(asked) else { panic!("no tree") };
    let group = |name: &str| groups.iter().find(|group| group.name == name).unwrap_or_else(|| panic!("{name} not in {groups:?}"));
    assert!(group("Voice").muted && !group("Text").muted);
    let row = |id: &str| conversations.iter().find(|row| row.key.id == id).unwrap_or_else(|| panic!("{id} is not in the tree"));
    let shown: Vec<&str> = conversations.iter().filter(|row| !row.opted_out && row.kind != ConversationKind::Thread).map(|row| row.key.id.as_str()).collect();
    assert_eq!(shown, ["4101"], "#general shows; the rest wait under Browse");
    assert!(row("4201").opted_out && row("4102").opted_out && conversations.len() > 3);
    assert!(world.conversation("4500").followed, "a thread READY lists is one we are in");

    // A message request waits behind Requests; a bot's DM is an app's; Discord's own is read-only.
    let home = |shelf| match world.ask(Request::Home { network: Network::Discord, account: None, folder: None, shelf, offset: 0, limit: 50 }) {
        Response::Conversations { conversations, .. } => conversations.into_iter().map(|row| row.key.id).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(home(protocol::Shelf::Requests), ["3005"]);
    assert!(!home(protocol::Shelf::Main).contains(&"3005".to_owned()));
    assert_eq!(world.conversation("3005").special, protocol::Special::Request);
    assert!(world.conversation("3006").bot && !world.conversation("3001").bot);
    let system = world.conversation("3007");
    assert_eq!((system.special, system.read_only, system.call), (protocol::Special::Service, true, CallSupport::None));

    // What the free plan may send.
    let can = match world.ask(Request::Status) {
        Response::Status { accounts, .. } => accounts.into_iter().find(|account| account.key == world.account()).unwrap().can,
        other => panic!("{other:?}"),
    };
    assert_eq!((can.max_file_bytes, can.max_files, can.custom_emoji), (10 << 20, 10, protocol::EmojiScope::Space));
    assert!(can.gifs && can.stickers && can.spoiler && can.silent && can.suppress_embeds && can.reply_ping && !can.schedule);

    // Friends: in voice (from the voice states) and playing (from presence). The voice channel
    // with a friend in it is told although no server is on screen. Both come in
    // READY_SUPPLEMENTAL, which may still be on its way when the account shows connected.
    let around = wait("Dev to be in a call", || match world.ask(Request::Around { network: Some(Network::Discord) }) {
        Response::Around { in_call, online, .. } => in_call.iter().any(|person| person.key.id == "2002").then_some((in_call, online)),
        other => panic!("{other:?}"),
    });
    let dev = around.0.iter().find(|person| person.key.id == "2002").expect("Dev is in a call");
    let voice = dev.activity.as_deref().unwrap();
    assert_eq!((voice.kind, voice.name.as_str(), voice.place.as_str()), (protocol::ActivityKind::InVoice, "Lounge", "Rustaceans"));
    assert_eq!(voice.conversation, Some(world.key("4201")));
    let mara = around.1.iter().find(|person| person.key.id == "2001").expect("Mara is online");
    let playing = mara.activity.as_deref().unwrap();
    assert_eq!((playing.kind, playing.name.as_str()), (protocol::ActivityKind::Playing, "Factorio"));
    let in_voice = match world.ask(Request::InVoice { network: Some(Network::Discord) }) {
        Response::Conversations { conversations, .. } => conversations,
        other => panic!("{other:?}"),
    };
    let lounge = in_voice.iter().find(|row| row.key.id == "4201").expect("the friend's voice channel");
    assert_eq!(lounge.occupants.as_ref().map(|occupants| occupants.people[0].person.id.as_str()), Some("2002"));
    // Dev leaves: the activity and the channel go.
    world.discord.push("VOICE_STATE_UPDATE", json!({"user_id": "2002", "guild_id": "4000", "channel_id": null, "session_id": "voice-dev"}));
    wait("Dev to leave the call", || match world.ask(Request::Around { network: Some(Network::Discord) }) {
        Response::Around { in_call, .. } => in_call.is_empty().then_some(()),
        _ => None,
    });

    // Opening the request reads nothing on Discord.
    wait("the unread DM to be warmed", || world.discord.requests_to("GET", "/channels/3001/messages").first().cloned());
    sleep(Duration::from_secs(3));
    let before = world.discord.asked();
    assert_eq!(world.ask(Request::MarkRead { conversation: world.key("3005"), message_id: "1300000000000000050".into() }), Response::Ok);
    world.ask(Request::MarkRead { conversation: world.key("3001"), message_id: "1300000000000000010".into() });
    wait("the other mark", || world.discord.requests_to("POST", "/channels/3001/messages/").first().cloned());
    assert!(world.discord.requests_to("POST", "/channels/3005/").is_empty(), "no ack for a request");

    // Connecting asked for the one unread DM's history and nothing else; rows on screen with no
    // preview ask nothing at all on Discord.
    assert_eq!(before, ["GET /channels/3001/messages?limit=50"]);
    let rows: Vec<Key> = ["3002", "3003", "3005", "3006", "3007"].iter().map(|id| world.key(id)).collect();
    world.ask(Request::Watch(Watching { rows, ..Watching::default() }));
    sleep(Duration::from_secs(2));
    let after: Vec<String> = world.discord.asked().into_iter().filter(|asked| !asked.contains("/ack")).collect();
    assert_eq!(after, before, "no preview is fetched on Discord");
}

#[test]
fn info_mute_leave_pins_and_the_shared_search_ask_discord_once_each_and_only_when_asked() {
    let world = polished();
    let asked_now = || world.discord.asked().len();

    // A profile opened inside a server: nickname, roles by name (the everyone role left out),
    // the join date, pronouns. One request, with the server named.
    let profile = match world.ask(Request::Profile { person: world.key("2006"), space: Some(world.key("4000")) }) {
        Response::Profile { profile } => profile,
        other => panic!("{other:?}"),
    };
    assert_eq!((profile.nickname.as_str(), profile.pronouns.as_str()), ("Sammy", "they/them"));
    assert_eq!(profile.joined, Some(1_677_672_000));
    assert_eq!(profile.roles.len(), 1, "{:?}", profile.roles);
    let profiles = world.discord.requests_to("GET", "/users/2006/profile");
    assert_eq!(profiles.len(), 1);
    assert!(profiles[0].path.contains("guild_id=4000") && profiles[0].path.contains("with_mutual_friends=false"));

    // A channel's info is what READY said: nothing is asked.
    let before = asked_now();
    let info = match world.ask(Request::ConversationInfo { conversation: world.key("4101") }) {
        Response::ConversationInfo { info } => info,
        other => panic!("{other:?}"),
    };
    assert_eq!(asked_now(), before, "a channel's info asks Discord nothing");
    assert_eq!(info.notify, protocol::Notify::Mentions, "the server's own level");
    assert_eq!(info.description, "Anything goes.\nKeep it kind.", "the topic whole, where the row has one line");
    assert_eq!(world.conversation("4101").topic, "Anything goes. Keep it kind.");

    // A server's info: one request with counts; our nickname from READY.
    let space = match world.ask(Request::SpaceInfo { space: world.key("4000") }) {
        Response::SpaceInfo { info } => info,
        other => panic!("{other:?}"),
    };
    assert_eq!((space.members, space.online, space.description.as_str()), (Some(1204), Some(230), "Crabs and their friends"));
    assert_eq!(space.nickname, "jscroggins");
    assert_eq!(world.discord.requests_to("GET", "/guilds/4000").len(), 1);

    // Mute: the server's settings for that one channel, as Discord's own menu sends them; the
    // row follows Discord's answer.
    assert_eq!(world.ask(Request::SetMute { conversation: world.key("4101"), notify: protocol::Notify::Nothing }), Response::Ok);
    let patched = world.discord.requests_to("PATCH", "/users/@me/guilds/4000/settings");
    assert_eq!(patched.len(), 1);
    assert_eq!(patched[0].body["channel_overrides"]["4101"]["muted"], true);
    assert_eq!(patched[0].body["channel_overrides"]["4101"]["mute_config"]["selected_time_window"], -1);
    assert!(world.conversation("4101").muted);
    assert_eq!(world.ask(Request::SetMute { conversation: world.key("3001"), notify: protocol::Notify::Mentions }), Response::Ok);
    let direct = world.discord.requests_to("PATCH", "/users/@me/guilds/@me/settings");
    assert_eq!(direct[0].body["channel_overrides"]["3001"], json!({"muted": false, "message_notifications": 1, "mute_config": {"end_time": null, "selected_time_window": -1}}));

    // Leave a group, close a DM: one DELETE each, and the row goes. A server channel is neither.
    assert_eq!(world.ask(Request::Leave { conversation: world.key("3003") }), Response::Ok);
    assert_eq!(world.ask(Request::CloseDirect { conversation: world.key("3002") }), Response::Ok);
    let deleted: Vec<String> = world.discord.requests_to("DELETE", "/channels/").into_iter().map(|seen| seen.path).collect();
    assert_eq!(deleted, ["/channels/3003?silent=false", "/channels/3002"]);
    assert!(matches!(world.ask(Request::Conversation { conversation: world.key("3003") }), Response::Error { .. }));
    assert!(matches!(world.ask(Request::Leave { conversation: world.key("4101") }), Response::Error { .. }));
    assert_eq!(world.discord.requests_to("DELETE", "/channels/").len(), 2, "refused here, not asked");

    // Pins, from the current route.
    let pinned = match world.ask(Request::Pinned { conversation: world.key("4101") }) {
        Response::Pinned { messages, .. } => messages,
        other => panic!("{other:?}"),
    };
    assert_eq!((pinned.len(), pinned[0].text.as_str(), pinned[0].pinned), (1, "Read the rules first", true));
    assert_eq!(world.discord.requests_to("GET", "/channels/4101/messages/pins").len(), 1);

    // What was shared: the cache, asking nothing; then Discord's search, because the user asked.
    let shared = |search: bool, before: Option<&str>| match world.ask(Request::Shared {
        conversation: world.key("4101"),
        kind: protocol::SharedKind::Media,
        before: before.map(str::to_owned),
        limit: 25,
        search,
    }) {
        Response::Shared { items, complete, total, .. } => (items, complete, total),
        other => panic!("{other:?}"),
    };
    let before = asked_now();
    assert!(!shared(false, None).1);
    assert_eq!(asked_now(), before, "the cache's list asks nothing");
    let (items, complete, total) = shared(true, None);
    assert!(complete && total == Some(37));
    assert_eq!(items.iter().map(|item| item.attachment.as_ref().unwrap().name.as_str()).collect::<Vec<_>>(), ["map.png"]);
    let search = world.discord.requests_to("GET", "/guilds/4000/messages/search");
    assert!(search[0].path.contains("channel_id=4101") && search[0].path.contains("has=image&has=video"), "{}", search[0].path);
    let (_, _, total) = shared(true, Some("1300000000000000600"));
    assert_eq!(total, None, "a later page has no total");
    assert!(world.discord.requests_to("GET", "/guilds/4000/messages/search")[1].path.contains("max_id=1300000000000000599"));
}

/// A file of `size` bytes beside the world's directory: chatd never sends from its own.
fn staged(world: &World, name: &str, size: usize) -> String {
    let dir = world.dir.with_extension("files");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, vec![b'x'; size]).unwrap();
    path.to_string_lossy().into_owned()
}

fn send_with(world: &World, conversation: &str, client_id: &str, text: &str, reply_to: Option<&str>, with: protocol::Outgoing) {
    let asked = Request::Send { conversation: world.key(conversation), client_id: client_id.into(), text: text.into(), reply_to: reply_to.map(str::to_owned), with };
    let answer = world.ask(asked);
    assert!(matches!(answer, Response::Sent { .. }), "{answer:?}");
}

fn file(path: &str, spoiler: bool) -> protocol::OutgoingFile {
    protocol::OutgoingFile { path: path.to_owned(), name: String::new(), spoiler, as_file: false, original: false }
}

#[test]
fn files_go_up_from_their_paths_with_the_senders_options_and_a_cancel_stops_them() {
    let world = polished();
    let notes = staged(&world, "notes.txt", 3);
    let photo = staged(&world, "photo.png", 70_000);

    // Two files, one a spoiler, silent, no link previews, a reply that does not ping.
    let with = protocol::Outgoing { files: vec![file(&notes, false), file(&photo, true)], silent: true, suppress_embeds: true, ping: false, ..Default::default() };
    send_with(&world, "4101", "77001", "see attached https://example.com", Some("1300000000000000090"), with);
    let sent = world.outcome("4101", "77001");
    assert_eq!(sent.state, SendState::Sent);
    let named = world.discord.requests_to("POST", "/channels/4101/attachments");
    assert_eq!(named.len(), 1);
    assert_eq!(named[0].body["files"], json!([
        {"id": "0", "filename": "notes.txt", "file_size": 3, "is_clip": false},
        {"id": "1", "filename": "SPOILER_photo.png", "file_size": 70_000, "is_clip": false},
    ]));
    // Each file went up whole, from its path, to the address Discord gave, without the session.
    let uploads = world.discord.requests_to("PUT", "/upload/");
    let shapes: Vec<(&str, usize, bool)> = uploads.iter().map(|seen| (seen.path.as_str(), seen.bytes, seen.headers.contains_key("authorization"))).collect();
    assert_eq!(shapes, [("/upload/up/notes.txt", 3, false), ("/upload/up/SPOILER_photo.png", 70_000, false)]);
    let posted = world.discord.requests_to("POST", "/channels/4101/messages");
    let body = &posted[0].body;
    assert_eq!(body["flags"], json!((1 << 12) | (1 << 2)));
    assert_eq!(body["allowed_mentions"]["replied_user"], false);
    assert_eq!(body["attachments"], json!([
        {"id": "0", "filename": "notes.txt", "uploaded_filename": "up/notes.txt"},
        {"id": "1", "filename": "SPOILER_photo.png", "uploaded_filename": "up/SPOILER_photo.png"},
    ]));
    assert_eq!(sent.attachments.len(), 2);
    assert!(sent.attachments[1].spoiler);

    // Too big for the free plan: refused with the file named, and nothing is asked.
    let big = world.dir.with_extension("files").join("big.mov");
    std::fs::File::create(&big).unwrap().set_len(11 << 20).unwrap();
    let before = world.discord.asked().len();
    let with = protocol::Outgoing { files: vec![file(&big.to_string_lossy(), false)], ..Default::default() };
    let asked = Request::Send { conversation: world.key("4101"), client_id: "77002".into(), text: String::new(), reply_to: None, with };
    match world.ask(asked) {
        Response::Error { detail, .. } => assert_eq!(detail, "big.mov is too big for Discord (limit 10 MB)."),
        other => panic!("{other:?}"),
    }
    assert_eq!(world.discord.asked().len(), before);

    // Cancelled while Discord is still handing out addresses: nothing goes up.
    *world.discord.stall.lock().unwrap() = Some(("/channels/4101/attachments".to_owned(), Duration::from_millis(1500)));
    let with = protocol::Outgoing { files: vec![file(&notes, false)], ..Default::default() };
    send_with(&world, "4101", "77003", "", None, with);
    sleep(Duration::from_millis(300));
    assert_eq!(world.ask(Request::CancelSend { conversation: world.key("4101"), client_id: "77003".into() }), Response::Ok);
    assert_eq!(world.failure("4101", "77003"), "Cancelled");
    assert_eq!(world.discord.requests_to("PUT", "/upload/").len(), 2, "the cancelled file never went up");
    *world.discord.stall.lock().unwrap() = None;

    // A GIF from the picker is its address; a sticker goes by id; the ping stays on by default.
    let catalog = |kind, query: &str, space: Option<&str>| match world.ask(Request::Catalog {
        account: world.account(),
        kind,
        space: space.map(|space| world.key(space)),
        query: query.into(),
        cursor: None,
    }) {
        Response::Catalog { items, .. } => items,
        other => panic!("{other:?}"),
    };
    let gifs = catalog(protocol::CatalogKind::Gifs, "cat", None);
    assert_eq!(gifs[0].id, "https://tenor.com/view/cat-typing-1");
    assert!(world.discord.requests_to("GET", "/gifs/search?q=cat&media_format=tinygif").len() == 1);
    let pick = protocol::Pick { kind: protocol::CatalogKind::Gifs, id: gifs[0].id.clone() };
    send_with(&world, "4101", "77004", "", Some("1300000000000000090"), protocol::Outgoing { pick: Some(pick), ..Default::default() });
    world.outcome("4101", "77004");
    let posted = world.discord.requests_to("POST", "/channels/4101/messages");
    assert_eq!(posted[1].body["content"], "https://tenor.com/view/cat-typing-1");
    assert_eq!(posted[1].body["allowed_mentions"]["replied_user"], true);
    // The server's own emoji and stickers, from READY: what lost its boost, an animated one
    // (Nitro's), and one only another role may use are not offered. Lottie stickers
    // have a native renderer; a DM still offers no server emoji without Nitro. Nothing is asked.
    let before = world.discord.asked().len();
    let emoji = catalog(protocol::CatalogKind::Emoji, "", Some("4000"));
    assert_eq!(emoji.iter().map(|item| (item.id.as_str(), item.name.as_str())).collect::<Vec<_>>(), [("emoji:5101", ":ferris:")]);
    assert!(catalog(protocol::CatalogKind::Emoji, "", None).is_empty());
    let stickers = catalog(protocol::CatalogKind::Stickers, "", Some("4000"));
    assert_eq!(stickers.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(), ["5201", "5202"]);
    assert_eq!(stickers[1].file.as_ref().unwrap().kind, protocol::MediaKind::File, "Lottie JSON is offered to the native renderer");
    assert_eq!(world.discord.asked().len(), before);
    let pick = protocol::Pick { kind: protocol::CatalogKind::Stickers, id: "5201".into() };
    send_with(&world, "4101", "77005", "", None, protocol::Outgoing { pick: Some(pick), ..Default::default() });
    world.outcome("4101", "77005");
    assert_eq!(world.discord.requests_to("POST", "/channels/4101/messages")[2].body["sticker_ids"], json!(["5201"]));
}

#[test]
fn a_waiting_profile_never_holds_up_a_send_a_superseded_one_is_dropped_and_reads_are_acked_once_per_three_seconds() {
    let world = World::start();
    settled(&world);
    let socket = world.dir.join("chat.sock");
    let ask_at = |request: Request| {
        let mut stream = ChatStream::connect(&socket).unwrap();
        write_frame(&mut stream, &request).unwrap();
        read_frame::<Response>(&mut stream).unwrap()
    };
    let profile = |person: &str| Request::Profile { person: world.key(person), space: None };

    // One profile now; the next two must wait out the 3 s. The later one supersedes the first.
    assert!(matches!(ask_at(profile("2006")), Response::Profile { .. }));
    std::thread::scope(|scope| {
        let stale = scope.spawn(|| ask_at(profile("2002")));
        sleep(Duration::from_millis(200));
        let newest = scope.spawn(|| ask_at(profile("2003")));
        sleep(Duration::from_millis(200));
        // A send while they wait goes at once.
        let sent = Instant::now();
        world.send("3001", "1730000000000000099", "not held up", None);
        world.outcome("3001", "1730000000000000099");
        assert!(sent.elapsed() < Duration::from_millis(1500), "the send waited {:?}", sent.elapsed());
        assert!(matches!(stale.join().unwrap(), Response::Error { .. }), "the superseded profile is not asked");
        assert!(matches!(newest.join().unwrap(), Response::Profile { .. }));
    });
    assert!(world.discord.requests_to("GET", "/users/2002/profile").is_empty());
    assert_eq!(world.discord.requests_to("GET", "/users/2003/profile").len(), 1);

    // Three reads in a row in one channel: the first is told at once, the newest when the 3 s
    // are up, and the one between never.
    let read: Vec<String> = (0..3).map(|n| world.incoming("3001", None, &format!("read {n}"), json!({}))).collect();
    wait("the messages to arrive", || world.messages("3001").iter().any(|message| message.id == read[2]).then_some(()));
    for id in &read {
        world.ask(Request::MarkRead { conversation: world.key("3001"), message_id: id.clone() });
    }
    let acked = |id: &str| world.discord.requests_to("POST", &format!("/channels/3001/messages/{id}/ack")).len();
    wait("the newest read to be told", || (acked(&read[2]) == 1).then_some(()));
    sleep(Duration::from_millis(500));
    assert_eq!((acked(&read[0]), acked(&read[1]), acked(&read[2])), (1, 0, 1));
}

#[test]
fn a_voice_message_and_a_gif_from_tenor_are_attachments_fetched_only_from_discord() {
    let world = World::start();
    let dm = "3002";
    let voice = format!("{}/media/attachments/1/9201/voice-message.ogg?ex=7fffffff&is=1&hm=abc", world.api);
    let still = format!("{}/media/external/tenor-still.png", world.api);
    let moving = format!("{}/media/external/tenor.mp4", world.api);
    let id = world.incoming(
        dm,
        None,
        "https://tenor.com/view/otter-123",
        json!({
            "flags": 8192,
            "attachments": [{"id": "9201", "filename": "voice-message.ogg", "size": 9120, "content_type": "audio/ogg",
                             "duration_secs": 4.2, "waveform": "AAoUHigyPEZQWg==", "url": voice, "proxy_url": voice}],
            "embeds": [{"type": "gifv", "url": "https://tenor.com/view/otter-123", "provider": {"name": "Tenor"},
                        "thumbnail": {"url": "https://media.tenor.com/still.png", "proxy_url": still, "width": 498, "height": 280},
                        "video": {"url": "https://media.tenor.com/otter.mp4", "proxy_url": moving, "width": 498, "height": 280}}]
        }),
    );
    let message = wait("the message", || world.messages(dm).pop().filter(|message| message.id == id));
    // The voice message: what the player and its waveform need, from Discord's own fields.
    let note = &message.attachments[0];
    assert_eq!((note.file.id.as_str(), note.file.kind, note.mime.as_str(), note.duration), ("att:9201", MediaKind::Audio, "audio/ogg", 4));
    assert_eq!(note.waveform, vec![0, 10, 20, 30, 40, 50, 60, 70, 80, 90]);
    // The GIF is an attachment, not a link card: a still in the row, the MP4 behind it.
    assert!(message.previews.is_empty(), "{:?}", message.previews);
    let gif = &message.attachments[1];
    assert_eq!((gif.name.as_str(), gif.file.kind, gif.mime.as_str()), ("GIF", MediaKind::Gif, "video/mp4"));
    assert_eq!((gif.file.width, gif.file.height), (498, 280));
    let thumb = gif.thumb.as_ref().unwrap();
    assert_eq!((thumb.id.clone(), thumb.kind), (format!("embed:{id}:0"), MediaKind::Image));

    // Nothing moving is fetched until asked, and then only from Discord's proxy.
    settled(&world);
    assert!(world.discord.requests_to("GET", "/media/external/tenor.mp4").is_empty());
    assert!(world.discord.requests_to("GET", "/media/attachments/1/9201/").is_empty());
    let played = world.fetched(&gif.file.id, None, false);
    assert!(played.path.is_some(), "{played:?}");
    assert_eq!(world.discord.requests_to("GET", "/media/external/tenor.mp4").len(), 1);
    let heard = world.fetched("att:9201", None, false);
    assert!(heard.path.is_some(), "{heard:?}");
    let fetched = world.discord.requests_to("GET", "/media/attachments/1/9201/");
    let paths: Vec<&str> = fetched.iter().map(|seen| seen.path.as_str()).collect();
    assert_eq!(paths, ["/media/attachments/1/9201/voice-message.ogg?ex=7fffffff&is=1&hm=abc"]);
}
