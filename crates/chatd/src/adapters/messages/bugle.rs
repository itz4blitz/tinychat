//! The Google Messages phone relay, written from the published field layout of the Bugle RPC
//! (pairing, long-poll, list, send). The phone stays the account: this computer is one more
//! paired device, and every message goes through the phone. Nothing here is the official client.

use aes::Aes256;
use base64::Engine;
use cipher::{KeyIvInit, StreamCipher};
use ctr::Ctr128BE;
use hmac::{Hmac, Mac};
use p256::ecdsa::SigningKey;
use p256::pkcs8::EncodePublicKey;
use sha2::Sha256;
use std::io::Read;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const API_KEY: &str = "AIzaSyCA4RsOZUFrm9whhtGosPlJLmVPnfSHKz8";
const USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";
const ORIGIN: &str = "https://messages.google.com";
const QR_BASE: &str = "https://support.google.com/messages/?p=web_computer#?c=";
const RELAY: &str = "https://instantmessaging-pa.googleapis.com/$rpc/google.internal.communications.instantmessaging.v1";
const UPLOAD: &str = "https://instantmessaging-pa.googleapis.com/upload";
/// A guard for this client, not a limit Google publishes.
const FILE_CAP: u64 = 100 * 1024 * 1024;
const NETWORK: &str = "Bugle";
/// How long one request waits for the phone to answer on the long-poll.
const PHONE: Duration = Duration::from_secs(20);

type AesCtr = Ctr128BE<Aes256>;
type HmacSha = Hmac<Sha256>;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()
}

fn random(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("the operating system random source");
    buf
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
}

/// AES-256-CTR then HMAC-SHA256 over `ciphertext || iv`. The wire is `ciphertext || iv || mac`.
fn seal(aes_key: &[u8], hmac_key: &[u8], plain: &[u8]) -> Vec<u8> {
    let iv = random(16);
    let mut out = plain.to_vec();
    AesCtr::new(aes_key.into(), iv.as_slice().into()).apply_keystream(&mut out);
    out.extend_from_slice(&iv);
    let mut mac = <HmacSha as Mac>::new_from_slice(hmac_key).expect("hmac key");
    mac.update(&out);
    out.extend_from_slice(&mac.finalize().into_bytes());
    out
}

fn open_seal(aes_key: &[u8], hmac_key: &[u8], packed: &[u8]) -> Option<Vec<u8>> {
    if packed.len() < 48 {
        return None;
    }
    let (body, mac) = packed.split_at(packed.len() - 32);
    let mut check = <HmacSha as Mac>::new_from_slice(hmac_key).ok()?;
    check.update(body);
    check.verify_slice(mac).ok()?;
    let (cipher, iv) = body.split_at(body.len() - 16);
    let mut plain = cipher.to_vec();
    AesCtr::new(aes_key.into(), iv.into()).apply_keystream(&mut plain);
    Some(plain)
}

struct Buf(Vec<u8>);

impl Buf {
    fn new() -> Buf {
        Buf(Vec::new())
    }

    fn varint(&mut self, mut n: u64) {
        loop {
            let mut byte = (n & 0x7f) as u8;
            n >>= 7;
            if n != 0 {
                byte |= 0x80;
            }
            self.0.push(byte);
            if n == 0 {
                break;
            }
        }
    }

    fn key(&mut self, field: u32, wire: u32) {
        self.varint(((field as u64) << 3) | wire as u64);
    }

    fn int(&mut self, field: u32, n: u64) {
        if n != 0 {
            self.key(field, 0);
            self.varint(n);
        }
    }

    fn bytes(&mut self, field: u32, data: &[u8]) {
        if data.is_empty() && field != 0 {
            return;
        }
        self.key(field, 2);
        self.varint(data.len() as u64);
        self.0.extend_from_slice(data);
    }

    fn text(&mut self, field: u32, text: &str) {
        if !text.is_empty() {
            self.bytes(field, text.as_bytes());
        }
    }

    fn msg(&mut self, field: u32, inner: &[u8]) {
        self.bytes(field, inner);
    }
}

fn config_version() -> Vec<u8> {
    let mut buf = Buf::new();
    buf.int(3, 2026);
    buf.int(4, 9);
    buf.int(5, 2);
    buf.int(7, 4);
    buf.int(9, 6);
    buf.0
}

fn auth_message(request: &str, token: &[u8]) -> Vec<u8> {
    let mut buf = Buf::new();
    buf.text(1, request);
    buf.text(3, NETWORK);
    buf.bytes(6, token);
    buf.msg(7, &config_version());
    buf.0
}

fn browser_details() -> Vec<u8> {
    let mut buf = Buf::new();
    buf.text(1, USER_AGENT);
    buf.int(2, 1); // OTHER
    buf.text(3, "tinyclient");
    buf.int(6, 2); // TABLET, so a browser session can stay signed in beside this one
    buf.0
}

fn register_body(public_der: &[u8]) -> Vec<u8> {
    let mut ecdsa = Buf::new();
    ecdsa.int(1, 2);
    ecdsa.bytes(2, public_der);
    let mut keys = Buf::new();
    keys.msg(6, &ecdsa.0);
    let mut body = Buf::new();
    body.msg(1, &auth_message(&uuid(), &[]));
    body.msg(3, &browser_details());
    body.msg(4, &keys.0);
    body.0
}

fn url_data(pairing: &[u8], aes_key: &[u8], hmac_key: &[u8]) -> String {
    let mut buf = Buf::new();
    buf.bytes(1, pairing);
    buf.bytes(2, aes_key);
    buf.bytes(3, hmac_key);
    format!("{QR_BASE}{}", b64(&buf.0))
}

fn uuid() -> String {
    let b = random(16);
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6] & 0x0f | 0x40, b[7], b[8] & 0x3f | 0x80, b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// One protobuf field, as far as this client reads them.
struct Field {
    number: u32,
    bytes: Vec<u8>,
    varint: Option<u64>,
}

fn read_varint(input: &mut &[u8]) -> Option<u64> {
    let mut n = 0u64;
    let mut shift = 0;
    for _ in 0..10 {
        let byte = *input.first()?;
        *input = &input[1..];
        n |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(n);
        }
        shift += 7;
    }
    None
}

