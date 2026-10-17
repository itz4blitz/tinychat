//! Discord's objects as the gateway and the HTTP API send them, naming only the fields used.
//! Everything else in a payload is skipped while parsing.

use crate::store::PersonRow;
use protocol::{MediaFile, MediaKind, Presence};
use serde::de::{self, Deserializer, IgnoredAny};
use serde::{Deserialize, Serialize};

const CDN: &str = "https://cdn.discordapp.com";
const MEDIA: &str = "https://media.discordapp.net";

/// Where avatars, icons and emoji are served from. `TINYCLIENT_DISCORD_CDN` replaces it, and the
/// media proxy with it, so a test's pictures come from its own fake and never from Discord.
pub fn cdn() -> &'static str {
    static CDN_AT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CDN_AT.get_or_init(|| super::super::net::address("TINYCLIENT_DISCORD_CDN", CDN))
}

/// The media proxy, which serves stickers and resized pictures.
pub fn media() -> &'static str {
    static MEDIA_AT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MEDIA_AT.get_or_init(|| super::super::net::address("TINYCLIENT_DISCORD_CDN", MEDIA))
}

/// Channel types.
pub const TEXT: u8 = 0;
pub const DM: u8 = 1;
pub const VOICE: u8 = 2;
pub const GROUP_DM: u8 = 3;
pub const CATEGORY: u8 = 4;
pub const NEWS: u8 = 5;
pub const NEWS_THREAD: u8 = 10;
pub const PUBLIC_THREAD: u8 = 11;
pub const PRIVATE_THREAD: u8 = 12;
pub const STAGE: u8 = 13;
pub const FORUM: u8 = 15;
pub const MEDIA_FORUM: u8 = 16;

pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const SEND_MESSAGES: u64 = 1 << 11;
pub const ADMINISTRATOR: u64 = 1 << 3;
pub const ADD_REACTIONS: u64 = 1 << 6;
pub const READ_MESSAGE_HISTORY: u64 = 1 << 16;

/// Where Discord's own app, or its site when the app is not installed, shows a person.
pub fn user_link(id: &str) -> String {
    format!("https://discord.com/users/{id}")
}

/// Unix seconds from a Discord id, which starts with a timestamp.
pub fn seconds(id: &str) -> i64 {
    let id: u64 = id.parse().unwrap_or(0);
    (((id >> 22) + 1_420_070_400_000) / 1000) as i64
}

pub fn number(id: &str) -> u64 {
    id.parse().unwrap_or(0)
}

/// Discord sends some numbers as strings (permissions) and some ids as either.
fn lenient_u64<'de, D: Deserializer<'de>>(de: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Number(u64),
        Text(String),
    }
    Ok(match Option::<Either>::deserialize(de)? {
        Some(Either::Number(number)) => number,
        Some(Either::Text(text)) => text.parse().unwrap_or(0),
        None => 0,
    })
}

/// An overwrite's `type` is 0 or 1, and `"role"` or `"member"` on old payloads.
fn overwrite_kind<'de, D: Deserializer<'de>>(de: D) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Number(u8),
        Text(String),
    }
    Ok(match Either::deserialize(de)? {
        Either::Number(number) => number == 1,
        Either::Text(text) => text == "member" || text == "1",
    })
}

/// A field that is `null`, absent or a value: all but the last are `None`.
fn nullable<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(de: D) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(de)?.unwrap_or_default())
}

#[derive(Deserialize, Default, Clone, Debug)]
pub struct User {
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub discriminator: String,
    #[serde(default, deserialize_with = "nullable")]
    pub bot: bool,
    /// Discord's own account, which writes the system DMs.
    #[serde(default, deserialize_with = "nullable")]
    pub system: bool,
    /// Our own plan, on READY's user: 0 none, 1 Nitro Classic, 2 Nitro, 3 Nitro Basic.
    #[serde(default, deserialize_with = "nullable")]
    pub premium_type: u8,
    /// Present on the users of a message's `mentions`.
    #[serde(default)]
    pub member: Option<Box<Member>>,
}

