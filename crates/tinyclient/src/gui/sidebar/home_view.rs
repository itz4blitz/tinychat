//! Reading and drawing the network homes and the lists they drill into.

use super::chat::Look;
use super::home::{self, Act, Drill, Item};
use super::{on_screen, respliced, Mode};
use crate::client::ask;
use crate::gui::components::{badge_dot, empty_state, glyph, list_row, pair, rollup_badge, section_header, Avatar, Shape};
use crate::gui::state::{Shell, Snapshot};
use crate::gui::theme::{self, Type};
use crate::view::now;
use gpui::{div, list, prelude::*, px, AnyElement, Context, Div, MouseButton, Stateful, Window};
use protocol::{Conversation, ConversationKind, Folder, Key, Network, Request, Response, Shelf, PAGE_LIMIT};
use std::rc::Rc;

/// The most rows a home holds; chatd's order puts the ones that matter first.
const HOME_HELD: u32 = 500;

/// What one read of a home brought back.
#[derive(Default)]
struct Read {
    chats: Vec<Conversation>,
    requests: u32,
    in_voice: Vec<Conversation>,
    folders: Vec<Folder>,
    around: Option<(usize, u32)>,
    archive: Vec<Conversation>,
    archive_total: u32,
    drilled: Vec<Conversation>,
}

fn conversations(request: &Request) -> std::io::Result<(Vec<Conversation>, u32)> {
    match ask(request)? {
        Response::Conversations { conversations, total, .. } => Ok((conversations, total)),
        _ => Ok((Vec::new(), 0)),
    }
}

/// A whole home list, page by page, up to [`HOME_HELD`].
fn whole(network: Network, folder: Option<String>, shelf: Shelf) -> std::io::Result<(Vec<Conversation>, u32)> {
    let mut all = Vec::new();
    loop {
        let request = Request::Home { network, account: None, folder: folder.clone(), shelf, offset: all.len() as u32, limit: PAGE_LIMIT };
        let (page, total) = conversations(&request)?;
        let last = page.is_empty();
        all.extend(page);
        if last || all.len() as u32 >= total.min(HOME_HELD) {
            return Ok((all, total));
        }
    }
}

impl Shell {
    /// Reads what the home on show draws. Asking is also what makes chatd bring it up to date.
    pub(in crate::gui) fn load_home(&mut self, cx: &mut Context<Self>) {
        let network = match &self.sidebar_state.mode {
            Mode::Network(network) => *network,
            Mode::Space(space) if space.network == Network::Discord => Network::Discord,
            _ => return,
        };
        // Keys only: the read runs off this thread.
        let drill = self.sidebar_state.drill.as_ref().map(|drill| match drill {
            Drill::Threads(space) => (Shelf::Main, Some(space.clone()), None),
            Drill::Archive => (Shelf::Archive, None, None),
            Drill::Requests => (Shelf::Requests, None, None),
            Drill::Topics(forum) => (Shelf::Main, None, Some(forum.key.clone())),
        });
        let folder = self.sidebar_state.home.folder.clone();
        self.sidebar_state.home.epoch += 1;
        let epoch = self.sidebar_state.home.epoch;
        let read = move || -> std::io::Result<Read> {
            let mut read = Read::default();
            match drill {
                Some((_, Some(space), _)) => read.drilled = conversations(&Request::Threads { space, offset: 0, limit: PAGE_LIMIT })?.0,
                Some((_, _, Some(forum))) => read.drilled = conversations(&Request::Topics { forum, offset: 0, limit: PAGE_LIMIT })?.0,
                Some((shelf, None, None)) => read.drilled = whole(network, None, shelf)?.0,
                None => {}
            }
            match network {
                Network::Discord => {
                    read.chats = whole(network, None, Shelf::Main)?.0;
                    let requests = Request::Home { network, account: None, folder: None, shelf: Shelf::Requests, offset: 0, limit: 1 };
                    read.requests = conversations(&requests)?.1;
                    read.in_voice = conversations(&Request::InVoice { network: Some(network) })?.0;
                    if let Response::Folders { folders } = ask(&Request::Folders { network })? {
                        read.folders = folders;
                    }
                    if let Response::Around { requests, online, in_call, .. } = ask(&Request::Around { network: Some(network) })? {
                        read.around = Some((online.len() + in_call.len(), requests));
                    }
                }
                Network::Telegram => {
                    read.chats = whole(network, folder.clone(), Shelf::Main)?.0;
                    let archive = Request::Home { network, account: None, folder: None, shelf: Shelf::Archive, offset: 0, limit: 3 };
                    (read.archive, read.archive_total) = conversations(&archive)?;
                    if let Response::Folders { folders } = ask(&Request::Folders { network })? {
                        read.folders = folders;
                    }
                }
                Network::Teams | Network::Signal | Network::Messages => read.chats = whole(network, None, Shelf::Main)?.0,
                Network::Slack => {}
            }
            Ok(read)
        };
        self.off_thread(cx, read, move |this, answer, cx| {
            let home = &mut this.sidebar_state.home;
            let (Ok(read), true) = (answer, home.epoch == epoch && home.network == Some(network)) else {
                return;
            };
            let rows = |rows: Vec<Conversation>| rows.into_iter().filter(|c| !c.hidden).map(Rc::new).collect::<Vec<_>>();
            home.chats = rows(read.chats);
            home.requests = read.requests;
            home.in_voice = rows(read.in_voice);
            home.folders = read.folders;
            if let Some((online, requests)) = read.around {
                (home.online, home.friend_requests) = (online, requests);
            }
            home.archive = rows(read.archive);
            home.archive_total = read.archive_total;
            home.drilled = rows(read.drilled);
            home.loaded = true;
            home.loaded_once = true;
            this.rebuild_home(cx);
        });
    }