fn fields(mut input: &[u8]) -> Vec<Field> {
    let mut out = Vec::new();
    while !input.is_empty() {
        let Some(key) = read_varint(&mut input) else {
            break;
        };
        let number = (key >> 3) as u32;
        let wire = (key & 7) as u32;
        match wire {
            0 => {
                let Some(n) = read_varint(&mut input) else {
                    break;
                };
                out.push(Field {
                    number,
                    bytes: Vec::new(),
                    varint: Some(n),
                });
            }
            2 => {
                let Some(len) = read_varint(&mut input) else {
                    break;
                };
                let len = len as usize;
                if input.len() < len {
                    break;
                }
                let (data, rest) = input.split_at(len);
                input = rest;
                out.push(Field {
                    number,
                    bytes: data.to_vec(),
                    varint: None,
                });
            }
            1 => {
                if input.len() < 8 {
                    break;
                }
                input = &input[8..];
            }
            5 => {
                if input.len() < 4 {
                    break;
                }
                input = &input[4..];
            }
            _ => break,
        }
    }
    out
}

fn field<'a>(list: &'a [Field], number: u32) -> Option<&'a Field> {
    list.iter().find(|field| field.number == number)
}

fn field_text(list: &[Field], number: u32) -> String {
    field(list, number)
        .map(|field| String::from_utf8_lossy(&field.bytes).into_owned())
        .unwrap_or_default()
}

fn field_bytes(list: &[Field], number: u32) -> Vec<u8> {
    field(list, number)
        .map(|field| field.bytes.clone())
        .unwrap_or_default()
}

fn field_int(list: &[Field], number: u32) -> u64 {
    field(list, number)
        .and_then(|field| field.varint)
        .unwrap_or(0)
}

fn field_texts(list: &[Field], number: u32) -> Vec<String> {
    list.iter()
        .filter(|field| field.number == number)
        .map(|field| String::from_utf8_lossy(&field.bytes).into_owned())
        .filter(|text| !text.is_empty())
        .collect()
}

#[derive(Clone, Default)]
struct Device {
    user: i64,
    source: String,
}

fn decode_device(bytes: &[u8]) -> Device {
    let list = fields(bytes);
    Device {
        user: field_int(&list, 1) as i64,
        source: field_text(&list, 2),
    }
}

fn decode_token(bytes: &[u8]) -> (Vec<u8>, i64) {
    let list = fields(bytes);
    (field_bytes(&list, 1), field_int(&list, 2) as i64)
}

struct RelayReply {
    browser: Device,
    pairing: Vec<u8>,
    token: Vec<u8>,
    ttl: i64,
}

fn decode_register(bytes: &[u8]) -> Option<RelayReply> {
    let list = fields(bytes);
    let browser = decode_device(&field_bytes(&list, 2));
    let pairing = field_bytes(&list, 3);
    let (token, ttl) = decode_token(&field_bytes(&list, 5));
    if pairing.is_empty() || token.is_empty() || browser.source.is_empty() {
        return None;
    }
    Some(RelayReply {
        browser,
        pairing,
        token,
        ttl,
    })
}

/// What the keychain holds after a phone has paired. `key` is the ECDSA signing key, hex PKCS#8,
/// kept so a later refresh can sign. Not `Debug`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Saved {
    pub account: String,
    #[serde(default)]
    pub name: String,
    pub aes: String,
    pub hmac: String,
    pub token: String,
    pub ttl: i64,
    pub mobile_user: i64,
    pub mobile: String,
    pub browser_user: i64,
    pub browser: String,
    pub key: String,
}

impl Saved {
    fn keys(&self) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        Some((unhex(&self.aes)?, unhex(&self.hmac)?, unb64(&self.token)?))
    }
}

fn json_array(value: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    value.as_array()
}

fn at<'a>(value: &'a serde_json::Value, field: usize) -> Option<&'a serde_json::Value> {
    let list = json_array(value)?;
    let item = list.get(field.saturating_sub(1))?;
    (!item.is_null()).then_some(item)
}

fn as_i64(value: &serde_json::Value) -> i64 {
    match value {
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0),
        serde_json::Value::String(text) => text.parse().unwrap_or(0),
        _ => 0,
    }
}

fn as_bytes(value: &serde_json::Value) -> Vec<u8> {
    value.as_str().and_then(unb64).unwrap_or_default()
}

/// PBLite: array index is field number minus one. Int64 is a string. Bytes are standard base64.
fn pblite_auth(request: &str, token: &[u8]) -> serde_json::Value {
    let config = serde_json::json!([null, null, 2026, 9, 2, null, 4, null, 6]);
    serde_json::json!([
        request,
        null,
        NETWORK,
        null,
        null,
        if token.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(b64(token))
        },
        config
    ])
}

fn pblite_device(device: &Device) -> serde_json::Value {
    serde_json::json!([device.user.to_string(), device.source, NETWORK])
}

fn outgoing(
    mobile: &Device,
    request: &str,
    session: &str,
    action: u32,
    encrypted: &[u8],
    token: &[u8],
    ttl: i64,
    message_type: u32,
) -> serde_json::Value {
    let mut data = Buf::new();
    data.text(1, request);
    data.int(2, action as u64);
    data.bytes(5, encrypted);
    data.text(6, session);
    let inner = serde_json::json!([
        request,
        19,
        null,
        null,
        null,
        null,
        null,
        null,
        null,
        null,
        null,
        b64(&data.0),
        null,
        null,
        null,
        null,
        null,
        null,
        null,
        null,
        null,
        null,
        [serde_json::json!([]), message_type]
    ]);
    serde_json::json!([
        pblite_device(mobile),
        inner,
        [
            request,
            null,
            null,
            null,
            null,
            b64(token),
            serde_json::json!([null, null, 2026, 9, 2, null, 4, null, 6])
        ],
        null,
        ttl.to_string()
    ])
}

fn list_request(count: u64) -> Vec<u8> {
    let mut buf = Buf::new();
    buf.int(2, count);
    buf.int(4, 1); // INBOX
    buf.0
}

fn messages_request(conversation: &str, count: u64) -> Vec<u8> {
    let mut buf = Buf::new();
    buf.text(2, conversation);
    buf.int(3, count);
    buf.0
}