impl User {
    pub fn name(&self) -> &str {
        match self.global_name.as_deref() {
            Some(name) if !name.is_empty() => name,
            _ => &self.username,
        }
    }

    /// The picture at list size, and where it is fetched from. No session is needed.
    pub fn avatar(&self) -> (MediaFile, String) {
        avatar(&self.id, self.avatar.as_deref(), &self.discriminator)
    }

    /// The row People stores. `name` wins over the user's own names (a friend nickname).
    pub fn person(&self, name: Option<&str>) -> PersonRow {
        PersonRow {
            link: user_link(&self.id),
            id: self.id.clone(),
            name: name.filter(|name| !name.is_empty()).unwrap_or(self.name()).to_owned(),
            handle: self.username.clone(),
            avatar: Some(self.avatar().0.id),
            bot: self.bot,
            status: String::new(),
        }
    }
}

fn sized(id: String, kind: MediaKind, side: u32) -> MediaFile {
    MediaFile {
        width: side,
        height: side,
        ..MediaFile::new(id, kind)
    }
}

/// What a CDN address adds for a picture that moves: Discord marks those with an `a_` hash,
/// and serves their WebP still unless `animated=true` is asked for. The same small file then
/// holds every frame; the window plays it under the pointer or as its setting says.
fn moving(hash: &str) -> &'static str {
    if hash.starts_with("a_") {
        "&animated=true"
    } else {
        ""
    }
}

pub fn avatar(user: &str, hash: Option<&str>, discriminator: &str) -> (MediaFile, String) {
    match hash {
        Some(hash) if !hash.is_empty() => (
            sized(format!("avatar:{user}:{hash}"), MediaKind::Avatar, 80),
            format!("{}/avatars/{user}/{hash}.webp?size=80{}", cdn(), moving(hash)),
        ),
        _ => {
            // New-style names (discriminator 0) pick by id, old ones by discriminator.
            let index = match discriminator.parse::<u64>() {
                Ok(old) if old > 0 => old % 5,
                _ => (number(user) >> 22) % 6,
            };
            (
                sized(format!("avatar:default:{index}"), MediaKind::Avatar, 80),
                format!("{}/embed/avatars/{index}.png", cdn()),
            )
        }
    }
}

/// A registered avatar at the sizes a profile card draws it (80 pt, so 160 px on a 2x screen;
/// the CDN serves powers of two). `None` for a default avatar, which has one size, and for an
/// id that is not an avatar's.
pub fn avatar_variants(file: &str) -> Option<Vec<(u32, String)>> {
    let mut parts = file.strip_prefix("avatar:")?.split(':');
    let (user, hash) = (parts.next()?, parts.next()?);
    if user == "default" {
        return None;
    }
    Some([128, 256].into_iter().map(|size| (size, format!("{}/avatars/{user}/{hash}.webp?size={size}{}", cdn(), moving(hash)))).collect())
}

pub fn guild_icon(guild: &str, hash: &str) -> (MediaFile, String) {
    (
        sized(format!("icon:{guild}:{hash}"), MediaKind::Icon, 96),
        format!("{}/icons/{guild}/{hash}.webp?size=96{}", cdn(), moving(hash)),
    )
}

/// A group direct message's own picture. The address is from the community documentation and
/// unconfirmed against a real account.
pub fn group_icon(channel: &str, hash: &str) -> (MediaFile, String) {
    (
        sized(format!("gicon:{channel}:{hash}"), MediaKind::Avatar, 80),
        format!("{}/channel-icons/{channel}/{hash}.webp?size=80{}", cdn(), moving(hash)),
    )
}

/// A custom emoji at 48 px; an animated one as an animated WebP of that size, not its GIF.
pub fn emoji(id: &str, animated: bool) -> (MediaFile, String) {
    (
        sized(format!("emoji:{id}"), MediaKind::Emoji, 48),
        format!("{}/emojis/{id}.webp?size=48{}", cdn(), if animated { "&animated=true" } else { "" }),
    )
}