    /// Counts the friend requests waiting, for People's dot on the rail.
    pub(in crate::gui) fn load_requests(&mut self, cx: &mut Context<Self>) {
        self.off_thread(cx, || ask(&Request::Around { network: None }), |this, answer, cx| {
            if let Ok(Response::Around { requests, .. }) = answer {
                if this.sidebar_state.friend_requests != requests {
                    this.sidebar_state.friend_requests = requests;
                    cx.notify();
                }
            }
        });
    }

    /// Goes into a list of the home: Threads, Archive, Message requests, a forum's topics.
    pub(super) fn drill(&mut self, drill: Drill, cx: &mut Context<Self>) {
        let state = &mut self.sidebar_state;
        state.drill = Some(drill);
        state.cursor = None;
        state.home.drilled.clear();
        state.home.loaded = false;
        self.rebuild_rows(cx);
        self.load_home(cx);
    }

    /// The Slack workspace's Threads row: followed threads with new replies, from its tree.
    pub(super) fn threads_row(&self, space: &Key) -> Option<super::tree::Row> {
        let tree = self.trees.get(space)?;
        let news: Vec<&Rc<Conversation>> = tree.conversations.iter().filter(|c| c.kind == ConversationKind::Thread && c.followed && c.unread > 0).collect();
        Some(super::tree::Row::Threads { space: space.clone(), count: news.len() as u32, mention: news.iter().any(|c| c.mentions > 0) })
    }