fn send_request(conversation: &str, text: &str, tmp: &str, mine: &str) -> Vec<u8> {
    let mut content = Buf::new();
    content.text(1, text);
    let mut info = Buf::new();
    info.msg(2, &content.0);
    let mut payload_content = Buf::new();
    payload_content.msg(1, &content.0);
    let mut payload = Buf::new();
    payload.text(1, tmp);
    payload.msg(6, &payload_content.0);
    payload.text(7, conversation);
    payload.text(9, mine);
    payload.msg(10, &info.0);
    payload.text(12, tmp);
    let mut req = Buf::new();
    req.text(2, conversation);
    req.msg(3, &payload.0);
    req.text(5, tmp);
    req.0
}

fn refused(bytes: &[u8]) -> Result<(), String> {
    let list = fields(bytes);
    let status = field_int(&list, 3);
    if status > 1 {
        return Err("Messages didn't do that. Keep the phone online.".into());
    }
    if field(&list, 1).is_some_and(|field| field.varint == Some(0)) {
        return Err("Messages didn't do that.".into());
    }
    Ok(())
}

fn opened(key: &[u8], bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if key.len() == 32 && bytes.first() == Some(&0) {
        super::media::unpack(key, &bytes)
    } else {
        Ok(bytes)
    }
}

fn looks_b64(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes.len() % 4 == 0
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
}

fn mime_of(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        "pdf" => "application/pdf",
        "vcf" => "text/vcard",
        _ => "application/octet-stream",
    }
}

fn send_media_request(
    conversation: &str,
    caption: &str,
    tmp: &str,
    mine: &str,
    media_id: &str,
    key: &[u8],
    name: &str,
    mime: &str,
    size: u64,
) -> Vec<u8> {
    let mut media = Buf::new();
    media.text(2, media_id);
    media.text(4, name);
    media.int(5, size);
    media.bytes(11, key);
    media.text(14, mime);
    let mut media_info = Buf::new();
    media_info.msg(3, &media.0);
    let mut payload = Buf::new();
    payload.text(1, tmp);
    payload.text(7, conversation);
    payload.text(9, mine);
    payload.msg(10, &media_info.0);
    if !caption.is_empty() {
        let mut content = Buf::new();
        content.text(1, caption);
        let mut payload_content = Buf::new();
        payload_content.msg(1, &content.0);
        payload.msg(6, &payload_content.0);
        let mut caption_info = Buf::new();
        caption_info.msg(2, &content.0);
        payload.msg(10, &caption_info.0);
    }
    payload.text(12, tmp);
    let mut req = Buf::new();
    req.text(2, conversation);
    req.msg(3, &payload.0);
    req.text(5, tmp);
    req.0
}

fn reaction_request(message: &str, emoji: &str, action: u64) -> Vec<u8> {
    let mut data = Buf::new();
    data.text(1, emoji);
    data.int(2, reaction_type(emoji));
    let mut req = Buf::new();
    req.text(1, message);
    req.msg(2, &data.0);
    req.int(3, action);
    req.0
}

fn image_request(message: &str, action: &str) -> Vec<u8> {
    let mut req = Buf::new();
    req.text(1, message);
    req.text(2, action);
    req.0
}

fn read_request(conversation: &str, message: &str) -> Vec<u8> {
    let mut buf = Buf::new();
    buf.text(2, conversation);
    buf.text(3, message);
    buf.0
}

fn typing_request(conversation: &str) -> Vec<u8> {
    let mut data = Buf::new();
    data.text(1, conversation);
    data.int(3, 1);
    let mut buf = Buf::new();
    buf.msg(2, &data.0);
    buf.0
}

/// Unix seconds from a Bugle timestamp, which arrives in microseconds.
fn unix_seconds(raw: i64) -> i64 {
    if raw > 10_000_000_000_000_000 {
        raw / 1_000_000
    } else if raw > 10_000_000_000_000 {
        raw / 1_000
    } else {
        raw
    }
}

#[derive(Clone)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub group: bool,
    pub unread: bool,
    pub mine: String,
}

/// One photo or file on a message. Bytes are already opened when the phone sent them inline.
#[derive(Clone, Default)]
pub struct Piece {
    pub name: String,
    pub mime: String,
    pub media_id: String,
    pub key: Vec<u8>,
    /// The message part this file belongs to, for a later full-size fetch.
    pub action_id: String,
    pub inline: Vec<u8>,
    pub size: u64,
    pub width: u32,
    pub height: u32,
}

/// One emoji on a message, and who put it there.
#[derive(Clone, Default)]
pub struct Mark {
    pub emoji: String,
    pub who: Vec<String>,
}

#[derive(Clone, Default)]
pub struct Note {
    pub id: String,
    pub conversation: String,
    pub author: String,
    pub name: String,
    pub sent: i64,
    pub text: String,
    pub outgoing: bool,
    pub file: bool,
    pub pieces: Vec<Piece>,
    pub reactions: Vec<Mark>,
}

struct Live {
    saved: Saved,
    aes: Vec<u8>,
    hmac: Vec<u8>,
    token: Vec<u8>,
    ttl: i64,
    /// One id for the long-poll session. Responses are matched on it and on the request id.
    session: String,
    chats: Vec<Chat>,
    cookies: String,
}

struct Waiter {
    request: String,
    session: String,
    reply: Sender<Vec<u8>>,
}

