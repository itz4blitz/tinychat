//! Discord's messages into the one message model: markdown to text and spans, attachments,
//! embeds, stickers, reactions, replies, system lines.

use super::model::{self, seconds, Attachment as Attached, Embed, EmbedImage, Posted};
use crate::adapters::Context;
use base64::Engine;
use protocol::{
    AccountKey, Attachment, Card, Forwarded, Key, MediaFile, MediaKind, MentionKind, Message,
    MessageKind, Preview, Reaction, ReplyTo, Sender, Span, Style, ThreadSummary,
};

/// Names for what a message only gives ids for.
pub trait Names {
    fn user(&self, id: &str) -> Option<String>;
    fn role(&self, id: &str) -> Option<String>;
    fn channel(&self, id: &str) -> Option<String>;
    /// A server's name.
    fn space(&self, id: &str) -> Option<String>;
}

/// The files something refers to: each with the address it is fetched from, and the other
/// sizes Discord serves of those that are pictures.
#[derive(Default)]
pub struct Files {
    pub files: Vec<(MediaFile, String)>,
    /// A file's id, and `(longest side, address)` for each larger size.
    pub sizes: Vec<(String, Vec<(u32, String)>)>,
}

impl Files {
    pub fn push(&mut self, file: (MediaFile, String)) {
        self.files.push(file);
    }

    /// Adds what `more` holds that is not here yet.
    pub fn merge(&mut self, more: Files) {
        for file in more.files {
            if !self.files.iter().any(|(known, _)| known.id == file.0.id) {
                self.files.push(file);
            }
        }
        for sizes in more.sizes {
            if !self.sizes.iter().any(|(known, _)| *known == sizes.0) {
                self.sizes.push(sizes);
            }
        }
    }

    /// Registers the files, then their sizes. Before recording whatever refers to them.
    pub fn register(&self, ctx: &Context) -> rusqlite::Result<()> {
        ctx.media(&self.files)?;
        for (id, sizes) in &self.sizes {
            ctx.media_variants(id, sizes)?;
        }
        Ok(())
    }
}

/// Text being built, with the ranges styled so far and the files they refer to.
#[derive(Default)]
pub struct Rich {
    pub text: String,
    pub spans: Vec<Span>,
    pub files: Files,
}

impl Rich {
    fn span(&mut self, start: usize, style: Style) {
        if self.text.len() > start {
            self.spans.push(Span {
                start: start as u32,
                end: self.text.len() as u32,
                style,
            });
        }
    }
}

/// Where the unescaped `delimiter` next closes, searching `text` from `from`.
fn closing(text: &str, from: usize, delimiter: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut at = from;
    while at < bytes.len() {
        if bytes[at] == b'\\' {
            at += 1;
            at += text[at..].chars().next().map_or(0, char::len_utf8);
            continue;
        }
        if text[at..].starts_with(delimiter) {
            return (at > from).then_some(at);
        }
        at += text[at..].chars().next().map_or(1, char::len_utf8);
    }
    None
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// `<t:1618953630:f>` as words, in the machine's own time zone.
fn timestamp(unix: i64, style: &str) -> String {
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September",
        "October", "November", "December",
    ];
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    let unix = unix as libc::time_t;
    // SAFETY: platform localtime helpers fill the struct we hand them.
    unsafe {
        #[cfg(unix)]
        libc::localtime_r(&unix, &mut local);
        #[cfg(windows)]
        {
            if libc::localtime_s(&mut local, &unix) != 0 {
                return String::new();
            }
        }
    }
    let local = local;
    let month = MONTHS[(local.tm_mon.clamp(0, 11)) as usize];
    let weekday = DAYS[(local.tm_wday.clamp(0, 6)) as usize];
    let (day, year) = (local.tm_mday, local.tm_year + 1900);
    let time = format!("{}:{:02}", local.tm_hour, local.tm_min);
    match style {
        "t" => time,
        "T" => format!("{time}:{:02}", local.tm_sec),
        "d" => format!("{year}-{:02}-{day:02}", local.tm_mon + 1),
        "D" => format!("{day} {month} {year}"),
        "F" => format!("{weekday}, {day} {month} {year} {time}"),
        // Relative times ("in 2 hours") would go stale in a stored message, so they are dated.
        _ => format!("{day} {month} {year} {time}"),
    }
}