#[derive(Deserialize, Default, Clone, Debug)]
pub struct Member {
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub presence: Option<PresenceUpdate>,
}

impl Member {
    pub fn id(&self) -> &str {
        match (&self.user, &self.user_id) {
            (Some(user), _) => &user.id,
            (None, Some(id)) => id,
            (None, None) => "",
        }
    }

    pub fn name(&self) -> &str {
        match (self.nick.as_deref(), &self.user) {
            (Some(nick), _) if !nick.is_empty() => nick,
            (_, Some(user)) => user.name(),
            _ => "",
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct Overwrite {
    pub id: String,
    #[serde(rename = "type", deserialize_with = "overwrite_kind")]
    pub member: bool,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub allow: u64,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub deny: u64,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ThreadMetadata {
    #[serde(default)]
    pub archived: bool,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Channel {
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: u8,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub position: i32,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub last_message_id: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub permission_overwrites: Vec<Overwrite>,
    /// Whole users, or only their ids when the session asked for deduplicated users.
    #[serde(default)]
    pub recipients: Vec<User>,
    #[serde(default)]
    pub recipient_ids: Vec<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub thread_metadata: Option<ThreadMetadata>,
    #[serde(default)]
    pub message_count: Option<u32>,
    /// Our own membership of a thread, present when we are in it.
    #[serde(default)]
    pub member: Option<IgnoredAny>,
    /// A DM from someone who is not a friend, waiting behind Message Requests, and one Discord
    /// judged spam.
    #[serde(default, deserialize_with = "nullable")]
    pub is_message_request: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub is_spam: bool,
}

/// What is kept of a server's channel between connects, in a note: enough to work out again
/// who may see it when a role or our own membership changes. Short names, because a large
/// server has hundreds.
#[derive(Deserialize, Serialize)]
pub struct Kept {
    #[serde(rename = "i")]
    pub id: String,
    #[serde(rename = "k")]
    pub kind: u8,
    #[serde(rename = "n", default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(rename = "p", default)]
    pub position: i32,
    #[serde(rename = "u", default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(rename = "t", default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    #[serde(rename = "o", default, skip_serializing_if = "Vec::is_empty")]
    pub overwrites: Vec<(String, bool, u64, u64)>,
}

impl Kept {
    pub fn of(channel: &Channel) -> Kept {
        let overwrites = channel.permission_overwrites.iter();
        Kept {
            id: channel.id.clone(),
            kind: channel.kind,
            name: channel.name.clone(),
            position: channel.position,
            parent: channel.parent_id.clone(),
            topic: channel.topic.clone().filter(|topic| !topic.is_empty()),
            overwrites: overwrites.map(|over| (over.id.clone(), over.member, over.allow, over.deny)).collect(),
        }
    }

    pub fn channel(self) -> Channel {
        let overwrites = self.overwrites.into_iter().map(|(id, member, allow, deny)| Overwrite { id, member, allow, deny });
        Channel {
            id: self.id,
            kind: self.kind,
            name: self.name,
            position: self.position,
            parent_id: self.parent,
            topic: self.topic,
            permission_overwrites: overwrites.collect(),
            ..Channel::default()
        }
    }
}

impl Channel {
    pub fn is_thread(&self) -> bool {
        matches!(self.kind, NEWS_THREAD | PUBLIC_THREAD | PRIVATE_THREAD)
    }

    pub fn is_voice(&self) -> bool {
        matches!(self.kind, VOICE | STAGE)
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct Role {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub permissions: u64,
}

/// What newer sessions wrap a guild's own fields in.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct GuildProperties {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct Guild {
    pub id: String,
    #[serde(default)]
    pub properties: Option<GuildProperties>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub threads: Vec<Channel>,
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub joined_at: Option<String>,
    #[serde(default)]
    pub members: Vec<Member>,
    #[serde(default)]
    pub voice_states: Vec<VoiceState>,
    #[serde(default)]
    pub unavailable: Option<bool>,
    /// The server's own emoji and stickers, top-level even when the rest is in `properties`.
    #[serde(default, deserialize_with = "nullable")]
    pub emojis: Vec<GuildEmoji>,
    #[serde(default, deserialize_with = "nullable")]
    pub stickers: Vec<GuildSticker>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct GuildEmoji {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub animated: bool,
    /// False once the server lost the boosts that paid for it.
    #[serde(default = "yes", deserialize_with = "nullable_yes")]
    pub available: bool,
    /// The roles allowed to use it; empty for everyone.
    #[serde(default, deserialize_with = "nullable")]
    pub roles: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct GuildSticker {
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    /// 1 PNG, 2 APNG, 3 Lottie, 4 GIF.
    #[serde(default, deserialize_with = "nullable")]
    pub format_type: u8,
    #[serde(default = "yes", deserialize_with = "nullable_yes")]
    pub available: bool,
}

fn yes() -> bool {
    true
}

/// A flag that is on unless it says otherwise.
fn nullable_yes<'de, D: Deserializer<'de>>(de: D) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(de)?.unwrap_or(true))
}

/// A sticker's picture from the media proxy, registered as a message shows it (`content.rs`), so
/// the picker and a sent sticker share one file. Lottie JSON is rendered by the shell.
pub fn sticker(id: &str, format: u8) -> Option<(MediaFile, String)> {
    let (kind, url) = match format {
        1 => (MediaKind::Image, format!("{}/stickers/{id}.png?size=160", media())),
        2 => (MediaKind::Image, apng_sticker(id)),
        3 => (MediaKind::File, format!("{}/stickers/{id}.json", cdn())),
        4 => (MediaKind::Gif, format!("{}/stickers/{id}.gif", media())),
        _ => return None,
    };
    Some((sized(format!("sticker:{id}"), kind, 160), url))
}

/// An APNG sticker, frames and all: the proxy flattens one to a still unless asked to pass it
/// through. The parameter is the web client's, unconfirmed against a real account.
pub fn apng_sticker(id: &str) -> String {
    format!("{}/stickers/{id}.png?size=160&passthrough=true", media())
}

impl Guild {
    pub fn name(&self) -> &str {
        let wrapped = self.properties.as_ref().and_then(|properties| properties.name.as_deref());
        wrapped.or(self.name.as_deref()).unwrap_or("Server")
    }

    pub fn icon(&self) -> Option<&str> {
        let wrapped = self.properties.as_ref().and_then(|properties| properties.icon.as_deref());
        wrapped.or(self.icon.as_deref())
    }

    pub fn owner(&self) -> &str {
        let wrapped = self.properties.as_ref().and_then(|properties| properties.owner_id.as_deref());
        wrapped.or(self.owner_id.as_deref()).unwrap_or("")
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct Relationship {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: u8,
    #[serde(default)]
    pub nickname: Option<String>,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub user_id: Option<String>,
}

impl Relationship {
    pub fn person(&self) -> &str {
        match (&self.user, &self.user_id) {
            (Some(user), _) => &user.id,
            (None, Some(id)) => id,
            (None, None) => &self.id,
        }
    }

    pub fn relation(&self) -> protocol::Relation {
        match self.kind {
            1 => protocol::Relation::Friend,
            2 => protocol::Relation::Blocked,
            3 => protocol::Relation::Incoming,
            4 => protocol::Relation::Outgoing,
            _ => protocol::Relation::None,
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct ReadState {
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub mention_count: u32,
    /// A string, or the number 0 for "never".
    #[serde(default, deserialize_with = "lenient_u64")]
    pub last_message_id: u64,
    /// 0 is a channel; the others (events, onboarding, the inbox) are not conversations.
    #[serde(default, deserialize_with = "nullable")]
    pub read_state_type: u8,
}

/// A list on older sessions, wrapped with a version on newer ones.
#[derive(Deserialize)]
#[serde(untagged)]
pub enum Versioned<T> {
    List(Vec<T>),
    Wrapped { entries: Vec<T> },
}

impl<T> Versioned<T> {
    pub fn into_entries(self) -> Vec<T> {
        match self {
            Versioned::List(entries) | Versioned::Wrapped { entries } => entries,
        }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct MuteConfig {
    /// ISO 8601, or null for "until I turn it back on".
    #[serde(default)]
    pub end_time: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct ChannelOverride {
    pub channel_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub muted: bool,
    #[serde(default)]
    pub mute_config: Option<MuteConfig>,
    /// 0 all, 1 mentions, 2 nothing, 3 as the server says.
    #[serde(default = "inherit", deserialize_with = "nullable_inherit")]
    pub message_notifications: u8,
    /// `OPT_IN_ENABLED` and the like.
    #[serde(default, deserialize_with = "nullable")]
    pub flags: u32,
}

fn inherit() -> u8 {
    3
}

fn nullable_inherit<'de, D: Deserializer<'de>>(de: D) -> Result<u8, D::Error> {
    Ok(Option::<u8>::deserialize(de)?.unwrap_or(3))
}

/// Guild settings flags: the user turned "Show all channels" off, so only opted-in ones show.
pub const OPT_IN_CHANNELS_ON: u32 = 1 << 14;
/// Channel override flags: this channel (or category) is opted in.
pub const OPT_IN_ENABLED: u32 = 1 << 12;

#[derive(Deserialize, Clone, Debug)]
pub struct GuildSettings {
    /// Null for the entry that holds direct messages.
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub muted: bool,
    #[serde(default)]
    pub mute_config: Option<MuteConfig>,
    #[serde(default, deserialize_with = "nullable")]
    pub suppress_everyone: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub suppress_roles: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub channel_overrides: Vec<ChannelOverride>,
    #[serde(default, deserialize_with = "nullable")]
    pub hide_muted_channels: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub flags: u32,
    /// 0 all, 1 mentions, 2 nothing.
    #[serde(default, deserialize_with = "nullable")]
    pub message_notifications: u8,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Activity {
    #[serde(rename = "type", default)]
    pub kind: u8,
    #[serde(default)]
    pub state: Option<String>,
    /// The game, the service, the show.
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    /// A custom status's emoji: a Unicode one has no id.
    #[serde(default)]
    pub emoji: Option<Emoji>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct PresenceUser {
    pub id: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct PresenceUpdate {
    #[serde(default)]
    pub user: Option<PresenceUser>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub status: String,
    #[serde(default, deserialize_with = "nullable")]
    pub activities: Vec<Activity>,
}

impl PresenceUpdate {
    pub fn id(&self) -> &str {
        match (&self.user, &self.user_id) {
            (Some(user), _) => &user.id,
            (None, Some(id)) => id,
            (None, None) => "",
        }
    }

    pub fn presence(&self) -> Presence {
        match self.status.as_str() {
            "online" => Presence::Online,
            "idle" => Presence::Idle,
            "dnd" => Presence::Dnd,
            "offline" | "invisible" => Presence::Offline,
            _ => Presence::Unknown,
        }
    }

    /// The custom status line (activity type 4).
    pub fn status(&self) -> &str {
        let custom = self.activities.iter().find(|activity| activity.kind == 4);
        custom.and_then(|activity| activity.state.as_deref()).unwrap_or("")
    }

    /// The custom status's emoji when it is a Unicode one; a server's own has no glyph to show.
    pub fn emoji(&self) -> &str {
        let custom = self.activities.iter().find(|activity| activity.kind == 4);
        let emoji = custom.and_then(|activity| activity.emoji.as_ref()).filter(|emoji| emoji.id.is_none());
        emoji.and_then(|emoji| emoji.name.as_deref()).unwrap_or("")
    }

    /// What they are doing, as the first activity that is not the custom status says.
    pub fn doing(&self) -> Option<protocol::Activity> {
        use protocol::ActivityKind::*;
        self.activities.iter().find_map(|activity| {
            let kind = match activity.kind {
                0 => Playing,
                1 => Streaming,
                2 => Listening,
                3 => Watching,
                5 => Competing,
                _ => return None,
            };
            let mut doing = protocol::Activity {
                kind,
                name: activity.name.clone(),
                place: String::new(),
                conversation: None,
            };
            doing.bound();
            Some(doing)
        })
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
pub struct VoiceState {
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub self_mute: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub self_deaf: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub mute: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub deaf: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub self_stream: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub self_video: bool,
    #[serde(default)]
    pub member: Option<Member>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Emoji {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub animated: bool,
}

impl Emoji {
    /// How the protocol spells it, and its picture when it is a custom one.
    pub fn spelled(&self) -> (String, Option<(MediaFile, String)>) {
        let name = self.name.as_deref().unwrap_or("emoji");
        match &self.id {
            Some(id) => (format!(":{name}:"), Some(emoji(id, self.animated))),
            None => (name.to_owned(), None),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct Attachment {
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default, deserialize_with = "nullable")]
    pub size: u64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub width: u32,
    #[serde(default, deserialize_with = "nullable")]
    pub height: u32,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub duration_secs: f64,
    /// A voice message's loudness, base64 of one byte a bar.
    #[serde(default)]
    pub waveform: Option<String>,
    /// Bit 3 marks a spoiler; older clients say it with the file's name.
    #[serde(default, deserialize_with = "nullable")]
    pub flags: u32,
}

impl Attachment {
    pub fn spoiler(&self) -> bool {
        self.flags & (1 << 3) != 0 || self.filename.starts_with("SPOILER_")
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct EmbedImage {
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub width: u32,
    #[serde(default, deserialize_with = "nullable")]
    pub height: u32,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Named {
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct EmbedField {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Footer {
    #[serde(default, deserialize_with = "nullable")]
    pub text: String,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Embed {
    /// `rich` is what a bot or webhook composed; the others are built from a link.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub footer: Option<Footer>,
    #[serde(default)]
    pub color: Option<u32>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub provider: Option<Named>,
    #[serde(default)]
    pub author: Option<Named>,
    #[serde(default)]
    pub thumbnail: Option<EmbedImage>,
    #[serde(default)]
    pub image: Option<EmbedImage>,
    /// The moving picture of a `gifv` (Tenor, Giphy) or `video` embed. Only its `proxy_url`,
    /// on Discord's own media host, is ever fetched.
    #[serde(default)]
    pub video: Option<EmbedImage>,
    #[serde(default, deserialize_with = "nullable")]
    pub fields: Vec<EmbedField>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Reacted {
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub me: bool,
    pub emoji: Emoji,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Reference {
    #[serde(default)]
    pub message_id: Option<String>,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub guild_id: Option<String>,
    /// 0 is a reply, 1 a forward.
    #[serde(rename = "type", default)]
    pub kind: u8,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Sticker {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// 1 PNG, 2 APNG, 3 Lottie, 4 GIF.
    #[serde(default)]
    pub format_type: u8,
}

#[derive(Deserialize, Clone, Debug)]
pub struct PollText {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Poll {
    #[serde(default)]
    pub question: Option<PollText>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct Snapshot {
    pub message: Box<Posted>,
}

/// A message, whole or (in `MESSAGE_UPDATE`) only the fields that changed.
#[derive(Deserialize, Clone, Debug, Default)]
pub struct Posted {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub channel_id: String,
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub author: Option<User>,
    #[serde(default)]
    pub member: Option<Member>,
    #[serde(default)]
    pub webhook_id: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub attachments: Option<Vec<Attachment>>,
    #[serde(default)]
    pub embeds: Option<Vec<Embed>>,
    #[serde(default, deserialize_with = "nullable")]
    pub mentions: Vec<User>,
    #[serde(default, deserialize_with = "nullable")]
    pub mention_roles: Vec<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub mention_everyone: bool,
    #[serde(default)]
    pub reactions: Option<Vec<Reacted>>,
    #[serde(default)]
    pub pinned: Option<bool>,
    #[serde(rename = "type", default)]
    pub kind: Option<u8>,
    #[serde(default)]
    pub message_reference: Option<Reference>,
    #[serde(default)]
    pub referenced_message: Option<Box<Posted>>,
    #[serde(default)]
    pub thread: Option<Channel>,
    #[serde(default, deserialize_with = "nullable")]
    pub sticker_items: Vec<Sticker>,
    #[serde(default)]
    pub poll: Option<Poll>,
    #[serde(default, deserialize_with = "nullable")]
    pub components: Vec<IgnoredAny>,
    #[serde(default, deserialize_with = "nullable")]
    pub message_snapshots: Vec<Snapshot>,
    /// A string from our own client, a number from some others.
    #[serde(default, deserialize_with = "nonce")]
    pub nonce: Option<String>,
}

fn nonce<'de, D: Deserializer<'de>>(de: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Either {
        Number(u64),
        Text(String),
    }
    Ok(match Option::<Either>::deserialize(de).map_err(de::Error::custom)? {
        Some(Either::Number(number)) => Some(number.to_string()),
        Some(Either::Text(text)) => Some(text),
        None => None,
    })
}

#[derive(Deserialize, Default)]
pub struct ProfileText {
    #[serde(default, deserialize_with = "nullable")]
    pub bio: String,
    #[serde(default, deserialize_with = "nullable")]
    pub pronouns: String,
    #[serde(default)]
    pub accent_color: Option<u32>,
}

/// Their membership of the server asked about (`guild_id` on the profile request).
#[derive(Deserialize, Default)]
pub struct ProfileMember {
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub roles: Vec<String>,
    #[serde(default)]
    pub joined_at: Option<String>,
}

#[derive(Deserialize)]
pub struct MutualGuild {
    pub id: String,
}

/// `GET /users/{id}/profile`: the parts a person's card shows.
#[derive(Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub user_profile: Option<ProfileText>,
    #[serde(default, deserialize_with = "nullable")]
    pub mutual_guilds: Vec<MutualGuild>,
    #[serde(default)]
    pub guild_member: Option<ProfileMember>,
}

impl Profile {
    /// Their own words, cut to what a card holds.
    pub fn bio(&self) -> String {
        let mut bio = self.user_profile.as_ref().map_or_else(String::new, |profile| profile.bio.clone());
        protocol::clip(&mut bio, 1_000);
        bio
    }
}

/// Unix seconds from an ISO 8601 time as Discord writes it (`2026-10-13T08:15:00.123000+00:00`).
/// Discord always sends UTC, so the offset is not read.
pub fn iso_seconds(text: &str) -> Option<i64> {
    let part = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (part(0..4)?, part(5..7)?, part(8..10)?);
    let (hour, minute, second) = (part(11..13)?, part(14..16)?, part(17..19)?);
    // Days from the civil date (Howard Hinnant's algorithm).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let of_era = year.rem_euclid(400);
    let of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let days = era * 146_097 + of_era * 365 + of_era / 4 - of_era / 100 + of_year - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_carry_their_time() {
        // The example id from Discord's documentation, created 2016-04-30 11:18:25 UTC.
        assert_eq!(seconds("175928847299117063"), 1_462_015_105);
        assert_eq!(iso_seconds("2016-04-30T11:18:25.796000+00:00"), Some(1_462_015_105));
    }

    #[test]
    fn a_user_without_a_picture_gets_the_default_for_their_id() {
        let (file, url) = avatar("175928847299117063", None, "0");
        let index = (175_928_847_299_117_063u64 >> 22) % 6;
        assert_eq!(url, format!("{}/embed/avatars/{index}.png", cdn()));
        assert_eq!(file.id, format!("avatar:default:{index}"));
    }
}