/// Speaks to Google until the phone scans the code, then returns the session.
pub fn pair(
    show: &dyn Fn(String),
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Saved, String> {
    let signing = SigningKey::from_slice(&random(32))
        .map_err(|_| "could not make a pairing key".to_owned())?;
    let public = signing
        .verifying_key()
        .to_public_key_der()
        .map_err(|_| "could not encode the pairing key".to_owned())?;
    let aes_key = random(32);
    let hmac_key = random(32);
    let agent = agent(Duration::from_secs(30));
    let mut cookies = String::new();
    let body = register_body(public.as_bytes());
    let response = post(
        &agent,
        &format!("{RELAY}.Pairing/RegisterPhoneRelay"),
        "application/x-protobuf",
        &body,
        &cookies,
    )?;
    cookies = merge_cookies(&cookies, &response.cookies);
    let registered = decode_register(&response.body)
        .ok_or_else(|| "Messages did not start a pairing.".to_owned())?;
    let qr = url_data(&registered.pairing, &aes_key, &hmac_key);
    show(qr);
    let (events, inbox) = channel();
    let saved = Saved {
        account: String::new(),
        name: "Messages".into(),
        aes: hex(&aes_key),
        hmac: hex(&hmac_key),
        token: b64(&registered.token),
        ttl: registered.ttl,
        mobile_user: 0,
        mobile: String::new(),
        browser_user: registered.browser.user,
        browser: registered.browser.source.clone(),
        key: hex(signing.to_bytes().as_slice()),
    };
    let live = Arc::new(Mutex::new(Live {
        saved: saved.clone(),
        aes: aes_key,
        hmac: hmac_key,
        token: registered.token,
        ttl: registered.ttl,
        session: uuid(),
        chats: Vec::new(),
        cookies,
    }));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let poll_live = live.clone();
    let poll_stop = stop.clone();
    let (paired_tx, paired_rx) = channel::<Saved>();
    std::thread::Builder::new()
        .name("messages-pair".into())
        .spawn(move || {
            poll(
                poll_live,
                events,
                Arc::default(),
                poll_stop,
                false,
                Some(paired_tx),
            )
        })
        .map_err(|err| err.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            return Err("cancelled".into());
        }
        if Instant::now() > deadline {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            return Err("The code expired before the phone scanned it.".into());
        }
        match paired_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(saved) => {
                // The phone needs a moment to store the pair before the next connection.
                std::thread::sleep(Duration::from_secs(2));
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = inbox;
                return Ok(saved);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Messages closed the pairing connection.".into());
            }
        }
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    // The same system TLS as the rest of chatd. ureq's default is rustls, which this crate does
    // not enable, and asking for it panics before a pairing code exists.
    ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .user_agent(USER_AGENT)
        .build()
        .into()
}

struct Http {
    body: Vec<u8>,
    cookies: String,
}

fn post(
    agent: &ureq::Agent,
    url: &str,
    content: &str,
    body: &[u8],
    cookies: &str,
) -> Result<Http, String> {
    let mut request = agent
        .post(url)
        .header("Content-Type", content)
        .header("x-goog-api-key", API_KEY)
        .header("x-user-agent", "grpc-web-javascript/0.1")
        .header("Origin", ORIGIN)
        .header("Referer", &format!("{ORIGIN}/"))
        .header(
            "sec-ch-ua",
            "\"Google Chrome\";v=\"152\", \"Chromium\";v=\"152\", \"Not-A.Brand\";v=\"24\"",
        );
    if !cookies.is_empty() {
        request = request.header("Cookie", cookies);
    }
    let mut response = request
        .send(body)
        .map_err(|err| format!("Messages: {err}"))?;
    let status = response.status().as_u16();
    let mut cookies_out = String::new();
    for (name, value) in response.headers() {
        if name.as_str().eq_ignore_ascii_case("set-cookie") {
            if let Some(pair) = value.to_str().ok().and_then(|line| line.split(';').next()) {
                if !cookies_out.is_empty() {
                    cookies_out.push_str("; ");
                }
                cookies_out.push_str(pair.trim());
            }
        }
    }
    let mut body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .read_to_end(&mut body)
        .map_err(|err| err.to_string())?;
    if !(200..300).contains(&status) {
        return Err(format!("Messages refused the request ({status})."));
    }
    Ok(Http {
        body,
        cookies: cookies_out,
    })
}

fn merge_cookies(old: &str, new: &str) -> String {
    if new.is_empty() {
        return old.to_owned();
    }
    if old.is_empty() {
        return new.to_owned();
    }
    format!("{old}; {new}")
}

/// A running session: the long-poll thread and the commands share `live`.
pub struct Session {
    live: Arc<Mutex<Live>>,
    events: Sender<crate::adapters::messages::Event>,
    heard: Mutex<Option<Receiver<crate::adapters::messages::Event>>>,
    waiters: Arc<Mutex<Vec<Waiter>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Session {
    pub fn open(saved: Saved) -> Result<Session, String> {
        let (aes, hmac, token) = saved
            .keys()
            .ok_or_else(|| "the Messages session is unreadable".to_owned())?;
        let (events, heard) = channel();
        let live = Arc::new(Mutex::new(Live {
            ttl: saved.ttl,
            saved,
            aes,
            hmac,
            token,
            session: uuid(),
            chats: Vec::new(),
            cookies: String::new(),
        }));
        let session = Session {
            live,
            events,
            heard: Mutex::new(Some(heard)),
            waiters: Arc::default(),
            stop: Arc::default(),
        };
        let (live, events, waiters, stop) = (
            session.live.clone(),
            session.events.clone(),
            session.waiters.clone(),
            session.stop.clone(),
        );
        std::thread::Builder::new()
            .name("messages-events".into())
            .spawn(move || {
                let mut first = true;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    poll(
                        live.clone(),
                        events.clone(),
                        waiters.clone(),
                        stop.clone(),
                        first,
                        None,
                    );
                    first = false;
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
            })
            .map_err(|err| err.to_string())?;
        Ok(session)
    }

    pub fn events(&self) -> Option<Receiver<crate::adapters::messages::Event>> {
        self.heard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    pub fn me(&self) -> (String, String) {
        let live = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let name = if live.saved.name.is_empty() {
            "Messages".into()
        } else {
            live.saved.name.clone()
        };
        (live.saved.account.clone(), name)
    }

    pub fn close(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn rpc(&self, action: u32, plain: &[u8], message_type: u32) -> Result<Vec<u8>, String> {
        self.rpc_as(&uuid(), action, plain, message_type)
    }

    /// SetActiveSession. The request id is the long-poll's session id; without it the phone
    /// accepts the pair and then sends nothing.
    fn activate(&self) -> Result<(), String> {
        let session = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .session
            .clone();
        let _ = self.rpc_as(&session, 16, &[], 2)?;
        Ok(())
    }

    fn rpc_as(
        &self,
        request: &str,
        action: u32,
        plain: &[u8],
        message_type: u32,
    ) -> Result<Vec<u8>, String> {
        let request = request.to_owned();
        let (session, body, cookies) = {
            let live = self
                .live
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let encrypted = if plain.is_empty() {
                Vec::new()
            } else {
                seal(&live.aes, &live.hmac, plain)
            };
            let mobile = Device {
                user: live.saved.mobile_user,
                source: live.saved.mobile.clone(),
            };
            let body = outgoing(
                &mobile,
                &request,
                &live.session,
                action,
                &encrypted,
                &live.token,
                live.ttl,
                message_type,
            );
            (
                live.session.clone(),
                serde_json::to_vec(&body).unwrap_or_default(),
                live.cookies.clone(),
            )
        };
        let (tx, rx) = channel();
        self.waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Waiter {
                request: request.clone(),
                session: session.clone(),
                reply: tx,
            });
        let agent = agent(Duration::from_secs(30));
        let sent = post(
            &agent,
            &format!("{RELAY}.Messaging/SendMessage"),
            "application/json+protobuf",
            &body,
            &cookies,
        );
        if let Err(err) = sent {
            self.waiters
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|waiter| waiter.request != request);
            return Err(err);
        }
        match rx.recv_timeout(PHONE) {
            Ok(bytes) => Ok(bytes),
            Err(_) => {
                self.waiters
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .retain(|waiter| waiter.request != request);
                Err("Your phone isn't responding. Keep Messages open, and the phone online.".into())
            }
        }
    }

    pub fn refresh(&self) -> Result<(), String> {
        self.activate()?;
        let first = self.rpc(1, &list_request(50), 16)?;
        self.apply_chats(&first);
        Ok(())
    }

    pub fn history(&self, conversation: &str) -> Result<Vec<Note>, String> {
        let bytes = self.rpc(2, &messages_request(conversation, 40), 2)?;
        Ok(decode_messages(&bytes))
    }

    fn mine_of(&self, conversation: &str) -> String {
        let live = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        live.chats
            .iter()
            .find(|chat| chat.id == conversation)
            .map(|chat| chat.mine.clone())
            .unwrap_or_default()
    }

    pub fn send(&self, conversation: &str, text: &str) -> Result<String, String> {
        let mine = self.mine_of(conversation);
        let tmp = uuid();
        let bytes = self.rpc(3, &send_request(conversation, text, &tmp, &mine), 2)?;
        refused(&bytes)?;
        Ok(tmp)
    }

    /// Encrypts the file, uploads it, and asks the phone to send it. A large photo may leave as
    /// an MMS copy; this does not promise an RCS high-quality send.
    pub fn send_file(
        &self,
        conversation: &str,
        caption: &str,
        path: &str,
        name: &str,
    ) -> Result<String, String> {
        let named = if name.is_empty() { "file" } else { name };
        let meta =
            std::fs::metadata(path).map_err(|_| format!("Messages couldn't read {named}."))?;
        if meta.len() > FILE_CAP {
            return Err(
                "That file is larger than 100 MB, which tinyclient won't send through Messages."
                    .into(),
            );
        }
        let plain = std::fs::read(path).map_err(|_| format!("Messages couldn't read {named}."))?;
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|_| "Messages could not make a media key.".to_owned())?;
        let packed = super::media::pack(&key, &plain)?;
        let mime = mime_of(named);
        let media_id = self.upload(&packed, &mime)?;
        let mine = self.mine_of(conversation);
        let tmp = uuid();
        let bytes = self.rpc(
            3,
            &send_media_request(
                conversation,
                caption,
                &tmp,
                &mine,
                &media_id,
                &key,
                named,
                &mime,
                plain.len() as u64,
            ),
            2,
        )?;
        refused(&bytes)?;
        Ok(tmp)
    }