/// One `<...>` token: a mention, a custom emoji, a time, a link without a preview. Returns how
/// many bytes of `text` it took.
fn angle(rich: &mut Rich, text: &str, names: &dyn Names) -> Option<usize> {
    let end = text.find('>')?;
    let inner = &text[1..end];
    let start = rich.text.len();
    let mention = |rich: &mut Rich, shown: String, kind: MentionKind, id: &str| {
        rich.text.push_str(&shown);
        rich.span(start, Style::Mention { kind, id: id.to_owned() });
    };
    if let Some(id) = inner.strip_prefix("@&").filter(|id| digits(id)) {
        let name = names.role(id).unwrap_or_else(|| "role".to_owned());
        mention(rich, format!("@{name}"), MentionKind::Role, id);
    } else if let Some(id) = inner.strip_prefix('@').map(|id| id.trim_start_matches('!')).filter(|id| digits(id)) {
        let name = names.user(id).unwrap_or_else(|| "someone".to_owned());
        mention(rich, format!("@{name}"), MentionKind::User, id);
    } else if let Some(id) = inner.strip_prefix('#').filter(|id| digits(id)) {
        let name = names.channel(id).unwrap_or_else(|| "channel".to_owned());
        mention(rich, format!("#{name}"), MentionKind::Channel, id);
    } else if let Some(emoji) = inner.strip_prefix("a:").or_else(|| inner.strip_prefix(':')) {
        let (name, id) = emoji.split_once(':')?;
        if !digits(id) || name.is_empty() {
            return None;
        }
        let (file, url) = model::emoji(id, inner.starts_with("a:"));
        rich.text.push_str(&format!(":{name}:"));
        rich.span(start, Style::Emoji { file: file.clone() });
        rich.files.push((file, url));
    } else if let Some(time) = inner.strip_prefix("t:") {
        let (unix, style) = time.split_once(':').unwrap_or((time, "f"));
        rich.text.push_str(&timestamp(unix.parse().ok()?, style));
    } else if let Some(command) = inner.strip_prefix('/') {
        let (name, id) = command.rsplit_once(':')?;
        if !digits(id) {
            return None;
        }
        rich.text.push_str(&format!("/{name}"));
    } else if inner.starts_with("https://") || inner.starts_with("http://") {
        rich.text.push_str(inner);
        rich.span(start, Style::Link { url: inner.to_owned() });
    } else {
        return None;
    }
    Some(end + 1)
}

/// The length of the bare link `text` starts with.
fn bare_link(text: &str) -> usize {
    let mut end = text.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '|')).unwrap_or(text.len());
    // Punctuation after a link belongs to the sentence, and so does an unmatched bracket.
    while let Some(last) = text[..end].chars().next_back() {
        let unmatched = last == ')' && !text[..end].contains('(');
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '*' | '_' | '~') || unmatched {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// Inline markdown of one line.
fn inline(rich: &mut Rich, text: &str, names: &dyn Names) {
    const PAIRS: [(&str, Option<Style>); 5] = [
        ("**", Some(Style::Bold)),
        ("__", Some(Style::Underline)),
        ("~~", Some(Style::Strike)),
        ("||", Some(Style::Spoiler)),
        ("``", None),
    ];
    let mut at = 0;
    'scan: while at < text.len() {
        let rest = &text[at..];
        let start = rich.text.len();
        let first = rest.chars().next().unwrap_or(' ');
        // What comes before decides whether `_` and a link start here.
        let after_word = text[..at].chars().next_back().is_some_and(char::is_alphanumeric);

        if first == '\\' {
            if let Some(escaped) = rest[1..].chars().next().filter(char::is_ascii_punctuation) {
                rich.text.push(escaped);
                at += 1 + escaped.len_utf8();
                continue;
            }
        }
        for (delimiter, style) in &PAIRS {
            if !rest.starts_with(delimiter) {
                continue;
            }
            if let Some(close) = closing(rest, delimiter.len(), delimiter) {
                let inner = &rest[delimiter.len()..close];
                match style {
                    Some(style) => {
                        inline(rich, inner, names);
                        rich.span(start, style.clone());
                    }
                    None => {
                        rich.text.push_str(inner.trim());
                        rich.span(start, Style::Code);
                    }
                }
                at += close + delimiter.len();
                continue 'scan;
            }
        }
        match first {
            '`' => {
                if let Some(close) = closing(rest, 1, "`") {
                    rich.text.push_str(&rest[1..close]);
                    rich.span(start, Style::Code);
                    at += close + 1;
                    continue;
                }
            }
            '*' | '_' if !(first == '_' && after_word) => {
                let delimiter = &rest[..1];
                let close = closing(rest, 1, delimiter).filter(|close| {
                    let inner = &rest[1..*close];
                    let word_follows = rest[close + 1..].chars().next().is_some_and(char::is_alphanumeric);
                    !inner.starts_with(char::is_whitespace)
                        && !inner.ends_with(char::is_whitespace)
                        && !(first == '_' && word_follows)
                });
                if let Some(close) = close {
                    inline(rich, &rest[1..close], names);
                    rich.span(start, Style::Italic);
                    at += close + 1;
                    continue;
                }
            }
            '[' => {
                // A masked link: [words](https://example.com).
                let masked = closing(rest, 1, "](").and_then(|label_end| {
                    let target = &rest[label_end + 2..];
                    let target_end = target.find(')')?;
                    let url = target[..target_end].trim_matches(|c| c == '<' || c == '>');
                    let url = url.split_whitespace().next()?;
                    (url.starts_with("https://") || url.starts_with("http://"))
                        .then(|| (label_end, url.to_owned(), label_end + 2 + target_end + 1))
                });
                if let Some((label_end, url, taken)) = masked {
                    inline(rich, &rest[1..label_end], names);
                    rich.span(start, Style::Link { url });
                    at += taken;
                    continue;
                }
            }
            '<' => {
                if let Some(taken) = angle(rich, rest, names) {
                    at += taken;
                    continue;
                }
            }
            '@' => {
                let everyone = ["@everyone", "@here"].into_iter().find(|word| rest.starts_with(word));
                if let Some(word) = everyone {
                    rich.text.push_str(word);
                    let id = word[1..].to_owned();
                    rich.span(start, Style::Mention { kind: MentionKind::Everyone, id });
                    at += word.len();
                    continue;
                }
            }
            'h' if !after_word && (rest.starts_with("https://") || rest.starts_with("http://")) => {
                let end = bare_link(rest);
                if end > "https://".len() {
                    rich.text.push_str(&rest[..end]);
                    rich.span(start, Style::Link { url: rest[..end].to_owned() });
                    at += end;
                    continue;
                }
            }
            _ => {}
        }
        rich.text.push(first);
        at += first.len_utf8();
    }
}