    /// The home's items again, from what is held.
    pub(super) fn rebuild_home(&mut self, cx: &mut Context<Self>) {
        let state = &self.sidebar_state;
        let items: Vec<Item> = match (&state.mode, &state.drill) {
            (Mode::Network(_), Some(_)) => state.home.drilled.iter().cloned().map(Item::Chat).collect(),
            (Mode::Network(Network::Discord), None) | (Mode::Space(Key { network: Network::Discord, .. }), None) => {
                let spaces: Vec<protocol::Space> = self.spaces.iter().filter(|s| s.key.network == Network::Discord).cloned().collect();
                let open_space = match &state.mode {
                    Mode::Space(space) => Some(space.id.clone()),
                    _ => self.snapshot.as_ref().and_then(|s| s.open.as_ref()).and_then(|open| open.space.clone()),
                };
                let items = home::discord(&state.home, &spaces, open_space.as_deref(), state.home_unread);
                // The servers' indexes are into the shell's whole list of Spaces.
                items
                    .into_iter()
                    .map(|item| match item {
                        Item::Space { index, child, open } => {
                            let at = self.spaces.iter().position(|s| s.key == spaces[index].key).unwrap_or(index);
                            Item::Space { index: at, child, open }
                        }
                        other => other,
                    })
                    .collect()
            }
            (Mode::Network(Network::Telegram), None) => home::telegram(&state.home),
            (Mode::Network(Network::Signal | Network::Messages), None) => home::signal(&state.home),
            (Mode::Network(Network::Teams), None) => {
                let channels = |team: &Key| -> Vec<Rc<Conversation>> {
                    self.trees.get(team).map(|tree| tree.conversations.iter().filter(|c| c.kind != ConversationKind::Thread).cloned().collect()).unwrap_or_default()
                };
                home::teams(&state.home, &self.spaces, &channels, self.open.as_ref(), now())
            }
            _ => Vec::new(),
        };
        let news = if self.sidebar_state.mode.network() == Some(Network::Discord) && self.sidebar_state.drill.is_none() {
            let state = &self.sidebar_state;
            let spaces: Vec<protocol::Space> = self.spaces.iter().filter(|s| s.key.network == Network::Discord).cloned().collect();
            home::discord(&state.home, &spaces, None, true).iter().filter(|item| matches!(item, Item::Chat(_) | Item::Space { .. } | Item::Folder { .. })).count() as u32
        } else {
            0
        };
        let state = &mut self.sidebar_state;
        state.home_news = news;
        if *state.home_items != items {
            respliced(&state.home_list, &state.home_items, &items, Item::anchor);
            state.home_items = Rc::new(items);
            cx.notify();
        }
    }

    pub(super) fn activate_home(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(item) = self.sidebar_state.home_items.get(ix).cloned() else {
            return;
        };
        let home = &mut self.sidebar_state.home;
        match item {
            Item::Chat(c) | Item::Voice(c) | Item::Channel(c) => return self.pick_row(&c, cx),
            Item::Header { .. } => return,
            Item::Action { act, .. } => match act {
                Act::Friends => {
                    self.go(Mode::People, cx);
                    self.people_scope(Some(Network::Discord), cx);
                    return;
                }
                Act::Requests => {
                    if matches!(self.sidebar_state.mode, Mode::Space(_)) {
                        self.go(Mode::Network(Network::Discord), cx);
                    }
                    return self.drill(Drill::Requests, cx);
                },
                Act::Archive => return self.drill(Drill::Archive, cx),
                Act::MoreDms => home.dms_all = true,
                Act::MoreVoice => home.voice_all = true,
                Act::MoreChats => home.chats_all = true,
                Act::Meetings => home.meetings_open = !home.meetings_open,
            },
            Item::Folder { id, .. } => {
                if !home.opened.remove(&id) {
                    home.opened.insert(id);
                }
            }
            Item::Space { index, open: Some(_), .. } => {
                // A team opens in place; its channels are read as it opens.
                if let Some(team) = self.spaces.get(index) {
                    let id = team.key.id.clone();
                    if !home.opened.remove(&id) {
                        home.opened.insert(id);
                    }
                }
                self.show_mode(cx);
                return;
            }
            Item::Space { index, .. } => {
                if let Some(space) = self.spaces.get(index) {
                    self.go(Mode::Space(space.key.clone()), cx);
                }
                return;
            }
        }
        self.rebuild_home(cx);
    }