    pub fn react(&self, message: &str, emoji: &str, on: bool) -> Result<(), String> {
        let action = if on { 1 } else { 2 };
        let bytes = self.rpc(38, &reaction_request(message, emoji, action), 2)?;
        if on && refused(&bytes).is_err() {
            let switched = self.rpc(38, &reaction_request(message, emoji, 3), 2)?;
            refused(&switched)?;
            return Ok(());
        }
        refused(&bytes)
    }

    /// The bytes of one attachment, opened. The content store is asked first; the phone is asked
    /// for the full image when that has nothing.
    pub fn fetch(
        &self,
        media_id: &str,
        key: &[u8],
        message: &str,
        action: &str,
    ) -> Result<Vec<u8>, String> {
        if !media_id.is_empty() {
            if let Ok(bytes) = self.download(media_id) {
                return opened(key, bytes);
            }
        }
        if !message.is_empty() {
            let asked = self
                .rpc(36, &image_request(message, action), 2)
                .unwrap_or_default();
            if let Some(inline) = decode_messages(&asked)
                .into_iter()
                .flat_map(|note| note.pieces)
                .find(|piece| !piece.inline.is_empty())
            {
                return Ok(inline.inline);
            }
        }
        Err("Messages couldn't fetch that file. Keep the phone online.".into())
    }

    fn upload(&self, packed: &[u8], mime: &str) -> Result<String, String> {
        let (payload, cookies) = {
            let live = self
                .live
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut device = Buf::new();
            device.int(1, live.saved.mobile_user as u64);
            device.text(2, &live.saved.mobile);
            device.text(3, NETWORK);
            let mut req = Buf::new();
            req.int(1, 1);
            req.msg(2, &auth_message(&uuid(), &live.token));
            req.msg(3, &device.0);
            (b64(&req.0), live.cookies.clone())
        };
        let http = agent(Duration::from_secs(60));
        let mut start = http
            .post(UPLOAD)
            .header("Content-Type", "text/plain")
            .header("x-goog-api-key", API_KEY)
            .header("Origin", ORIGIN)
            .header("X-Goog-Upload-Protocol", "resumable")
            .header("X-Goog-Upload-Command", "start")
            .header(
                "X-Goog-Upload-Header-Content-Length",
                &packed.len().to_string(),
            )
            .header("X-Goog-Upload-Header-Content-Type", mime);
        if !cookies.is_empty() {
            start = start.header("Cookie", &cookies);
        }
        let response = start
            .send(payload.as_bytes())
            .map_err(|err| format!("Messages couldn't upload that file ({err})."))?;
        if !(200..300).contains(&response.status().as_u16()) {
            return Err("Messages couldn't start the upload. Keep the phone online.".into());
        }
        let url = response
            .headers()
            .get("x-goog-upload-url")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned();
        if url.is_empty() {
            return Err("Messages didn't give an upload address.".into());
        }
        let mut finish = http
            .post(&url)
            .header("Content-Type", mime)
            .header("X-Goog-Upload-Command", "upload, finalize")
            .header("X-Goog-Upload-Offset", "0");
        if !cookies.is_empty() {
            finish = finish.header("Cookie", &cookies);
        }
        let mut response = finish
            .send(packed)
            .map_err(|err| format!("Messages couldn't upload that file ({err})."))?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .read_to_end(&mut body)
            .map_err(|err| err.to_string())?;
        if !(200..300).contains(&status) {
            return Err("Messages refused the upload.".into());
        }
        let raw = if looks_b64(&body) {
            unb64(&String::from_utf8_lossy(&body)).unwrap_or(body)
        } else {
            body
        };
        let list = fields(&raw);
        let media = field_text(&fields(&field_bytes(&list, 1)), 1);
        if media.is_empty() {
            return Err("Messages uploaded the file and sent back no id.".into());
        }
        Ok(media)
    }