/// A list marker at the start of a line: how deep, whether numbered, and where the item begins.
fn list_item(line: &str) -> Option<(u8, bool, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let depth = (indent / 2).min(8) as u8;
    if rest.starts_with("- ") || rest.starts_with("* ") {
        return Some((depth, false, indent + 2));
    }
    let number = rest.find(|c: char| !c.is_ascii_digit()).filter(|end| *end > 0)?;
    rest[number..].starts_with(". ").then_some((depth, true, indent + number + 2))
}

/// The lines outside code blocks: quotes, headings, lists, then inline markdown.
fn lines(rich: &mut Rich, text: &str, names: &dyn Names) {
    let mut quoting = false;
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            rich.text.push('\n');
        }
        let start = rich.text.len();
        let mut line = line;
        let mut quoted = quoting;
        if !quoting {
            if let Some(rest) = line.strip_prefix(">>> ") {
                // Everything from here to the end of the message is quoted.
                (quoting, quoted, line) = (true, true, rest);
            } else if let Some(rest) = line.strip_prefix("> ") {
                (quoted, line) = (true, rest);
            }
        }
        let heading = ["### ", "## ", "# "].into_iter().find_map(|mark| line.strip_prefix(mark));
        if let Some(heading) = heading {
            // The model has no heading; bold is the nearest.
            inline(rich, heading, names);
            rich.span(start, Style::Bold);
        } else if let Some(small) = line.strip_prefix("-# ") {
            inline(rich, small, names);
        } else if let Some((depth, ordered, begins)) = list_item(line) {
            // The marker stays in the text for numbered items, so the numbers are not lost.
            if ordered {
                rich.text.push_str(line[..begins].trim_start());
            }
            inline(rich, &line[begins..], names);
            rich.span(start, Style::ListItem { ordered, depth });
        } else {
            inline(rich, line, names);
        }
        if quoted {
            rich.span(start, Style::Quote);
        }
    }
}

/// Discord markdown into plain text and styled ranges.
pub fn markdown(rich: &mut Rich, text: &str, names: &dyn Names) {
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let Some(close) = closing(rest, open + 3, "```") else {
            break;
        };
        let before = rest[..open].strip_suffix('\n').unwrap_or(&rest[..open]);
        if !before.is_empty() {
            lines(rich, before, names);
        }
        if !rich.text.is_empty() {
            rich.text.push('\n');
        }
        let code = &rest[open + 3..close];
        // A first line of letters alone names the language.
        let (language, code) = match code.split_once('\n') {
            Some((first, body))
                if !first.is_empty() && first.chars().all(|c| c.is_ascii_alphanumeric() || "+-#_.".contains(c)) =>
            {
                (first, body)
            }
            _ => ("", code.strip_prefix('\n').unwrap_or(code)),
        };
        let start = rich.text.len();
        rich.text.push_str(code.strip_suffix('\n').unwrap_or(code));
        rich.span(start, Style::CodeBlock { language: language.to_owned() });
        rest = &rest[close + 3..];
        if let Some(after) = rest.strip_prefix('\n') {
            rest = after;
        }
        if !rest.is_empty() {
            rich.text.push('\n');
        }
    }
    if !rest.is_empty() {
        lines(rich, rest, names);
    }
}

fn media_kind(mime: &str, name: &str) -> MediaKind {
    let lower = name.to_ascii_lowercase();
    match mime {
        "image/gif" => MediaKind::Gif,
        mime if mime.starts_with("image/") => MediaKind::Image,
        mime if mime.starts_with("video/") => MediaKind::Video,
        mime if mime.starts_with("audio/") => MediaKind::Audio,
        "" if lower.ends_with(".gif") => MediaKind::Gif,
        "" if [".png", ".jpg", ".jpeg", ".webp"].iter().any(|end| lower.ends_with(end)) => MediaKind::Image,
        _ => MediaKind::File,
    }
}

/// The size a picture is drawn at on its own: inside 400 x 300 points, never larger than it is,
/// keeping the shape (`docs/design/03-conversation.md`).
fn fitted(width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 || (width <= 400 && height <= 300) {
        return (width, height);
    }
    // Whichever side is further over its limit decides.
    if u64::from(width) * 300 >= u64::from(height) * 400 {
        (400, ((u64::from(height) * 400) / u64::from(width)).max(1) as u32)
    } else {
        (((u64::from(width) * 300) / u64::from(height)).max(1) as u32, 300)
    }
}

/// Discord's media proxy resizes when asked, and answers a still for a video or a GIF. A GIF
/// asked for `animated` comes back as an animated WebP at that size, so a row plays it without
/// the whole original. The parameter names are from the community documentation and
/// unconfirmed against a real account.
fn resized(proxy: &str, width: u32, height: u32, animated: bool) -> String {
    if width == 0 || height == 0 {
        return proxy.to_owned();
    }
    let joiner = if proxy.contains('?') { '&' } else { '?' };
    let moving = if animated { "&animated=true" } else { "" };
    format!("{proxy}{joiner}format=webp&width={width}&height={height}{moving}")
}