    fn home_item(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(item) = self.sidebar_state.home_items.get(ix).cloned() else {
            return div().into_any_element();
        };
        let focused = self.sidebar_state.listed() && self.sidebar_state.cursor == Some(ix) && (self.sidebar_state.focus.is_focused(window) || self.focus.is_focused(window));
        let network = self.sidebar_state.home.network;
        let click = move |row: Stateful<Div>, cx: &mut Context<Self>| row.on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.activate_home(ix, cx)));
        match item {
            Item::Header { label, first } => section_header(label, first).into_any_element(),
            Item::Chat(c) => {
                let look = match network {
                    Some(Network::Discord) if self.sidebar_state.drill.is_none() => Look { status: true, ..Look::default() },
                    Some(Network::Telegram | Network::Teams | Network::Signal | Network::Messages) => Look { time: true, ticks: true, ..Look::default() },
                    _ => Look { time: true, context: false, ..Look::default() },
                };
                self.chat_row("home", &c, look, focused, cx).into_any_element()
            }
            Item::Voice(c) => self.voice_row(&c, focused, cx).into_any_element(),
            Item::Action { act, icon, words, after, dot } => {
                // Folds and drills keep their words quiet; a number or dot says what is inside.
                let lit = act == Act::Friends && self.sidebar_state.home.friend_requests > 0;
                let trailing = match (&act, dot) {
                    (Act::Friends, _) if self.sidebar_state.home.friend_requests > 0 => {
                        Some(div().flex().items_center().gap(px(8.)).child(faint_words(after.clone())).child(crate::gui::components::badge_count_unread(self.sidebar_state.home.friend_requests)))
                    }
                    (_, Some(mention)) => Some(div().flex().items_center().gap(px(8.)).child(faint_words(after.clone())).child(badge_dot(8., if mention { theme::mention() } else { theme::unread() }))),
                    _ if after.is_empty() => None,
                    _ => Some(faint_words(after.clone())),
                };
                let chevron = matches!(act, Act::Archive | Act::Requests).then(|| glyph("icons/chevron-right.svg", 12., theme::text_faint()));
                let trailing = match (trailing, chevron) {
                    (Some(trailing), Some(chevron)) => Some(div().flex().items_center().gap(px(6.)).child(trailing).child(chevron)),
                    (None, chevron) => chevron.map(|chevron| div().child(chevron)),
                    (trailing, None) => trailing,
                };
                click(self.action_row(("home-action", ix), Some(icon), words.into(), lit, trailing, focused), cx).into_any_element()
            }
            Item::Space { index, child, open } => click(self.space_row(ix, index, child, open, focused), cx).into_any_element(),
            Item::Folder { name, color, open, lit, mention, .. } => {
                let color = color.map_or(theme::text_faint(), gpui::rgb);
                let chevron = if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" };
                let content = div()
                    .gap(px(8.))
                    .child(div().size(px(20.)).flex_none().flex().items_center().child(div().w(px(3.)).h(px(16.)).rounded(px(1.5)).bg(color)))
                    .child(div().min_w_0().truncate().type_ui().font_weight(if lit { theme::SEMIBOLD } else { theme::REGULAR }).text_color(if lit { theme::text() } else { theme::text_soft() }).child(name))
                    .child(glyph(chevron, 12., theme::text_faint()))
                    .child(div().flex_1())
                    .children(lit.then(|| badge_dot(8., if mention { theme::mention() } else { theme::unread() })));
                click(list_row(("folder", ix), theme::row_height(theme::ROW_ONE), false, focused, content), cx).into_any_element()
            }
            Item::Channel(c) => self.place_row(&c, 1, focused, cx).into_any_element(),
        }
    }

    /// A server or a team, 28: its icon, its name, a dot for what is inside. A team opens in
    /// place with a chevron; a server opens its tree.
    fn space_row(&self, ix: usize, index: usize, child: Option<u32>, open: Option<bool>, focused: bool) -> Stateful<Div> {
        let Some(space) = self.spaces.get(index) else {
            return list_row(("space-gone", ix), theme::row_height(theme::ROW_ONE), false, false, div());
        };
        let lit = space.attention > 0 || space.unread;
        let here = if let Mode::Space(selected) = &self.sidebar_state.mode { selected == &space.key } else { self.open.as_ref().is_some_and(|open| self.snapshot.as_ref().and_then(|s| s.open.as_ref()).is_some_and(|row| row.key == *open && row.space.as_deref() == Some(space.key.id.as_str()))) };
        let icon = self.media_path(&space.key, space.icon.as_ref());
        let color = if space.muted { theme::text_faint() } else if lit { theme::text() } else { theme::text_soft() };
        // A team's chevron sits in the tree's 16 slot, so its name lines up with its channels'.
        let chevron = open.map(|open| {
            div().w(px(16.)).mr(px(-8.)).flex_none().child(glyph(if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" }, 12., theme::text_faint()))
        });
        let content = div()
            .gap(px(8.))
            .children(child.map(|_| div().w(px(12.)).flex_none()))
            .children(chevron)
            .child(Avatar::new(&space.key, &space.name, 20.).shape(Shape::Place).image_at(icon).build())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .type_ui()
                    .font_weight(if lit && !space.muted { theme::SEMIBOLD } else if open.is_some() { theme::MEDIUM } else { theme::REGULAR })
                    .text_color(color)
                    .child(space.name.clone()),
            )
            .children(rollup_badge(space.attention, space.unread && !space.muted).filter(|_| open != Some(true)));
        let spine = child.map(|color| {
            let color = if color == 0 { theme::text_faint() } else { gpui::rgb(color) };
            div().absolute().left(px(16.)).top_0().bottom_0().w(px(3.)).bg(color).opacity(0.7)
        });
        list_row(super::row_id("space", &space.key), theme::row_height(theme::ROW_ONE), here, focused, content).relative().children(spine)
    }

    /// Friends in a voice channel or a DM call: their faces, where, and who.
    fn voice_row(&self, c: &Rc<Conversation>, focused: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let selected = self.open.as_ref() == Some(&c.key);
        let surface = theme::row_surface(selected);
        let people: Vec<&str> = c.occupants.as_ref().map(|inside| inside.people.iter().map(|p| p.name.as_str()).collect()).unwrap_or_default();
        let total = c.occupants.as_ref().map_or(0, |inside| inside.total as usize);
        let faces = match people.as_slice() {
            [] => self.conversation_avatar(c, 32., surface, cx),
            [one] => Avatar::new(&c.key, one, 32.).build(),
            [one, two, ..] => pair(&c.key, &format!("{one}, {two}"), 32., surface),
        };
        let firsts: Vec<&str> = people.iter().take(3).map(|name| name.split_whitespace().next().unwrap_or(name)).collect();
        let who = match total.saturating_sub(firsts.len()) {
            0 => firsts.join(", "),
            more => format!("{} +{more}", firsts.join(", ")),
        };
        let (title, glyph_name) = if c.kind.is_direct() { ("Call in your DMs".to_owned(), "icons/phone.svg") } else { (c.title.trim_start_matches('#').to_owned(), "icons/volume-2.svg") };
        let content = div()
            .gap(px(10.))
            .child(faces)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(18.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(glyph(glyph_name, 14., theme::accent_text()))
                            .child(div().flex_none().max_w(px(150.)).truncate().type_ui().text_color(theme::text()).child(title))
                            .children((!c.space_title.is_empty()).then(|| div().flex_1().min_w_0().truncate().type_ui().text_color(theme::text_faint()).child(c.space_title.clone()))),
                    )
                    .child(div().h(px(16.)).truncate().type_small().text_color(theme::text_soft()).child(who)),
            );
        let picked = c.clone();
        list_row(super::row_id("voice", &c.key), theme::row_height(theme::ROW_TWO), selected, focused, content)
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| this.pick_row(&picked, cx)))
    }

    /// Telegram's folder tabs under the jump strip: All, then each folder with its count.
    pub(super) fn folder_tabs(&self, cx: &mut Context<Self>) -> Option<Div> {
        let state = &self.sidebar_state;
        if state.mode != Mode::Network(Network::Telegram) || state.drill.is_some() || state.home.folders.is_empty() {
            return None;
        }
        let chosen = state.home.folder.clone();
        let tab = |id: Option<String>, name: String, unread: u32, mentions: u32, cx: &mut Context<Self>| {
            let on = chosen == id;
            let count = match (mentions, unread) {
                (0, 0) => None,
                (0, unread) => Some(crate::gui::components::badge_count(unread)),
                (mentions, _) => Some(crate::gui::components::badge_mention(mentions)),
            };
            let key = id.clone().unwrap_or_default();
            div()
                .id(gpui::SharedString::from(format!("tab-{key}")))
                .flex_none()
                .h(px(32.))
                .px(px(8.))
                .relative()
                .flex()
                .items_center()
                .gap(px(5.))
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                    this.sidebar_state.home.folder = id.clone();
                    this.sidebar_state.cursor = None;
                    this.load_home(cx);
                }))
                .child(div().type_ui().font_weight(theme::MEDIUM).text_color(if on { theme::text() } else { theme::text_soft() }).child(name))
                .children(count)
                .children(on.then(|| div().absolute().bottom_0().left(px(8.)).right(px(8.)).h(px(2.)).rounded(px(1.)).bg(theme::text())))
        };
        let mut tabs = div().id("folder-tabs").flex_none().h(px(32.)).px(px(8.)).mb(px(4.)).flex().items_center().gap(px(4.)).overflow_x_scroll();
        tabs = tabs.child(tab(None, "All".into(), 0, 0, cx));
        let folders = state.home.folders.clone();
        for folder in folders {
            tabs = tabs.child(tab(Some(folder.key.id.clone()), folder.name.clone(), folder.unread, folder.mentions, cx));
        }
        Some(div().child(tabs))
    }

    fn home_empty(&self) -> Div {
        let state = &self.sidebar_state;
        let (icon, title, detail): (&'static str, String, String) = match (&state.home.network, &state.drill) {
            (_, Some(Drill::Threads(_))) => ("icons/messages-square.svg", "No new replies".into(), "Threads you follow show here when someone replies.".into()),
            (_, Some(Drill::Archive)) => ("icons/inbox.svg", "Nothing archived".into(), "Chats you archive in Telegram show up here.".into()),
            (_, Some(Drill::Requests)) => ("icons/mail.svg", "No message requests".into(), "Messages from people you don't know wait here.".into()),
            (_, Some(Drill::Topics(_))) => ("icons/messages-square.svg", "No topics yet".into(), "Topics of this group show up here.".into()),
            (Some(Network::Discord), None) if state.home_unread => ("icons/check.svg", "Nothing unread".into(), String::new()),
            (Some(Network::Discord), None) => ("icons/hash.svg", "No servers yet".into(), "Servers you join in Discord show up here.".into()),
            (Some(Network::Telegram), None) => match state.home.folder.as_ref().and_then(|id| state.home.folders.iter().find(|f| &f.key.id == id)) {
                Some(folder) => ("icons/list.svg", format!("Nothing in {}", folder.name), "Chats you add to this folder in Telegram show up here.".into()),
                None => ("icons/message-circle.svg", "No chats yet".into(), "Type a @username to find someone.".into()),
            },
            (Some(Network::Teams), None) => ("icons/message-circle.svg", "No chats yet".into(), "Type a name to start one.".into()),
            // A newly linked device is sent no history: chats appear as messages arrive.
            (Some(Network::Signal), None) => ("icons/message-circle.svg", "No chats yet".into(), "New messages show up here as they arrive.".into()),
            (Some(Network::Messages), None) => ("icons/message-circle.svg", "No chats yet".into(), "Texts show up here while your phone is online.".into()),
            _ => ("icons/message-circle.svg", "Nothing here yet".into(), String::new()),
        };
        empty_state(icon, &title, &detail).px(px(24.))
    }

    pub(super) fn home_list(&self, snapshot: &Snapshot, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let _ = (snapshot, window);
        let state = &self.sidebar_state;
        if state.home_items.is_empty() {
            if !state.home.loaded {
                let network = state.home.network.map_or("", Network::label);
                let words = if state.drill.is_some() { "Getting the list…".to_owned() } else { format!("Getting your {network} chats…") };
                return div().px(px(theme::PANE_PAD)).pt(px(8.)).type_small().text_color(theme::text_faint()).child(words).into_any_element();
            }
            return self.home_empty().into_any_element();
        }
        let items = state.home_items.clone();
        let visible = on_screen(&state.home_list, items.len(), |ix| items[ix].height());
        self.watch_rows(items[visible].iter().filter_map(Item::conversation), cx);
        list(state.home_list.clone(), cx.processor(|this: &mut Shell, ix: usize, window, cx| this.home_item(ix, window, cx))).size_full().into_any_element()
    }
}

fn faint_words(words: String) -> Div {
    div().flex_none().max_w(px(140.)).truncate().type_small().text_color(theme::text_faint()).child(words)
}