    fn download(&self, media_id: &str) -> Result<Vec<u8>, String> {
        let (payload, cookies) = {
            let live = self
                .live
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut info = Buf::new();
            info.text(1, media_id);
            info.int(2, 1);
            let mut req = Buf::new();
            req.msg(1, &info.0);
            req.msg(2, &auth_message(&uuid(), &live.token));
            (b64(&req.0), live.cookies.clone())
        };
        let http = agent(Duration::from_secs(60));
        let mut request = http
            .get(UPLOAD)
            .header("x-goog-api-key", API_KEY)
            .header("Origin", ORIGIN)
            .header("X-Goog-Download-Metadata", &payload);
        if !cookies.is_empty() {
            request = request.header("Cookie", &cookies);
        }
        let mut response = request
            .call()
            .map_err(|err| format!("Messages couldn't fetch that file ({err})."))?;
        if !(200..300).contains(&response.status().as_u16()) {
            return Err("Messages couldn't fetch that file.".into());
        }
        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .read_to_end(&mut body)
            .map_err(|err| err.to_string())?;
        if body.is_empty() {
            return Err("Messages returned an empty file.".into());
        }
        Ok(body)
    }

    pub fn mark_read(&self, conversation: &str, message: &str) -> Result<(), String> {
        let _ = self.rpc(10, &read_request(conversation, message), 2)?;
        Ok(())
    }

    pub fn typing(&self, conversation: &str) -> Result<(), String> {
        let _ = self.rpc(12, &typing_request(conversation), 2)?;
        Ok(())
    }

    fn apply_chats(&self, bytes: &[u8]) {
        let chats = decode_chats(bytes);
        if chats.is_empty() {
            return;
        }
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .chats = chats.clone();
        let _ = self
            .events
            .send(crate::adapters::messages::Event::Chats(chats));
    }
}

fn poll(
    live: Arc<Mutex<Live>>,
    events: Sender<crate::adapters::messages::Event>,
    waiters: Arc<Mutex<Vec<Waiter>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    announce: bool,
    paired: Option<Sender<Saved>>,
) {
    let agent = agent(Duration::from_secs(90));
    let (request, token, cookies) = {
        let held = live.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (uuid(), held.token.clone(), held.cookies.clone())
    };
    // field 4 is an empty wrapper the web client sends: [null, []].
    let payload = serde_json::json!([
        pblite_auth(&request, &token),
        null,
        null,
        [null, serde_json::json!([])]
    ]);
    let body = serde_json::to_vec(&payload).unwrap_or_default();
    let mut request = agent
        .post(&format!("{RELAY}.Messaging/ReceiveMessages"))
        .header("Content-Type", "application/json+protobuf")
        .header("x-goog-api-key", API_KEY)
        .header("x-user-agent", "grpc-web-javascript/0.1")
        .header("Origin", ORIGIN)
        .header("Referer", &format!("{ORIGIN}/"));
    if !cookies.is_empty() {
        request = request.header("Cookie", &cookies);
    }
    let Ok(mut response) = request.send(&body) else {
        let _ = events.send(crate::adapters::messages::Event::Closed(
            "Couldn't reach Messages.".into(),
        ));
        return;
    };
    if !(200..300).contains(&response.status().as_u16()) {
        let _ = events.send(crate::adapters::messages::Event::Closed(
            "Messages refused the connection.".into(),
        ));
        return;
    }
    // The long-poll is open. Commands (SetActiveSession, then the inbox) go out on another
    // request while this one stays up to carry the phone's answer.
    if announce {
        let _ = events.send(crate::adapters::messages::Event::Ready);
    }
    let mut reader = response.body_mut().as_reader();
    let mut pending = Vec::new();
    let mut buf = [0u8; 8192];
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => pending.extend_from_slice(&buf[..n]),
        }
        consume(&pending, &live, &events, &waiters, &paired);
        // Drop bytes already parsed as complete JSON values. `consume` returns how many.
        let used = take_complete(&pending);
        pending.drain(..used);
    }
}

fn take_complete(bytes: &[u8]) -> usize {
    let text = String::from_utf8_lossy(bytes);
    let mut depth = 0i32;
    let mut start = None;
    let mut end = 0;
    let mut in_string = false;
    let mut escape = false;
    for (at, ch) in text.char_indices() {
        if in_string {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '[' => {
                if depth == 0 {
                    start = Some(at);
                }
                depth += 1;
            }
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = at + 1;
                }
            }
            _ => {}
        }
    }
    if depth == 0 {
        end
    } else {
        start.unwrap_or(0)
    }
}

fn consume(
    bytes: &[u8],
    live: &Mutex<Live>,
    events: &Sender<crate::adapters::messages::Event>,
    waiters: &Mutex<Vec<Waiter>>,
    paired: &Option<Sender<Saved>>,
) {
    let text = String::from_utf8_lossy(bytes);
    let mut depth = 0i32;
    let mut start = None;
    let mut in_string = false;
    let mut escape = false;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for (index, (at, ch)) in chars.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if *ch == '\\' {
                escape = true;
            } else if *ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '[' => {
                if depth == 0 {
                    start = Some(*at);
                }
                depth += 1;
            }
            ']' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(from) = start.take() {
                        let slice = &text[from..*at + 1];
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(slice) {
                            on_payload(&value, live, events, waiters, paired);
                        }
                    }
                }
            }
            _ => {}
        }
        let _ = index;
    }
}