/// Whether a proxied picture may move: a GIF, or a WebP, which can.
fn may_move(address: &str) -> bool {
    let path = address.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    path.ends_with(".gif") || path.ends_with(".webp")
}

/// The larger sizes of a picture registered at `fitted`: one and a half, two and three times
/// it, for a 2x screen and for a grid tile, which crops and so draws the whole picture larger
/// than its box. Never larger than the picture is; the picture itself is the last size when
/// it is smaller than the next step.
fn larger(proxy: &str, (width, height): (u32, u32), (full_width, full_height): (u32, u32), animated: bool) -> Vec<(u32, String)> {
    let mut sizes = Vec::new();
    for (times, over) in [(3, 2), (2, 1), (3, 1)] {
        let (wide, high) = (width * times / over, height * times / over);
        if wide >= full_width || high >= full_height {
            if full_width > width {
                sizes.push((full_width.max(full_height), resized(proxy, full_width, full_height, animated)));
            }
            break;
        }
        sizes.push((wide.max(high), resized(proxy, wide, high, animated)));
    }
    sizes
}

/// A voice message's bars: base64 of one byte each.
fn waveform(encoded: Option<&str>) -> Vec<u8> {
    let decoded = encoded.and_then(|encoded| base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok());
    let mut bars = decoded.unwrap_or_default();
    bars.truncate(protocol::WAVEFORM_LIMIT);
    bars
}

fn attachment(rich: &mut Rich, attached: &Attached) -> Attachment {
    let mime = attached.content_type.as_deref().unwrap_or("");
    let mime = mime.split(';').next().unwrap_or("").trim();
    let kind = media_kind(mime, &attached.filename);
    let file = MediaFile {
        width: attached.width,
        height: attached.height,
        bytes: attached.size,
        ..MediaFile::new(format!("att:{}", attached.id), kind)
    };
    rich.files.push((file.clone(), attached.url.clone()));
    let drawn = matches!(kind, MediaKind::Image | MediaKind::Gif | MediaKind::Video);
    let thumb = attached.proxy_url.as_deref().filter(|_| drawn && attached.width > 0).map(|proxy| {
        let (width, height) = fitted(attached.width, attached.height);
        let thumb = MediaFile {
            width,
            height,
            ..MediaFile::new(format!("att:{}:thumb", attached.id), MediaKind::Image)
        };
        let animated = kind == MediaKind::Gif || mime == "image/webp";
        rich.files.push((thumb.clone(), resized(proxy, width, height, animated)));
        let sizes = larger(proxy, (width, height), (attached.width, attached.height), animated);
        if !sizes.is_empty() {
            rich.files.sizes.push((thumb.id.clone(), sizes));
        }
        thumb
    });
    Attachment {
        spoiler: attached.spoiler(),
        sticker: false,
        waveform: waveform(attached.waveform.as_deref()),
        name: attached.filename.clone(),
        file,
        thumb,
        mime: mime.to_owned(),
        duration: attached.duration_secs.round() as u32,
        round: false,
    }
}

/// A sticker is drawn like an attached picture; the shell renders Lottie JSON with tlottie.
fn sticker(rich: &mut Rich, sticker: &model::Sticker) -> Attachment {
    let (kind, url, mime) = match sticker.format_type {
        3 => (MediaKind::File, format!("{}/stickers/{}.json", model::cdn(), sticker.id), "application/json"),
        4 => (MediaKind::Gif, format!("{}/stickers/{}.gif", model::media(), sticker.id), "image/gif"),
        2 => (MediaKind::Image, model::apng_sticker(&sticker.id), "image/png"),
        _ => (MediaKind::Image, format!("{}/stickers/{}.png?size=160", model::media(), sticker.id), "image/png"),
    };
    let file = MediaFile {
        width: 160,
        height: 160,
        ..MediaFile::new(format!("sticker:{}", sticker.id), kind)
    };
    // An APNG's frames are passed through at the size Discord made them.
    if kind == MediaKind::Image && sticker.format_type != 2 {
        let twice = format!("{}/stickers/{}.png?size=320", model::media(), sticker.id);
        rich.files.sizes.push((file.id.clone(), vec![(320, twice)]));
    }
    rich.files.push((file.clone(), url));
    Attachment {
        spoiler: false,
        sticker: true,
        waveform: Vec::new(),
        name: sticker.name.clone(),
        file,
        thumb: None,
        mime: mime.to_owned(),
        duration: 0,
        round: false,
    }
}

/// An embed's picture, registered at the size a row draws it, when Discord's proxy serves it.
fn embed_still(rich: &mut Rich, id: String, picture: &EmbedImage) -> Option<MediaFile> {
    let proxy = picture.proxy_url.as_deref()?;
    let (width, height) = fitted(picture.width, picture.height);
    let file = MediaFile { width, height, ..MediaFile::new(id, MediaKind::Image) };
    let animated = may_move(proxy);
    rich.files.push((file.clone(), resized(proxy, width, height, animated)));
    let sizes = larger(proxy, (width, height), (picture.width, picture.height), animated);
    if !sizes.is_empty() {
        rich.files.sizes.push((file.id.clone(), sizes));
    }
    Some(file)
}

/// A GIF from Tenor or Giphy (`gifv`, an MP4) or a video Discord's proxy serves, as an
/// attachment: its still in the row, the moving picture fetched only when it is played. A
/// video only its site can play (YouTube) has no proxy address and stays a link card.
fn moving(rich: &mut Rich, message: &str, index: usize, embed: &Embed) -> Option<Attachment> {
    let kind = match embed.kind.as_deref() {
        Some("gifv") => MediaKind::Gif,
        Some("video") => MediaKind::Video,
        _ => return None,
    };
    let video = embed.video.as_ref()?;
    let proxy = video.proxy_url.as_deref()?;
    let poster = embed.thumbnail.as_ref().or(embed.image.as_ref())?;
    let thumb = embed_still(rich, format!("embed:{message}:{index}"), poster)?;
    let file = MediaFile {
        width: video.width,
        height: video.height,
        ..MediaFile::new(format!("embed:{message}:{index}:video"), kind)
    };
    rich.files.push((file.clone(), proxy.to_owned()));
    let name = match kind {
        MediaKind::Gif => "GIF".to_owned(),
        _ => embed.title.clone().filter(|title| !title.is_empty()).unwrap_or_else(|| "Video".to_owned()),
    };
    let mut attachment = Attachment::new(name, file);
    attachment.thumb = Some(thumb);
    attachment.mime = "video/mp4".to_owned();
    Some(attachment)
}

/// A card, with a picture only when Discord's own proxy serves it.
fn preview(rich: &mut Rich, message: &str, index: usize, embed: &Embed) -> Preview {
    let mut description = embed.description.clone().unwrap_or_default();
    for field in &embed.fields {
        if !description.is_empty() {
            description.push('\n');
        }
        description.push_str(&format!("{}: {}", field.name, field.value));
    }
    let picture = embed.image.as_ref().or(embed.thumbnail.as_ref());
    let image = picture.and_then(|picture| embed_still(rich, format!("embed:{message}:{index}"), picture));
    let site = embed.provider.as_ref().or(embed.author.as_ref());
    Preview {
        url: embed.url.clone().unwrap_or_default(),
        site: site.map_or_else(String::new, |site| site.name.clone()),
        title: embed.title.clone().unwrap_or_default(),
        description,
        image,
    }
}

/// What a bot or webhook composed, as the one framed card a message can carry. Its text is
/// plain: the markup goes, the words stay.
fn card(embed: &Embed, names: &dyn Names) -> Card {
    let plain = |text: &str| {
        let mut rich = Rich::default();
        markdown(&mut rich, text, names);
        rich.text
    };
    let author = embed.author.as_ref().map_or("", |author| author.name.as_str());
    let title = embed.title.as_deref().filter(|title| !title.is_empty()).unwrap_or(author);
    Card {
        title: plain(title),
        text: plain(embed.description.as_deref().unwrap_or("")),
        fields: embed.fields.iter().map(|field| (plain(&field.name), plain(&field.value))).collect(),
        footer: embed.footer.as_ref().map_or_else(String::new, |footer| footer.text.clone()),
        color: embed.color,
    }
}

fn reactions(rich: &mut Rich, reacted: &[model::Reacted]) -> Vec<Reaction> {
    let mapped = reacted.iter().filter(|reaction| reaction.count > 0).map(|reaction| {
        let (emoji, file) = reaction.emoji.spelled();
        let file = file.map(|(file, url)| {
            rich.files.push((file.clone(), url));
            file
        });
        Reaction {
            emoji,
            file,
            count: reaction.count,
            mine: reaction.me,
        }
    });
    mapped.collect()
}

/// The message as one line of plain text, for a reply's quote.
fn snippet(posted: &Posted, names: &dyn Names) -> String {
    let mut rich = Rich::default();
    markdown(&mut rich, posted.content.as_deref().unwrap_or(""), names);
    let line = rich.text.replace('\n', " ");
    if !line.is_empty() {
        return line.chars().take(120).collect();
    }
    let attached = posted.attachments.as_deref().and_then(|attached| attached.first());
    attached.map_or_else(String::new, |attached| attached.filename.clone())
}

pub fn sender_name(posted: &Posted) -> String {
    let nick = posted.member.as_ref().and_then(|member| member.nick.as_deref()).filter(|nick| !nick.is_empty());
    let own = posted.author.as_ref().map_or("", |author| author.name());
    nick.unwrap_or(own).to_owned()
}

/// The one grey line a system message is. `None` for types that are ordinary messages.
fn system_line(kind: u8, who: &str, content: &str, mentioned: &str) -> Option<String> {
    Some(match kind {
        0 | 19 | 20 | 21 | 23 => return None,
        1 => format!("{who} added {mentioned} to the group."),
        2 if mentioned == who || mentioned.is_empty() => format!("{who} left the group."),
        2 => format!("{who} removed {mentioned} from the group."),
        3 => format!("{who} started a call."),
        4 => format!("{who} changed the group name: {content}"),
        5 => format!("{who} changed the group icon."),
        6 => format!("{who} pinned a message."),
        7 => format!("{who} joined the server."),
        8..=11 => format!("{who} boosted the server."),
        12 => format!("{who} added {content} to this channel."),
        18 => format!("{who} started a thread: {content}"),
        24 => "AutoMod flagged a message.".to_owned(),
        27 => format!("{who} started {content}"),
        28 => format!("{who} ended {content}"),
        29 => format!("{who} is now a speaker."),
        31 => format!("{who} changed the Stage topic: {content}"),
        46 => "A poll has closed.".to_owned(),
        _ => return Some(String::new()),
    })
}