fn on_payload(
    value: &serde_json::Value,
    live: &Mutex<Live>,
    events: &Sender<crate::adapters::messages::Event>,
    waiters: &Mutex<Vec<Waiter>>,
    paired: &Option<Sender<Saved>>,
) {
    // Long-poll chunks are one payload, or a stream wrapped in an extra array.
    if let Some(list) = value.as_array() {
        if list.first().and_then(|item| item.as_array()).is_some() && at(value, 2).is_none() {
            for item in list {
                on_payload(item, live, events, waiters, paired);
            }
            return;
        }
    }
    let Some(data) = at(value, 2) else {
        return;
    };
    let route = at(data, 2).map(as_i64).unwrap_or(0);
    let message = at(data, 12).map(as_bytes).unwrap_or_default();
    if route == 14 {
        on_pair(&message, live, paired);
        return;
    }
    if route != 19 || message.is_empty() {
        return;
    }
    let rpc = fields(&message);
    let echoed = field_text(&rpc, 1);
    let action = field_int(&rpc, 4);
    let encrypted = field_bytes(&rpc, 8);
    let plain = if encrypted.is_empty() {
        field_bytes(&rpc, 5)
    } else {
        let held = live.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        open_seal(&held.aes, &held.hmac, &encrypted).unwrap_or_default()
    };
    if !echoed.is_empty() && !plain.is_empty() {
        let mut waiting = waiters
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(at) = waiting
            .iter()
            .position(|waiter| waiter.request == echoed || waiter.session == echoed)
        {
            let waiter = waiting.remove(at);
            drop(waiting);
            let _ = waiter.reply.send(plain.clone());
        }
    }
    if plain.is_empty() {
        return;
    }
    match action {
        1 => {
            let chats = decode_chats(&plain);
            if !chats.is_empty() {
                live.lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .chats = chats.clone();
                let _ = events.send(crate::adapters::messages::Event::Chats(chats));
            }
        }
        16 => on_updates(&plain, live, events),
        _ => {}
    }
}

fn on_pair(message: &[u8], live: &Mutex<Live>, paired: &Option<Sender<Saved>>) {
    let list = fields(message);
    let paired_bytes = field_bytes(&list, 4);
    if paired_bytes.is_empty() {
        if field(&list, 5).is_some() {
            let _ = paired;
        }
        return;
    }
    let inner = fields(&paired_bytes);
    let mobile = decode_device(&field_bytes(&inner, 1));
    let (token, ttl) = decode_token(&field_bytes(&inner, 2));
    let browser = decode_device(&field_bytes(&inner, 3));
    if mobile.source.is_empty() {
        return;
    }
    let mut held = live.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    held.saved.account = mobile.source.clone();
    held.saved.mobile = mobile.source;
    held.saved.mobile_user = mobile.user;
    if !browser.source.is_empty() {
        held.saved.browser = browser.source;
        held.saved.browser_user = browser.user;
    }
    if !token.is_empty() {
        held.token = token.clone();
        held.saved.token = b64(&token);
        held.ttl = ttl;
        held.saved.ttl = ttl;
    }
    if let Some(paired) = paired {
        let _ = paired.send(held.saved.clone());
    }
}

fn on_updates(bytes: &[u8], live: &Mutex<Live>, events: &Sender<crate::adapters::messages::Event>) {
    let list = fields(bytes);
    if let Some(conversations) = field(&list, 2) {
        let chats = decode_chats(&conversations.bytes);
        if !chats.is_empty() {
            live.lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .chats = chats.clone();
            let _ = events.send(crate::adapters::messages::Event::Chats(chats));
        }
    }
    if let Some(messages) = field(&list, 3) {
        for note in decode_messages(&messages.bytes) {
            let _ = events.send(crate::adapters::messages::Event::Message(note));
        }
    }
    if let Some(typing) = field(&list, 4) {
        let data = fields(&field_bytes(&fields(&typing.bytes), 2));
        let conversation = field_text(&data, 1);
        let user = fields(&field_bytes(&data, 2));
        let who = field_text(&user, 2);
        let started = field_int(&data, 3) == 1;
        if !conversation.is_empty() {
            let _ = events.send(crate::adapters::messages::Event::Typing {
                conversation,
                by: who.clone(),
                name: who,
                started,
            });
        }
    }
}

fn decode_chats(bytes: &[u8]) -> Vec<Chat> {
    // ListConversationsResponse field 2 is repeated Conversation. UpdateEvents wraps the same.
    let list = fields(bytes);
    let mut chats = Vec::new();
    for item in list.iter().filter(|field| field.number == 2) {
        let conv = fields(&item.bytes);
        // A conversation's own field 2 is the name, a string, so a nested conversation is the
        // one whose field 1 is a non-empty id and field 2 is text rather than another message.
        let id = field_text(&conv, 1);
        if id.is_empty() {
            chats.extend(decode_chats(&item.bytes));
            continue;
        }
        let mut title = field_text(&conv, 2);
        let group = field_int(&conv, 10) != 0;
        let unread = field_int(&conv, 6) != 0;
        let mut mine = field_text(&conv, 11);
        for person in conv.iter().filter(|field| field.number == 20) {
            let participant = fields(&person.bytes);
            let info = fields(&field_bytes(&participant, 1));
            let pid = field_text(&info, 3);
            let name = {
                let full = field_text(&participant, 3);
                if full.is_empty() {
                    field_text(&participant, 2)
                } else {
                    full
                }
            };
            let is_me = field_int(&participant, 6) != 0;
            if is_me && !pid.is_empty() {
                mine = pid;
            } else if title.is_empty() && !name.is_empty() && !group {
                title = name;
            } else if title.is_empty() && !group {
                let number = field_text(&participant, 15);
                if !number.is_empty() {
                    title = number;
                }
            }
        }
        if title.is_empty() {
            title = if group {
                "Group".into()
            } else {
                "Messages".into()
            };
        }
        chats.push(Chat {
            id,
            title,
            group,
            unread,
            mine,
        });
    }
    chats
}

fn emoji_of(unicode: &str, kind: u64) -> String {
    if !unicode.is_empty() {
        return unicode.to_owned();
    }
    match kind {
        1 => "👍",
        2 | 12 => "❤️",
        3 => "😂",
        4 => "😮",
        5 => "😢",
        6 | 11 => "😡",
        7 => "👎",
        9 => "🤔",
        10 => "😭",
        _ => "",
    }
    .to_owned()
}

fn reaction_type(emoji: &str) -> u64 {
    match emoji {
        "👍" => 1,
        "❤️" | "❤" => 2,
        "😂" => 3,
        "😮" => 4,
        "😢" => 5,
        "😡" => 6,
        "👎" => 7,
        "🤔" => 9,
        "😭" => 10,
        _ => 8,
    }
}