/// Where the official app shows this message.
pub fn jump_link(guild: Option<&str>, channel: &str, message: &str) -> String {
    format!("https://discord.com/channels/{}/{channel}/{message}", guild.unwrap_or("@me"))
}

/// Whose message this is and where, which the payload alone does not always say.
pub struct Place<'a> {
    pub account: &'a AccountKey,
    pub me: &'a str,
    pub guild: Option<&'a str>,
    pub names: &'a dyn Names,
    /// What the author goes by here when it is not what the message says: a friend's nickname.
    pub author: Option<String>,
}

/// A whole message object into the model, with every file it refers to. `mentions_me` is left
/// false: it depends on the user's settings, which the caller knows.
pub fn message(posted: &Posted, place: &Place) -> (Message, Files) {
    let mut rich = Rich::default();
    let author = posted.author.clone().unwrap_or_default();
    let who = place.author.clone().unwrap_or_else(|| sender_name(posted));
    let content = posted.content.as_deref().unwrap_or("");
    let kind = posted.kind.unwrap_or(0);
    let link = jump_link(place.guild, &posted.channel_id, &posted.id);

    let forwarded = posted.message_snapshots.first().map(|snapshot| &*snapshot.message);
    let mut message_kind = MessageKind::Normal;
    let mentioned = posted.mentions.first().map_or("", |user| user.name());
    match system_line(kind, &who, content, mentioned) {
        None => {
            markdown(&mut rich, content, place.names);
            // What was forwarded is the message's own text; a forward has none of its own.
            if let Some(forwarded) = forwarded {
                markdown(&mut rich, forwarded.content.as_deref().unwrap_or(""), place.names);
            }
        }
        Some(line) if line.is_empty() => {
            rich.text = if content.is_empty() { "A message tinyclient cannot show.".to_owned() } else { content.to_owned() };
            message_kind = MessageKind::Unsupported { link: link.clone() };
        }
        Some(line) => {
            rich.text = line;
            message_kind = MessageKind::System;
        }
    }

    let carried = forwarded.unwrap_or(posted);
    let attached = carried.attachments.as_deref().unwrap_or(&[]);
    let mut attachments: Vec<Attachment> = attached.iter().map(|attached| attachment(&mut rich, attached)).collect();
    attachments.extend(carried.sticker_items.iter().map(|item| sticker(&mut rich, item)));
    let embeds = carried.embeds.as_deref().unwrap_or(&[]);
    let composed = carried.author.as_ref().is_some_and(|author| author.bot) || carried.webhook_id.is_some();
    let (card, previews, moving_pictures) = cards(&mut rich, &posted.id, embeds, place.names, composed);
    attachments.extend(moving_pictures);
    let reactions = reactions(&mut rich, posted.reactions.as_deref().unwrap_or(&[]));
    let origin = posted.message_reference.as_ref().filter(|reference| reference.kind == 1 && forwarded.is_some());
    let forwarded_from = origin.map(|origin| {
        let channel = origin.channel_id.as_deref().unwrap_or("");
        let elsewhere = origin.guild_id.as_deref().filter(|guild| Some(*guild) != place.guild);
        Box::new(Forwarded {
            from_title: place.names.channel(channel).map_or_else(String::new, |name| format!("#{name}")),
            from_space: elsewhere.and_then(|guild| place.names.space(guild)).unwrap_or_default(),
            link: match &origin.message_id {
                Some(message) if !channel.is_empty() => jump_link(origin.guild_id.as_deref(), channel, message),
                _ => String::new(),
            },
        })
    });

    if let Some(poll) = &posted.poll {
        let question = poll.question.as_ref().and_then(|question| question.text.as_deref()).unwrap_or("");
        rich.text = format!("Poll: {question}");
        rich.spans.clear();
        message_kind = MessageKind::Unsupported { link: link.clone() };
    } else if message_kind == MessageKind::Normal
        && rich.text.is_empty()
        && attachments.is_empty()
        && previews.is_empty()
        && card.is_none()
    {
        // Buttons, menus, or something newer than this build: never a blank row.
        rich.text = if posted.components.is_empty() {
            "A message tinyclient cannot show.".to_owned()
        } else {
            "Interactive message".to_owned()
        };
        message_kind = MessageKind::Unsupported { link };
    }

    let reply_to = match (&posted.message_reference, &posted.referenced_message) {
        (Some(reference), Some(original)) if reference.kind == 0 => Some(ReplyTo {
            deleted: false,
            // A webhook's author id names the webhook, not a person.
            person: original.author.as_ref().filter(|_| original.webhook_id.is_none()).map(|author| Key::new(place.account, &author.id)),
            id: original.id.clone(),
            sender: sender_name(original),
            snippet: snippet(original, place.names),
        }),
        // The original was deleted, or Discord did not include it.
        (Some(reference), None) if reference.kind == 0 && kind == 19 => reference.message_id.as_ref().map(|id| ReplyTo {
            deleted: true,
            person: None,
            id: id.clone(),
            sender: String::new(),
            snippet: String::new(),
        }),
        _ => None,
    };
    // Discord does not say who replied in a thread or how much of it we read, short of
    // fetching the thread: the faces and the count stay empty.
    let thread = posted.thread.as_ref().map(|thread| ThreadSummary {
        participants: Vec::new(),
        unread: 0,
        id: thread.id.clone(),
        replies: thread.message_count.unwrap_or(0),
        last: thread.last_message_id.as_deref().map_or(0, seconds),
    });

    let (avatar, avatar_url) = author.avatar();
    rich.files.push((avatar.clone(), avatar_url));
    let outgoing = !author.id.is_empty() && author.id == place.me;
    // Our own client's nonce is the shell's client id: digits, at most 25.
    let client_id = posted.nonce.clone().filter(|nonce| outgoing && digits(nonce) && nonce.len() <= 25);
    let message = Message {
        card,
        deleted: false,
        forwarded: forwarded_from,
        id: posted.id.clone(),
        sender: Sender {
            // A webhook's author id names the webhook, not a person.
            id: if posted.webhook_id.is_some() { String::new() } else { author.id.clone() },
            name: who,
            avatar: Some(avatar),
            bot: author.bot || posted.webhook_id.is_some(),
        },
        sent: seconds(&posted.id),
        outgoing,
        text: rich.text,
        spans: rich.spans,
        attachments,
        previews,
        reactions,
        reply_to,
        thread,
        edited: posted.edited_timestamp.is_some(),
        pinned: posted.pinned.unwrap_or(false),
        mentions_me: false,
        kind: message_kind,
        state: protocol::SendState::Sent,
        client_id,
    };
    (message, rich.files)
}

/// The first embed a bot or webhook composed (`by_bot`, type `rich`) is the message's card;
/// every other one is a link's preview: a person's link that Discord unfurled is `rich` too.
/// A composed embed's picture rides along as a preview with nothing but the picture.
fn cards(
    rich: &mut Rich,
    message: &str,
    embeds: &[Embed],
    names: &dyn Names,
    by_bot: bool,
) -> (Option<Box<Card>>, Vec<Preview>, Vec<Attachment>) {
    let mut composed = None;
    let mut previews = Vec::new();
    let mut moving_pictures = Vec::new();
    for (index, embed) in embeds.iter().enumerate() {
        if let Some(attachment) = moving(rich, message, index, embed) {
            moving_pictures.push(attachment);
            continue;
        }
        if !by_bot || composed.is_some() || embed.kind.as_deref() != Some("rich") {
            previews.push(preview(rich, message, index, embed));
            continue;
        }
        composed = Some(Box::new(card(embed, names)));
        if embed.image.is_some() {
            let picture = Embed {
                title: None,
                description: None,
                fields: Vec::new(),
                provider: None,
                author: None,
                thumbnail: None,
                ..embed.clone()
            };
            // Only when Discord's own proxy serves it: a picture elsewhere is never fetched.
            let picture = preview(rich, message, index, &picture);
            if picture.image.is_some() {
                previews.push(picture);
            }
        }
    }
    (composed, previews, moving_pictures)
}