fn decode_piece(bytes: &[u8], action_id: &str) -> Option<Piece> {
    let media = fields(bytes);
    let mime = field_text(&media, 14);
    let mut name = field_text(&media, 4);
    if name.is_empty() {
        name = label_of(&mime).to_owned();
    }
    let size = field_int(&media, 5);
    let dims = fields(&field_bytes(&media, 6));
    let mut inline = field_bytes(&media, 7);
    let key = field_bytes(&media, 11);
    if !inline.is_empty() && key.len() == 32 && inline.first() == Some(&0) {
        if let Ok(opened) = super::media::unpack(&key, &inline) {
            inline = opened;
        }
    }
    let media_id = field_text(&media, 2);
    if name.is_empty() && media_id.is_empty() && inline.is_empty() {
        return None;
    }
    Some(Piece {
        name,
        mime,
        media_id,
        key,
        action_id: action_id.to_owned(),
        size: if size == 0 { inline.len() as u64 } else { size },
        width: field_int(&dims, 1) as u32,
        height: field_int(&dims, 2) as u32,
        inline,
    })
}

fn label_of(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "Photo"
    } else if mime.starts_with("video/") {
        "Video"
    } else if mime.starts_with("audio/") {
        "Audio"
    } else if mime.contains("vcard") {
        "Contact"
    } else {
        "File"
    }
}

fn decode_reactions(msg: &[Field]) -> Vec<Mark> {
    let mut marks: Vec<Mark> = Vec::new();
    for entry in msg.iter().filter(|field| field.number == 19) {
        let parts = fields(&entry.bytes);
        let data = fields(&field_bytes(&parts, 1));
        let emoji = emoji_of(&field_text(&data, 1), field_int(&data, 2));
        if emoji.is_empty() {
            continue;
        }
        let who = field_texts(&parts, 2);
        if let Some(have) = marks.iter_mut().find(|mark| mark.emoji == emoji) {
            for id in who {
                if !have.who.contains(&id) {
                    have.who.push(id);
                }
            }
        } else {
            marks.push(Mark { emoji, who });
        }
    }
    marks
}

fn decode_messages(bytes: &[u8]) -> Vec<Note> {
    let list = fields(bytes);
    let mut notes = Vec::new();
    for item in list
        .iter()
        .filter(|field| field.number == 2 || field.number == 1)
    {
        let msg = fields(&item.bytes);
        let id = field_text(&msg, 1);
        let conversation = field_text(&msg, 7);
        if id.is_empty() || conversation.is_empty() {
            if field(&msg, 2).is_some() {
                notes.extend(decode_messages(&item.bytes));
            }
            continue;
        }
        let status = field_int(&fields(&field_bytes(&msg, 4)), 2);
        // 200–299 is a tombstone, 300 a delete. Neither is a message to show.
        if (200..300).contains(&status) || status == 300 {
            continue;
        }
        let sent = unix_seconds(field_int(&msg, 5) as i64);
        let mut author = field_text(&msg, 9);
        let mut name = String::new();
        let mut outgoing = false;
        if let Some(sender) = field(&msg, 25) {
            let participant = fields(&sender.bytes);
            let info = fields(&field_bytes(&participant, 1));
            if author.is_empty() {
                author = field_text(&info, 3);
            }
            name = field_text(&participant, 3);
            if name.is_empty() {
                name = field_text(&participant, 2);
            }
            if name.is_empty() {
                name = field_text(&participant, 15);
            }
            outgoing = field_int(&participant, 6) != 0;
        }
        let mut text = String::new();
        let mut file = false;
        let mut pieces = Vec::new();
        for info in msg.iter().filter(|field| field.number == 10) {
            let parts = fields(&info.bytes);
            let action_id = field_text(&parts, 1);
            if let Some(content) = field(&parts, 2) {
                let words = field_text(&fields(&content.bytes), 1);
                if !words.is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&words);
                }
            }
            if let Some(media) = field(&parts, 3) {
                file = true;
                if let Some(piece) = decode_piece(&media.bytes, &action_id) {
                    pieces.push(piece);
                }
            }
            if field(&parts, 5).is_some() && pieces.is_empty() {
                file = true;
                pieces.push(Piece {
                    name: "Location".into(),
                    mime: "text/plain".into(),
                    ..Piece::default()
                });
            }
        }
        let reactions = decode_reactions(&msg);
        if text.is_empty()
            && file
            && pieces
                .iter()
                .all(|piece| piece.inline.is_empty() && piece.media_id.is_empty())
        {
            text = pieces
                .iter()
                .map(|piece| piece.name.as_str())
                .filter(|name| !name.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
            if text.is_empty() {
                text = "File".into();
            }
        }
        notes.push(Note {
            id,
            conversation,
            author,
            name,
            sent,
            text,
            outgoing,
            file,
            pieces,
            reactions,
        });
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_a_caption_and_a_reaction_decode_and_a_tombstone_does_not() {
        let mut media = Buf::new();
        media.text(2, "media-1");
        media.text(4, "IMG_1.jpg");
        media.text(14, "image/jpeg");
        media.int(5, 12);
        media.bytes(11, &[9; 32]);
        let mut info = Buf::new();
        info.text(1, "part-1");
        info.msg(3, &media.0);
        let mut words = Buf::new();
        words.text(1, "look");
        let mut caption = Buf::new();
        caption.msg(2, &words.0);
        let mut data = Buf::new();
        data.int(2, 1);
        let mut entry = Buf::new();
        entry.msg(1, &data.0);
        entry.text(2, "phone-grace");
        let mut message = Buf::new();
        message.text(1, "m1");
        message.int(5, 1_791_892_800_000_000);
        message.text(7, "c1");
        message.text(9, "phone-grace");
        message.msg(10, &info.0);
        message.msg(10, &caption.0);
        message.msg(19, &entry.0);
        let mut gone = Buf::new();
        gone.text(1, "m-gone");
        gone.text(7, "c1");
        let mut status = Buf::new();
        status.int(2, 200);
        gone.msg(4, &status.0);
        let mut wrap = Buf::new();
        wrap.msg(1, &message.0);
        wrap.msg(1, &gone.0);
        let notes = decode_messages(&wrap.0);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].text, "look");
        assert_eq!(notes[0].pieces[0].name, "IMG_1.jpg");
        assert_eq!(notes[0].pieces[0].media_id, "media-1");
        assert_eq!(notes[0].pieces[0].action_id, "part-1");
        assert_eq!(notes[0].reactions[0].emoji, "👍");
        assert_eq!(notes[0].reactions[0].who, ["phone-grace"]);
    }
}