/// Applies the fields a partial `MESSAGE_UPDATE` carries to the message as stored.
pub fn patch(stored: &mut Message, update: &Posted, place: &Place) -> Files {
    let mut rich = Rich::default();
    if let Some(content) = &update.content {
        if stored.kind == MessageKind::Normal {
            markdown(&mut rich, content, place.names);
            stored.text = std::mem::take(&mut rich.text);
            stored.spans = std::mem::take(&mut rich.spans);
        }
    }
    if let Some(attached) = &update.attachments {
        // Stickers and embeds' moving pictures are not attachments to Discord.
        let stickers: Vec<Attachment> = stored
            .attachments
            .iter()
            .filter(|kept| kept.file.id.starts_with("sticker:") || kept.file.id.starts_with("embed:"))
            .cloned()
            .collect();
        stored.attachments = attached.iter().map(|attached| attachment(&mut rich, attached)).collect();
        stored.attachments.extend(stickers);
    }
    if let Some(embeds) = &update.embeds {
        let (card, previews, moving_pictures) = cards(&mut rich, &stored.id, embeds, place.names, stored.sender.bot);
        stored.card = card;
        stored.previews = previews;
        stored.attachments.retain(|kept| !kept.file.id.starts_with("embed:"));
        stored.attachments.extend(moving_pictures);
    }
    if let Some(reacted) = &update.reactions {
        stored.reactions = reactions(&mut rich, reacted);
    }
    if let Some(pinned) = update.pinned {
        stored.pinned = pinned;
    }
    if update.edited_timestamp.is_some() {
        stored.edited = true;
    }
    rich.files
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nobody;

    impl Names for Nobody {
        fn user(&self, id: &str) -> Option<String> {
            (id == "7").then(|| "Priya".to_owned())
        }
        fn role(&self, _: &str) -> Option<String> {
            Some("mods".to_owned())
        }
        fn channel(&self, _: &str) -> Option<String> {
            Some("deploys".to_owned())
        }
        fn space(&self, _: &str) -> Option<String> {
            Some("Moss".to_owned())
        }
    }

    fn parsed(text: &str) -> Rich {
        let mut rich = Rich::default();
        markdown(&mut rich, text, &Nobody);
        rich
    }

    fn styled<'a>(rich: &'a Rich, style: &Style) -> Vec<&'a str> {
        let matching = rich.spans.iter().filter(|span| span.style == *style);
        matching.map(|span| &rich.text[span.start as usize..span.end as usize]).collect()
    }

    #[test]
    fn inline_styles_leave_no_markup_and_cover_their_words() {
        let rich = parsed("a **bold _both_** ~~gone~~ __under__ ||secret|| `x*y` snake_case_name 2 * 3 * 4");
        assert_eq!(rich.text, "a bold both gone under secret x*y snake_case_name 2 * 3 * 4");
        assert_eq!(styled(&rich, &Style::Bold), ["bold both"]);
        assert_eq!(styled(&rich, &Style::Italic), ["both"]);
        assert_eq!(styled(&rich, &Style::Strike), ["gone"]);
        assert_eq!(styled(&rich, &Style::Underline), ["under"]);
        assert_eq!(styled(&rich, &Style::Spoiler), ["secret"]);
        assert_eq!(styled(&rich, &Style::Code), ["x*y"]);
    }

    #[test]
    fn mentions_read_as_names_and_emoji_as_their_name() {
        let rich = parsed("<@7> <@!9> in <#5> cc <@&3> @everyone <:party:42> <a:spin:43>");
        assert_eq!(rich.text, "@Priya @someone in #deploys cc @mods @everyone :party: :spin:");
        let user = Style::Mention { kind: MentionKind::User, id: "7".into() };
        assert_eq!(styled(&rich, &user), ["@Priya"]);
        let everyone = Style::Mention { kind: MentionKind::Everyone, id: "everyone".into() };
        assert_eq!(styled(&rich, &everyone), ["@everyone"]);
        let emoji: Vec<&str> = rich.files.files.iter().map(|(file, _)| file.id.as_str()).collect();
        assert_eq!(emoji, ["emoji:42", "emoji:43"]);
    }

    #[test]
    fn links_bare_masked_and_bracketed() {
        let rich = parsed("see https://a.example/x?y=1. and [the docs](https://b.example) or <https://c.example>");
        assert_eq!(rich.text, "see https://a.example/x?y=1. and the docs or https://c.example");
        let link = |url: &str| Style::Link { url: url.into() };
        assert_eq!(styled(&rich, &link("https://a.example/x?y=1")), ["https://a.example/x?y=1"]);
        assert_eq!(styled(&rich, &link("https://b.example")), ["the docs"]);
        assert_eq!(styled(&rich, &link("https://c.example")), ["https://c.example"]);
    }

    #[test]
    fn blocks_code_quotes_and_lists() {
        let rich = parsed("before\n```rust\nlet a = *b;\n```\n> quoted **line**\n- one\n1. two\nénd");
        assert_eq!(rich.text, "before\nlet a = *b;\nquoted line\none\n1. two\nénd");
        assert_eq!(styled(&rich, &Style::CodeBlock { language: "rust".into() }), ["let a = *b;"]);
        assert_eq!(styled(&rich, &Style::Quote), ["quoted line"]);
        assert_eq!(styled(&rich, &Style::ListItem { ordered: false, depth: 0 }), ["one"]);
        assert_eq!(styled(&rich, &Style::ListItem { ordered: true, depth: 0 }), ["1. two"]);
        for span in &rich.spans {
            assert!(rich.text.is_char_boundary(span.start as usize) && rich.text.is_char_boundary(span.end as usize));
        }
    }

    #[test]
    fn a_picture_is_registered_at_the_size_it_is_drawn_and_at_larger_ones_up_to_its_own() {
        // Wide, tall, and already small enough.
        assert_eq!(fitted(1600, 900), (400, 225));
        assert_eq!(fitted(900, 1600), (168, 300));
        assert_eq!(fitted(320, 200), (320, 200));
        let sides = |sizes: Vec<(u32, String)>| sizes.into_iter().map(|(side, _)| side).collect::<Vec<_>>();
        let proxy = "https://media.discordapp.net/attachments/1/2/a.png?ex=1";
        // A 2x screen showing the 400 pt picture asks for 800: there is a size that large.
        assert_eq!(sides(larger(proxy, (400, 225), (1600, 900), false)), [600, 800, 1200]);
        // Never a size the picture does not have: its own is the last one.
        let small = larger(proxy, (400, 225), (700, 394), false);
        assert_eq!(sides(small.clone()), [600, 700]);
        assert!(small[1].1.ends_with("&format=webp&width=700&height=394"), "{}", small[1].1);
        assert_eq!(sides(larger(proxy, (320, 200), (320, 200), false)), Vec::<u32>::new());
        // A GIF is asked for moving at every size, never as its whole original.
        let gif = larger("https://media.discordapp.net/attachments/1/2/hop.gif?ex=1", (400, 225), (800, 450), true);
        assert!(gif.iter().all(|(_, url)| url.ends_with("&animated=true") && url.contains("format=webp")), "{gif:?}");
        assert!(may_move("https://images-ext-1.discordapp.net/external/x/https/example.org/a.GIF?width=1") && !may_move("https://x/a.png"));
    }

    #[test]
    fn unclosed_markup_stays_as_typed() {
        assert_eq!(parsed("2 ** 3 and a_b and `open").text, "2 ** 3 and a_b and `open");
        assert_eq!(parsed("\\*not italic\\*").text, "*not italic*");
    }
}
