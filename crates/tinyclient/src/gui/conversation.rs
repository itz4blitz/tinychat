//! The conversation pane: its strip, the messages, and what shows when nothing is open.
//!
//! The messages are a view of their own ([`Pane`]), so scrolling them redraws nothing else. It
//! keeps a bounded run of the conversation (`pages`), lays it out as rows and dividers (`rows`),
//! and draws them (`draw`, with `rich` and `text` for the words and `media` and `images` for
//! the pictures). A thread is a second, dense `Pane` in the right pane.

mod actions;
mod draw;
mod emoji;
mod header;
mod icons;
pub(in crate::gui) mod images;
mod media;
mod pages;
mod player;
mod rich;
mod rows;
#[cfg(feature = "gui-shot")]
mod shot;
mod side;
mod text;
mod tokens;
mod video;
mod viewer;
mod when;

pub(in crate::gui) use player::glyph as player_glyph;


use super::components::empty_state;
use super::layout::{drags_window, TOP_BAR};
use super::state::{Shell, Snapshot};
use super::theme::{self, Type};
use crate::client::ask;
use gpui::{
    div, prelude::*, px, Bounds, ClipboardItem, Context, Div, Entity, EventEmitter,
    FocusHandle, KeyDownEvent, ListAlignment, ListOffset, ListState, MouseButton, Pixels,
    Subscription, WeakEntity, Window,
};
use images::Images;
use pages::PAGE;
use protocol::{
    Change, Conversation, ConversationKind, Key, MediaFile, Message, Page, Request, Response, SendState, Special,
    PAGE_LIMIT,
};
use rows::Item;
use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokens::Metrics;

/// The thread pane and the viewer are reached from here; `Shell` only holds this.
#[derive(Default)]
pub struct State {
    pane: OnceCell<Entity<Pane>>,
    /// The thread in the right pane, while the pane's stack holds one.
    thread: RefCell<Option<Entity<Pane>>>,
    listening: OnceCell<Subscription>,
    following: OnceCell<Subscription>,
    side: side::Side,
}

/// How often the network is told how far the user has read, at most.
const READ_EVERY: Duration = Duration::from_secs(1);
/// How far from the bottom the jump button appears.
const AWAY: f32 = 200.;
/// Rows this far outside the viewport are built, so their pictures are asked for before they
/// show and older history is asked for before the top is reached.
const AHEAD: f32 = 600.;

/// Why a page was asked for, which decides what happens to the scroll position when it lands.
#[derive(Clone, PartialEq)]
enum Purpose {
    Open,
    /// Something changed while pinned to the bottom.
    Newest,
    /// Something changed while reading further up: what is on screen is read again.
    Around,
    /// The same, for what arrived at the end meanwhile.
    Arrivals,
    Older,
    Newer,
    /// A reply quote was clicked and its target was not held.
    Jump(String),
    /// The jump button, from a window that no longer reached the end.
    Bottom,
    /// Coming back this session to where the user left it: this message, this far into it.
    Restore(String, f32),
}

/// Where a conversation was left, for coming back to it in this session.
#[derive(Clone, Debug, PartialEq)]
struct Place {
    /// The first message on screen, and how far its top was above the viewport's.
    id: String,
    offset: f32,
    /// Left at the newest message: it opens there again, by the landing rule.
    bottom: bool,
}

struct Open {
    key: Key,
    title: String,
    kind: ConversationKind,
    /// My own id on this account.
    me: String,
    /// The conversation's picture: a direct one's is the other person's.
    picture: Option<MediaFile>,
    /// Where the user had read to on opening. The "New" line stays there until they leave.
    last_read: Option<String>,
    marker_known: bool,
    /// The message a thread hangs off, and how many replies it had.
    root: Option<(Rc<Message>, u32)>,
    /// What it takes as reactions, once chatd said. `None` until then, or from a chatd that
    /// does not say: everything is offered, as before there were rules.
    reactions: Option<protocol::ReactionRules>,
    #[cfg_attr(not(feature = "gui-shot"), allow(dead_code))]
    started: Instant,
}

#[derive(Default)]
struct ReadMark {
    /// The newest message the network has been told about.
    sent: Option<(i64, usize, String)>,
    at: Option<Instant>,
    /// Seen since, waiting for the second to pass.
    next: Option<String>,
    waiting: bool,
}

/// Selected text, by message and place in it rather than by anything drawn, so it outlives
/// the rows scrolling away and back.
struct Selection {
    message: String,
    /// Where the button went down.
    anchor: rich::Place,
    /// Where the drag has reached; `None` until it moves.
    head: Option<rich::Place>,
}

impl Selection {
    /// The bytes of `block` that are selected.
    fn within(&self, block: usize) -> Option<std::ops::Range<usize>> {
        let head = self.head?;
        let (from, to) = if self.anchor <= head { (self.anchor, head) } else { (head, self.anchor) };
        if block < from.0 || block > to.0 {
            return None;
        }
        let start = if block == from.0 { from.1 } else { 0 };
        let end = if block == to.0 { to.1 } else { usize::MAX };
        (start < end).then_some(start..end)
    }
}

/// The one rule for telling the network how far the user has read: a real window that is
/// active, showing the conversation, with the message on screen. A build that takes
/// screenshots never does, whatever it is showing: a picture of a conversation must not read it.
pub(super) fn may_mark_read(screenshots: bool, window_active: bool, open: bool, on_screen: bool) -> bool {
    !screenshots && window_active && open && on_screen
}

const SCREENSHOTS: bool = cfg!(feature = "gui-shot");

/// What to do with a file once it is on disk.
#[derive(Clone, Copy, PartialEq)]
enum Then {
    QuickLook,
    /// The system's own app for it: a video plays in the system's player.
    Open,
}

pub enum PaneEvent {
    /// The reply line under this message was clicked.
    OpenThread(Rc<Message>),
}

impl EventEmitter<PaneEvent> for Pane {}

/// Where each row drawn in a frame is.
type Seen = Rc<RefCell<Vec<(usize, Bounds<Pixels>)>>>;

/// The messages of one conversation, or of one thread.
pub struct Pane {
    metrics: Metrics,
    shell: WeakEntity<Shell>,
    open: Option<Open>,
    window: pages::Window,
    items: Vec<Item>,
    list: ListState,
    focus: FocusHandle,

    loading_older: bool,
    loading_newer: bool,
    /// Why older history did not come.
    older_failed: Option<String>,
    /// Refreshes on their way. A change that arrives meanwhile is read once they land.
    refreshing: u32,
    stale: bool,

    /// Scrolled more than [`AWAY`] from the bottom.
    away: bool,
    /// Messages that arrived while away.
    unseen: usize,
    read: ReadMark,
    active: bool,
    watching_window: Option<Subscription>,

    images: Images,
    /// The conversation changed; its pictures go at the next draw, which has the window.
    images_stale: bool,
    /// Files the rows being drawn need, and how many pixels their longest side must have.
    want: Vec<(String, u32)>,
    /// The largest size each file has been asked for at. Asked again only for more.
    asked: HashMap<String, u32>,
    /// Where each file was last seen on disk. A message read again while a larger size is on
    /// its way has no path; the smaller picture is drawn until the larger lands.
    known: HashMap<String, String>,
    /// Files whose bytes were replaced on disk; their decodes go at the next draw.
    replaced: Vec<String>,
    /// The viewer's decodes of originals that nothing shows any more, as `(path, edge)`; they
    /// go at the next draw.
    let_go: Vec<(String, u32)>,
    /// The window's scale when sizes were last asked for.
    scale: f32,
    /// Files the user asked to open, and the name each is opened under.
    then: HashMap<String, (Then, String)>,

    /// Spoilers opened, by message and span.
    revealed: HashSet<(String, u32)>,
    /// Spoilered pictures opened, by file.
    shown: HashSet<String>,
    /// Long code blocks opened, by message and block.
    expanded: HashSet<(String, usize)>,
    /// The code block just copied, which shows a check for a moment.
    copied: Option<(String, usize)>,
    /// The link under the pointer: message, block, run.
    hovered: Option<(String, usize, usize)>,
    selection: Option<Selection>,
    /// The button is down on a message's text.
    dragging: bool,
    /// The press now being handled landed on text, so it does not clear the selection.
    on_text: bool,
    /// The message a jump landed on, tinted while it fades.
    flash: Option<String>,
    viewer: Option<viewer::Viewer>,
    /// The message under the pointer, which shows the toolbar.
    hover: Option<String>,
    menu: Option<actions::Menu>,
    picker: Option<actions::Picking>,
    /// The message the delete dialog asks about.
    confirm: Option<Rc<Message>>,
    /// After an older page: the message that was on screen and how far below the top of the
    /// viewport it was, to put it back there once the rows above it can be measured.
    hold: Option<(String, f32)>,
    /// The first page put a "New" line in: it is placed a third of the way down once the rows
    /// can be measured, when what is unread does not fit.
    land: bool,
    /// Where each conversation was left this session.
    places: crate::gui::pane::Memory<Place>,
    /// The next open starts from the page around this message: more is unread than a page holds.
    around: Option<String>,
    /// A search hit asked for this message before its conversation opened; it wins over where
    /// reading stopped.
    landing: Option<String>,
    /// Nothing to type in: the messages take the focus on the next draw.
    wants_focus: bool,
    /// How far the files of each of my pending messages have gone, by `client_id`.
    uploads: HashMap<String, (u64, u64)>,
    /// When each pending message was first drawn, so its time and place hold until it is sent.
    pending_at: HashMap<String, i64>,

    /// A drag along a player, until it is let go.
    scrub: Option<player::Scrub>,
    /// Where each player's waveform or bar was last drawn, by file.
    scrub_bounds: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    /// A file the user pressed play on before it was on disk: file, message, attachment, from.
    play_after: Option<(String, String, usize, Option<u32>)>,

    /// How wide the list was when last drawn; 0 before the first draw.
    width: f32,
    /// Where each row drawn this frame is, for knowing what is on screen.
    seen: Seen,
    #[cfg(feature = "gui-shot")]
    script: shot::Script,
}

impl Pane {
    fn new(metrics: Metrics, shell: WeakEntity<Shell>, cx: &mut Context<Self>) -> Pane {
        // A thread reads down from its root; a conversation up from its newest message.
        let alignment = if metrics == Metrics::DENSE { ListAlignment::Top } else { ListAlignment::Bottom };
        Pane {
            metrics,
            shell,
            open: None,
            window: pages::Window::default(),
            items: Vec::new(),
            list: ListState::new(0, alignment, px(AHEAD)),
            focus: cx.focus_handle(),
            loading_older: false,
            loading_newer: false,
            older_failed: None,
            refreshing: 0,
            stale: false,
            away: false,
            unseen: 0,
            read: ReadMark::default(),
            active: false,
            watching_window: None,
            images: Images::default(),
            images_stale: false,
            want: Vec::new(),
            asked: HashMap::new(),
            known: HashMap::new(),
            replaced: Vec::new(),
            let_go: Vec::new(),
            scale: 0.,
            then: HashMap::new(),
            revealed: HashSet::new(),
            shown: HashSet::new(),
            expanded: HashSet::new(),
            copied: None,
            hovered: None,
            selection: None,
            dragging: false,
            on_text: false,
            flash: None,
            viewer: None,
            hover: None,
            menu: None,
            picker: None,
            confirm: None,
            hold: None,
            land: false,
            places: crate::gui::pane::Memory::default(),
            around: None,
            landing: None,
            wants_focus: false,
            uploads: HashMap::new(),
            pending_at: HashMap::new(),
            scrub: None,
            scrub_bounds: Rc::default(),
            play_after: None,
            width: 0.,
            seen: Rc::default(),
            #[cfg(feature = "gui-shot")]
            script: if metrics == Metrics::DENSE { shot::Script::default() } else { shot::Script::from_env() },
        }
    }

    fn is_thread(&self) -> bool {
        self.metrics == Metrics::DENSE
    }

    /// Shows this conversation. The same one again only takes its new title and picture.
    /// `typing`: it has a composer, which takes the focus; otherwise the messages do.
    fn show(&mut self, conversation: &Conversation, me: &str, picture: Option<MediaFile>, typing: bool, cx: &mut Context<Self>) {
        if let Some(open) = self.open.as_mut().filter(|open| open.key == conversation.key) {
            if open.title != conversation.title || open.picture != picture {
                open.title.clone_from(&conversation.title);
                open.picture = picture;
                cx.notify();
            }
            return;
        }
        // More unread than a page holds: the first page is the one around where reading stopped.
        // A search hit's landing wins over it.
        let around = conversation.last_read.clone().filter(|_| conversation.unread > PAGE / 2);
        if self.landing.is_none() {
            self.around = around;
        }
        self.start(conversation.key.clone(), &conversation.title, conversation.kind, me, None, cx);
        self.wants_focus = !typing;
        if let Some(open) = self.open.as_mut() {
            open.picture = picture;
        }
    }

    fn show_thread(&mut self, key: Key, title: &str, me: &str, root: Rc<Message>, cx: &mut Context<Self>) {
        let replies = root.thread.as_ref().map_or(0, |thread| thread.replies);
        self.start(key, title, ConversationKind::Thread, me, Some((root, replies)), cx);
    }

    fn start(
        &mut self,
        key: Key,
        title: &str,
        kind: ConversationKind,
        me: &str,
        root: Option<(Rc<Message>, u32)>,
        cx: &mut Context<Self>,
    ) {
        let around = self.landing.take().or(self.around.take());
        // Where the one being left was, for coming back to it.
        if let Some((left, place)) = self.key().cloned().zip(self.place()) {
            self.places.put(left, place);
        }
        let back = self.places.get(&key).filter(|place| !place.bottom).cloned();
        self.open = Some(Open {
            key,
            title: title.to_owned(),
            kind,
            me: me.to_owned(),
            picture: None,
            last_read: None,
            marker_known: false,
            root,
            reactions: None,
            started: Instant::now(),
        });
        self.window = pages::Window::default();
        self.items = Vec::new();
        self.list.reset(0);
        self.loading_older = false;
        self.loading_newer = false;
        self.older_failed = None;
        self.refreshing = 0;
        self.stale = false;
        self.away = false;
        self.unseen = 0;
        self.read = ReadMark::default();
        self.images_stale = true;
        self.want.clear();
        self.asked.clear();
        self.known.clear();
        self.replaced.clear();
        self.let_go.clear();
        self.then.clear();
        self.revealed.clear();
        self.shown.clear();
        self.expanded.clear();
        self.copied = None;
        self.hovered = None;
        self.selection = None;
        self.dragging = false;
        self.flash = None;
        self.viewer = None;
        self.hover = None;
        self.menu = None;
        self.picker = None;
        self.confirm = None;
        self.hold = None;
        self.land = false;
        self.uploads.clear();
        self.pending_at.clear();
        self.relay(true, ListOffset::default());
        self.ask_reactions(cx);
        // M2: a conversation opens at its newest message rather than the first unread.
        let around = around.filter(|_| !super::app::prefs::choices().newest);
        match back {
            Some(place) => self.load(Page::Around { id: place.id.clone() }, Purpose::Restore(place.id, place.offset), cx),
            None => match around {
                Some(id) => self.load(Page::Around { id }, Purpose::Open, cx),
                None => self.load(Page::Newest, Purpose::Open, cx),
            },
        }
        cx.notify();
    }

    /// Where the user is in this conversation now: the first message on screen and how far
    /// into it, as a place that stays true however the rows above it are measured later.
    fn place(&self) -> Option<Place> {
        if self.is_thread() || self.items.is_empty() {
            return None;
        }
        let bottom = !self.away;
        let top = self.list.logical_scroll_top();
        let (id, offset) = match self.items.get(top.item_ix).and_then(Item::message) {
            Some(message) => (message.id.clone(), f32::from(top.offset_in_item)),
            None => (self.items.get(top.item_ix..)?.iter().find_map(Item::message)?.id.clone(), 0.),
        };
        Some(Place { id, offset, bottom })
    }

    fn key(&self) -> Option<&Key> {
        self.open.as_ref().map(|open| &open.key)
    }

    /// The message with this id, wherever it is held.
    fn find(&self, id: &str) -> Option<Rc<Message>> {
        let root = self.open.as_ref().and_then(|open| open.root.as_ref()).map(|(root, _)| root);
        root.filter(|root| root.id == id).or_else(|| self.window.messages.iter().find(|m| m.id == id)).cloned()
    }

    // Pages.

    fn load(&mut self, page: Page, purpose: Purpose, cx: &mut Context<Self>) {
        let Some(conversation) = self.key().cloned() else {
            return;
        };
        let limit = match purpose {
            Purpose::Newest => (self.window.messages.len() as u32).clamp(PAGE, PAGE_LIMIT),
            Purpose::Around => PAGE * 2,
            _ => PAGE,
        };
        // The paging measurement has a second to cross all of history.
        #[cfg(feature = "gui-shot")]
        let limit = if purpose == Purpose::Older && self.script.paging() { PAGE_LIMIT } else { limit };
        match purpose {
            Purpose::Older | Purpose::Open | Purpose::Restore(..) => self.loading_older = true,
            Purpose::Newer => self.loading_newer = true,
            Purpose::Newest | Purpose::Around | Purpose::Arrivals => self.refreshing += 1,
            Purpose::Jump(_) | Purpose::Bottom => {}
        }
        let request = Request::Messages { conversation: conversation.clone(), page: page.clone(), limit };
        cx.spawn(async move |this, cx| {
            let answer = cx.background_executor().spawn(async move { ask(&request) }).await;
            let _ = this.update(cx, |this, cx| this.loaded(&conversation, &page, purpose, answer, cx));
        })
        .detach();
    }

    fn loaded(
        &mut self,
        conversation: &Key,
        page: &Page,
        purpose: Purpose,
        answer: io::Result<Response>,
        cx: &mut Context<Self>,
    ) {
        if self.key() != Some(conversation) {
            return;
        }
        match purpose {
            Purpose::Older | Purpose::Open | Purpose::Restore(..) => self.loading_older = false,
            Purpose::Newer => self.loading_newer = false,
            Purpose::Newest | Purpose::Around | Purpose::Arrivals => self.refreshing = self.refreshing.saturating_sub(1),
            Purpose::Jump(_) | Purpose::Bottom => {}
        }
        let (messages, last_read, older, newer) = match answer {
            Ok(Response::Messages { messages, last_read, older, newer, .. }) => (messages, last_read, older, newer),
            other => {
                if let Purpose::Restore(..) = purpose {
                    // The place is gone from the history: open as if for the first time.
                    self.load(Page::Newest, Purpose::Open, cx);
                    return;
                }
                if matches!(purpose, Purpose::Older | Purpose::Open) {
                    self.older_failed = Some(match other {
                        Ok(Response::Error { detail, .. }) => detail,
                        Ok(_) => crate::client::OTHER_ANSWER.to_owned(),
                        Err(err) => crate::client::trouble_words(&err),
                    });
                    if purpose == Purpose::Open {
                        // Nothing is known about this conversation, least of all that it is
                        // empty: the row that says so, with its Retry, stands in for the start.
                        self.window.older = true;
                        self.relay(true, ListOffset::default());
                    }
                    cx.notify();
                }
                return;
            }
        };
        if let Some(open) = self.open.as_mut().filter(|open| !open.marker_known) {
            open.last_read = last_read;
            open.marker_known = true;
            // What was read before opening is not told to the network again.
            let read = open.last_read.as_deref().and_then(|id| messages.iter().find(|message| message.id == id));
            self.read.sent = read.map(|message| (message.sent, message.id.len(), message.id.clone()));
        }
        self.older_failed = None;

        let pinned = self.pinned();
        let top = self.list.logical_scroll_top();
        let reading = self.items.get(top.item_ix..).and_then(|below| below.iter().find_map(Item::message)).cloned();
        let last = self.window.messages.last().cloned();
        if purpose == Purpose::Older && !pinned {
            self.hold = self.held_place();
        }
        self.window.merge(page, messages, older, newer);
        let keep = match purpose {
            Purpose::Older => 0,
            Purpose::Around | Purpose::Jump(_) => {
                let at = reading.as_ref().and_then(|message| self.window.position(&message.id));
                at.unwrap_or(self.window.messages.len() / 2)
            }
            Purpose::Restore(ref id, _) => self.window.position(id).unwrap_or(self.window.messages.len() / 2),
            _ => self.window.messages.len().saturating_sub(1),
        };
        self.window.trim(keep);

        // What joined the end since: mine sends the view to it, anyone else's is counted.
        let arrived = last.and_then(|last| self.window.position(&last.id)).map(|at| &self.window.messages[at + 1..]);
        let mine = arrived.is_some_and(|new| new.iter().any(|message| message.outgoing && message.state == SendState::Pending));
        if !pinned && !mine {
            self.unseen += arrived.map_or(0, |new| new.iter().filter(|message| !message.outgoing).count());
        }
        self.relay(pinned || mine, top);
        match &purpose {
            Purpose::Jump(id) => self.reveal(id, cx),
            Purpose::Bottom => self.snap_to_end(),
            Purpose::Restore(id, offset) => self.restore(id, *offset),
            // Unread that may not fit: placed once the rows can be measured.
            Purpose::Open => self.land = !super::app::prefs::choices().newest && self.items.iter().any(|item| matches!(item, Item::New)),
            _ if mine => self.snap_to_end(),
            _ => {}
        }
        #[cfg(feature = "gui-shot")]
        if purpose == Purpose::Open {
            shot::opened(self, cx);
        }
        if self.stale && self.refreshing == 0 {
            self.stale = false;
            self.refresh(cx);
        }
        cx.notify();
    }

    /// Something in this conversation changed: what is on screen is read again.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.open.is_none() {
            return;
        }
        if self.refreshing > 0 {
            self.stale = true;
            return;
        }
        let top = self.list.logical_scroll_top().item_ix;
        let reading = self.items.get(top..).and_then(|below| below.iter().find_map(Item::message));
        match reading.filter(|_| !self.pinned()) {
            Some(reading) if self.window.position(&reading.id).is_some() => {
                let around = Page::Around { id: reading.id.clone() };
                let end = self.window.messages.last().filter(|_| !self.window.newer).map(|last| last.id.clone());
                self.load(around, Purpose::Around, cx);
                if let Some(id) = end {
                    self.load(Page::After { id }, Purpose::Arrivals, cx);
                }
            }
            _ => self.load(Page::Newest, Purpose::Newest, cx),
        }
    }

    /// The view follows the newest message.
    fn pinned(&self) -> bool {
        if self.is_thread() {
            return self.list.is_scrolled_to_end() == Some(true);
        }
        self.list.logical_scroll_top().item_ix >= self.items.len()
    }

    /// Lays the held messages out again and tells the list which rows changed, keeping what the
    /// user is reading where it is.
    fn relay(&mut self, pinned: bool, top: ListOffset) {
        let Some(open) = &self.open else {
            return;
        };
        let messages = held_in_place(&self.window.messages, &mut self.pending_at);
        let new = rows::build(&messages, &rows::Build {
            last_read: open.last_read.as_deref(),
            older: self.window.older,
            root: open.root.clone(),
            same_day: super::time::same_day,
        });
        if let Some((range, count)) = rows::changed(&self.items, &new) {
            self.list.splice(range, count);
            if pinned {
                if self.is_thread() {
                    self.list.scroll_to_end();
                }
            } else if let Some((item_ix, offset)) = rows::anchor(&self.items, &new, top.item_ix, top.offset_in_item.into()) {
                self.list.scroll_to(ListOffset { item_ix, offset_in_item: px(offset) });
            }
        }
        self.items = new;
    }

    /// Scrolls by a distance. Following the newest message, the list counts from the end of
    /// its rows rather than the top of the viewport, so the first step up starts a viewport higher.
    fn scroll_by(&mut self, distance: Pixels) {
        let following = !self.is_thread() && self.pinned();
        let viewport = self.list.viewport_bounds().size.height;
        self.list.scroll_by(if following && distance < px(0.) { distance - viewport } else { distance });
    }

    /// Back where the user left it: the same message at the same depth.
    fn restore(&mut self, id: &str, offset: f32) {
        let Some(at) = self.items.iter().position(|item| item.message().is_some_and(|message| message.id == id)) else {
            return;
        };
        self.list.scroll_to(ListOffset { item_ix: at, offset_in_item: px(offset) });
        self.away = true;
    }

    /// Lands on the first unread (07 R3): when what is unread does not fit, the "New" line sits
    /// a third of the way down and the jump button counts what is below; otherwise it stays at
    /// the bottom with the line in view. Rows are measured, not guessed, so the line lands
    /// where it is said to.
    fn settle_land(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.land {
            return;
        }
        let viewport = self.list.viewport_bounds().size;
        let (height, width) = (f32::from(viewport.height), f32::from(viewport.width));
        if height <= 0. || width <= 0. {
            // Not laid out yet: the next draw does it.
            return;
        }
        self.land = false;
        let Some(new) = self.items.iter().position(|item| matches!(item, Item::New)) else {
            return;
        };
        let space = gpui::size(gpui::AvailableSpace::Definite(px(width)), gpui::AvailableSpace::MinContent);
        let tall = |this: &mut Self, ix: usize, window: &mut Window, cx: &mut Context<Self>| {
            f32::from(this.row(ix, window, cx).layout_as_root(space, window, cx).height)
        };
        let mut below = 0.;
        for ix in new..self.items.len() {
            below += tall(self, ix, window, cx);
            if below > height {
                break;
            }
        }
        let Some(at) = landing(below, height, new, |ix| tall(self, ix, window, cx)) else {
            return;
        };
        self.list.scroll_to(at);
        self.away = true;
        self.unseen = self.items[new..].iter().filter_map(Item::message).filter(|message| !message.outgoing).count();
        cx.notify();
    }

    fn snap_to_end(&mut self) {
        self.list.scroll_to(ListOffset { item_ix: self.items.len(), offset_in_item: px(0.) });
        self.unseen = 0;
        self.away = false;
    }

    /// The jump button: the "New" line when it is still below what is on screen, else the
    /// newest message.
    fn jump(&mut self, cx: &mut Context<Self>) {
        let new = self.items.iter().position(|item| matches!(item, Item::New));
        match new.filter(|ix| self.list.item_is_below_viewport(*ix) == Some(true)) {
            Some(ix) => {
                // One row of context above the line.
                self.list.scroll_to(ListOffset { item_ix: ix.saturating_sub(1), offset_in_item: px(0.) });
                cx.notify();
            }
            None => self.go_to_bottom(cx),
        }
    }

    /// The newest message, read again if the window had left it behind.
    fn go_to_bottom(&mut self, cx: &mut Context<Self>) {
        if self.window.newer {
            self.load(Page::Newest, Purpose::Bottom, cx);
        } else {
            self.snap_to_end();
        }
        cx.notify();
    }

    /// Scrolls to a message, reading the history around it first if it is not held.
    pub(in crate::gui) fn jump_to(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.window.position(id).is_some() {
            self.reveal(id, cx);
        } else {
            self.load(Page::Around { id: id.to_owned() }, Purpose::Jump(id.to_owned()), cx);
        }
    }

    /// The next open starts on this message: a search hit opening its conversation.
    pub(in crate::gui) fn land_next(&mut self, id: String) {
        self.landing = Some(id);
    }

    fn reveal(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(at) = self.items.iter().position(|item| item.message().is_some_and(|message| message.id == id)) else {
            return;
        };
        // One row of context above, so the target is not flush against the strip.
        self.list.scroll_to(ListOffset { item_ix: at.saturating_sub(1), offset_in_item: px(0.) });
        self.flash = Some(id.to_owned());
        let id = id.to_owned();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(draw::FLASH + Duration::from_millis(100)).await;
            let _ = this.update(cx, |this, cx| {
                if this.flash.as_deref() == Some(&id) {
                    this.flash = None;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    // Changes.

    fn changed(&mut self, change: &Change, cx: &mut Context<Self>) {
        let Some(key) = self.key() else {
            return;
        };
        match change {
            Change::Messages { conversation } if conversation == key => self.refresh(cx),
            Change::Upload { conversation, client_id, sent, total } if conversation == key => {
                self.uploads.insert(client_id.clone(), (*sent, *total));
                cx.notify();
            }
            Change::Media { account, file } if *account == key.account() => self.landed(file, true, cx),
            _ => {}
        }
    }

    /// chatd says where a file is, or why it is not. `fresh` when it has only now been written:
    /// a larger size lands at the same path, so what was decoded from the old bytes is stale.
    fn landed(&mut self, file: &MediaFile, fresh: bool, cx: &mut Context<Self>) {
        if let Some(path) = &file.path {
            if fresh && self.known.get(&file.id) == Some(path) {
                self.replaced.push(path.clone());
            }
            self.known.insert(file.id.clone(), path.clone());
            if let Some((then, name)) = self.then.remove(&file.id) {
                draw::open_file(path, &name, then);
            }
            self.play_landed(&file.id, cx);
        } else if file.failed.is_some() {
            self.then.remove(&file.id);
            if self.play_after.as_ref().is_some_and(|(wanted, ..)| *wanted == file.id) {
                self.play_after = None;
            }
        }
        let mut changed = self.window.landed(file);
        if let Some((root, _)) = self.open.as_mut().and_then(|open| open.root.as_mut()) {
            if root.files().any(|held| held.id == file.id && pages::differs(held, file)) {
                for held in Rc::make_mut(root).files_mut().filter(|held| held.id == file.id) {
                    pages::take(held, file);
                }
                changed = true;
            }
        }
        if changed {
            let pinned = self.pinned();
            self.relay(pinned, self.list.logical_scroll_top());
        }
        cx.notify();
    }

    /// Notes that a row on screen shows `file` with its longest side `size` device pixels. A
    /// file that failed is never asked for here: only the user's Retry does that.
    fn want(&mut self, file: &MediaFile, size: u32) {
        if file.failed.is_none() {
            self.want.push((file.id.clone(), size));
        }
    }

    /// Asks chatd for the files the rows on screen need, at the sizes they are drawn. Each is
    /// asked for once, and again only when a box grows or the window moves to a sharper screen.
    fn fetch_wanted(&mut self, cx: &mut Context<Self>) {
        let wanted = pages::unasked(std::mem::take(&mut self.want), &mut self.asked);
        for (size, ids) in wanted {
            self.fetch(ids, Some(size), false, cx);
        }
    }

    fn fetch(&mut self, ids: Vec<String>, size: Option<u32>, retry: bool, cx: &mut Context<Self>) {
        let Some(account) = self.key().map(Key::account) else {
            return;
        };
        let request = Request::Fetch { retry, size, account: account.clone(), ids };
        cx.spawn(async move |this, cx| {
            let answer = cx.background_executor().spawn(async move { ask(&request) }).await;
            let Ok(Response::Media { files }) = answer else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.key().map(Key::account) != Some(account) {
                    return;
                }
                // What is already on disk, or already known to have failed, is answered here;
                // the rest arrives as changes.
                for file in files.iter().filter(|file| file.path.is_some() || file.failed.is_some()) {
                    this.landed(file, false, cx);
                }
            });
        })
        .detach();
    }

    /// The user clicked Retry on a file that failed: one request, which clears the failure.
    fn retry_file(&mut self, file: &MediaFile, cx: &mut Context<Self>) {
        let size = self.asked.get(&file.id).copied();
        let mut cleared = file.clone();
        cleared.failed = None;
        // Shown as loading at once; chatd's word on it replaces this.
        self.window.landed(&cleared);
        self.images.forget_failed();
        let pinned = self.pinned();
        self.relay(pinned, self.list.logical_scroll_top());
        self.fetch(vec![file.id.clone()], size, true, cx);
        cx.notify();
    }

    /// Fetches a whole file because the user asked for it, and does `then` when it is here.
    fn fetch_then(&mut self, attachment: &protocol::Attachment, then: Then, cx: &mut Context<Self>) {
        let file = &attachment.file;
        let name = media::disk_name(attachment);
        if let Some(path) = file.path.clone().or_else(|| self.known.get(&file.id).cloned()) {
            draw::open_file(&path, &name, then);
            return;
        }
        self.then.insert(file.id.clone(), (then, name));
        // The user asked, so a file that failed before is tried again.
        self.fetch(vec![file.id.clone()], None, file.failed.is_some(), cx);
        cx.notify();
    }

    /// The first message on screen and how far its top is below the top of the viewport.
    fn held_place(&self) -> Option<(String, f32)> {
        let viewport = self.list.viewport_bounds();
        let seen = self.seen.borrow();
        let mut rows: Vec<&(usize, Bounds<Pixels>)> = seen.iter().filter(|(_, bounds)| bounds.bottom() > viewport.top()).collect();
        rows.sort_by_key(|(ix, _)| *ix);
        rows.into_iter().find_map(|(ix, bounds)| {
            let message = self.items.get(*ix).and_then(Item::message)?;
            Some((message.id.clone(), f32::from(bounds.top() - viewport.top())))
        })
    }

    /// Puts the held message back where it was. The rows that arrived above it have never been
    /// drawn, so they are measured here, upward, until they cover the distance.
    fn settle_hold(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, below)) = self.hold.take() else {
            return;
        };
        #[cfg(feature = "gui-shot")]
        self.script.held(&id, below);
        let Some(mut at) = self.items.iter().position(|item| item.message().is_some_and(|message| message.id == id)) else {
            return;
        };
        if self.width <= 0. {
            return;
        }
        let mut left = below;
        if left <= 0. {
            self.list.scroll_to(ListOffset { item_ix: at, offset_in_item: px(-left) });
            return;
        }
        while left > 0. && at > 0 {
            at -= 1;
            let mut row = self.row(at, window, cx);
            let space = gpui::size(gpui::AvailableSpace::Definite(px(self.width)), gpui::AvailableSpace::MinContent);
            let tall = f32::from(row.layout_as_root(space, window, cx).height);
            if tall >= left {
                self.list.scroll_to(ListOffset { item_ix: at, offset_in_item: px(tall - left) });
                return;
            }
            left -= tall;
        }
        self.list.scroll_to(ListOffset::default());
    }

    // After each draw.

    /// Runs once the rows are laid out: asks for what they need, and notes how far the user
    /// has read. Nothing here draws.
    fn after_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.images.sweep(window, cx);
        let scale = window.scale_factor();
        if scale != self.scale {
            // Another screen: every size asked for was for the old one.
            if self.scale != 0. {
                self.asked.clear();
                cx.notify();
            }
            self.scale = scale;
        }
        self.fetch_wanted(cx);
        self.point_at(window.mouse_position(), cx);
        let viewport = self.list.viewport_bounds();
        let width = f32::from(viewport.size.width);
        if self.land {
            cx.notify();
        }
        if width != self.width {
            // Pictures are sized to the pane, so a new width lays the rows out again.
            self.width = width;
            cx.notify();
        }
        let seen = self.seen.borrow().clone();
        let on_screen = |bounds: &Bounds<Pixels>| bounds.bottom() > viewport.top() && bounds.top() < viewport.bottom();

        let built = |ix: usize| seen.iter().any(|(seen, _)| *seen == ix);
        if self.window.older && !self.loading_older && self.older_failed.is_none() && built(0) {
            match self.window.messages.first() {
                Some(first) => self.load(Page::Before { id: first.id.clone() }, Purpose::Older, cx),
                // The first page never came: Retry asks for it again.
                None => self.load(Page::Newest, Purpose::Open, cx),
            }
        }
        let last = self.items.len().saturating_sub(1);
        if self.window.newer && !self.loading_newer && built(last) {
            if let Some(newest) = self.window.messages.last() {
                self.load(Page::After { id: newest.id.clone() }, Purpose::Newer, cx);
            }
        }

        let end = seen.iter().find(|(ix, _)| *ix == last).map(|(_, bounds)| bounds.bottom());
        let away = !self.items.is_empty() && end.is_none_or(|end| end - viewport.bottom() > px(AWAY));
        if away != self.away {
            self.away = away;
            if !away {
                self.unseen = 0;
            }
            cx.notify();
        }

        #[cfg(feature = "gui-shot")]
        shot::drawn(self, window, cx);
        self.active = window.is_window_active();
        let newest = seen
            .iter()
            .filter(|(_, bounds)| on_screen(bounds))
            .filter_map(|(ix, _)| self.items.get(*ix).and_then(Item::message))
            .filter(|message| message.state == SendState::Sent)
            .max_by_key(|message| (message.sent, message.id.len(), &message.id))
            .cloned();
        if let Some(newest) = newest.filter(|_| may_mark_read(SCREENSHOTS, self.active, self.open.is_some(), true)) {
            self.saw(&newest, cx);
        }
    }

    /// The user has this message on screen in the active window. The network hears of the
    /// newest such message at most once a second.
    fn saw(&mut self, message: &Message, cx: &mut Context<Self>) {
        let order = (message.sent, message.id.len(), message.id.clone());
        if self.read.sent.as_ref().is_some_and(|sent| *sent >= order) {
            return;
        }
        let since = self.read.at.map(|at| at.elapsed());
        if since.is_none_or(|since| since >= READ_EVERY) {
            self.mark_read(order, cx);
            return;
        }
        self.read.next = Some(message.id.clone());
        if self.read.waiting {
            return;
        }
        self.read.waiting = true;
        let wait = READ_EVERY - since.unwrap_or_default();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |this, cx| {
                this.read.waiting = false;
                let next = this.read.next.take().and_then(|id| this.find(&id));
                // Only if it is still true: the window may have lost focus meanwhile.
                if let Some(next) = next.filter(|_| may_mark_read(SCREENSHOTS, this.active, this.open.is_some(), true)) {
                    this.mark_read((next.sent, next.id.len(), next.id.clone()), cx);
                }
            });
        })
        .detach();
    }

    fn mark_read(&mut self, order: (i64, usize, String), cx: &mut Context<Self>) {
        // The last gate before the socket, whoever called.
        if !may_mark_read(SCREENSHOTS, self.active, self.open.is_some(), true) {
            return;
        }
        let Some(conversation) = self.key().cloned() else {
            return;
        };
        let request = Request::MarkRead { conversation, message_id: order.2.clone() };
        self.read.sent = Some(order);
        self.read.at = Some(Instant::now());
        cx.background_executor().spawn(async move { ask(&request) }).detach();
    }

    // Keys and the clipboard.

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.viewer.is_some() {
            if self.viewer_key(event, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        if self.action_key(event, window, cx) {
            cx.stop_propagation();
            return;
        }
        // Never further than the rows already built: the list knows no heights beyond them.
        let page = (self.list.viewport_bounds().size.height - px(40.)).min(px(AHEAD - 40.));
        let keys = &event.keystroke;
        match keys.key.as_str() {
            "pageup" => self.scroll_by(-page),
            "pagedown" => {
                self.scroll_by(page);
                if self.list.is_scrolled_to_end() == Some(true) && !self.window.newer {
                    self.snap_to_end();
                }
            }
            "home" => self.list.scroll_to(ListOffset::default()),
            "end" => self.go_to_bottom(cx),
            "c" if crate::gui::shortcuts::primary(keys.modifiers) && self.selected_text().is_some() => self.copy_selection(cx),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// What is selected, as plain text.
    fn selected_text(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        let head = selection.head?;
        let message = self.find(&selection.message)?;
        let blocks = rich::blocks(&message.text, &message.spans);
        let text = rich::plain(&blocks, selection.anchor, head);
        (!text.is_empty()).then_some(text)
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// A whole message's text as it would be typed: what "Copy text" copies.
    pub fn message_text(&self, message_id: &str) -> Option<String> {
        let message = self.find(message_id)?;
        let blocks = rich::blocks(&message.text, &message.spans);
        Some(rich::plain(&blocks, (0, 0), (blocks.len(), usize::MAX)))
    }
}

impl Render for Pane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.images_stale {
            self.images_stale = false;
            self.images.clear(window, cx);
        }
        if self.watching_window.is_none() {
            // Coming back to the window is when what is on screen becomes read; leaving it
            // ends that at once, not at the next draw.
            self.watching_window = Some(cx.observe_window_activation(window, |this, window, cx| {
                this.active = window.is_window_active();
                cx.notify();
            }));
        }
        for path in std::mem::take(&mut self.replaced) {
            self.images.replaced(&path, window, cx);
        }
        for (path, edge) in std::mem::take(&mut self.let_go) {
            self.images.let_go(&path, edge, window, cx);
        }
        self.images.begin_frame();
        self.settle_hold(window, cx);
        self.settle_land(window, cx);
        if std::mem::take(&mut self.wants_focus) {
            window.focus(&self.focus, cx);
        }
        self.seen.borrow_mut().clear();
        let this = cx.entity();
        let rows = gpui::list(self.list.clone(), cx.processor(|this: &mut Pane, ix: usize, window, cx| this.row(ix, window, cx)))
            .size_full();
        let after = gpui::canvas(
            |_, _, _| (),
            move |_, (), window, cx| this.update(cx, |this, cx| this.after_layout(window, cx)),
        )
        .absolute()
        .size_0();
        div()
            .id("messages")
            .size_full()
            .relative()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.focus, cx);
                    // A press on text starts a selection; one anywhere else ends it.
                    if !std::mem::take(&mut this.on_text) && this.selection.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if std::mem::take(&mut this.dragging) {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| this.point_at(event.position, cx)))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    this.unpoint(cx);
                }
            }))
            .child(rows)
            .child(after)
            .children(self.away.then(|| self.jump_button(cx)))
            .children(self.action_layers(window, cx))
            .children(self.viewer_layer(window, cx))
    }
}

impl Shell {
    fn pane(&self, cx: &mut Context<Self>) -> Entity<Pane> {
        let pane = self
            .conversation_state
            .pane
            .get_or_init(|| {
                let shell = cx.weak_entity();
                cx.new(|cx| Pane::new(Metrics::FULL, shell, cx))
            })
            .clone();
        self.subscribe_pane(&pane, cx);
        self.follow_changes(cx);
        pane
    }

    /// My own id on the account a conversation belongs to.
    fn me(&self, conversation: &Key) -> String {
        let account = conversation.account();
        let accounts = self.snapshot.iter().flat_map(|snapshot| &snapshot.accounts);
        accounts.filter(|status| status.key == account).map(|status| status.me.clone()).next().unwrap_or_default()
    }

    /// Draws the panes again: a player in them moved on. Only marks them, so it is safe from
    /// inside a pane's own handler, where the pane cannot be updated.
    pub(super) fn conversation_redraw(&mut self, cx: &mut Context<Self>) {
        let state = &self.conversation_state;
        let panes: Vec<gpui::EntityId> = state.pane.get().into_iter().chain(state.thread.borrow().as_ref()).map(Entity::entity_id).collect();
        let app: &mut gpui::App = cx;
        for pane in panes {
            app.notify(pane);
        }
    }

    /// Offered every change chatd reports, after `Shell` has applied its own part.
    pub(super) fn conversation_changed(&mut self, change: &Change, cx: &mut Context<Self>) {
        let state = &self.conversation_state;
        for pane in state.pane.get().into_iter().chain(state.thread.borrow().as_ref()) {
            pane.update(cx, |pane, cx| pane.changed(change, cx));
        }
    }

    /// chatd no longer knows what changed: everything held is read again.
    #[allow(dead_code)]
    pub(super) fn conversation_reset(&mut self, cx: &mut Context<Self>) {
        let state = &self.conversation_state;
        for pane in state.pane.get().into_iter().chain(state.thread.borrow().as_ref()) {
            pane.update(cx, |pane, cx| pane.refresh(cx));
        }
    }

    /// Follows the newest message, for after the user sends one.
    #[allow(dead_code)]
    pub(super) fn conversation_to_bottom(&mut self, cx: &mut Context<Self>) {
        if let Some(pane) = self.conversation_state.pane.get() {
            pane.update(cx, |pane, cx| pane.go_to_bottom(cx));
        }
    }

    /// Copies the text of a message of the open conversation, for the message menu.
    #[allow(dead_code)]
    pub(super) fn copy_message(&self, message_id: &str, cx: &mut Context<Self>) {
        if let Some(pane) = self.conversation_state.pane.get() {
            if let Some(text) = pane.read(cx).message_text(message_id) {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
        }
    }

    pub(super) fn conversation(&self, snapshot: &Snapshot, window: &Window, cx: &mut Context<Self>) -> Div {
        let Some(open) = snapshot.open.as_ref() else {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(drags_window(div().h(px(TOP_BAR)).flex_none()))
                .child(empty_state(
                    "icons/message-circle.svg",
                    "Select a conversation",
                    "Telegram, Discord, Slack and Teams, in one list.",
                ));
        };
        #[cfg(feature = "gui-shot")]
        let staged = shot::instead(open);
        #[cfg(feature = "gui-shot")]
        let open = staged.as_ref().unwrap_or(open);
        let link = snapshot.account_link(&open.key);
        let pane = self.pane(cx);
        let me = self.me(&open.key);
        let picture = self.conversation_picture(open, cx);
        // With no composer to type in, the messages take the keys after an open.
        let typing = open.special != Special::Service;
        pane.update(cx, |pane, cx| pane.show(open, &me, picture, typing, cx));
        let keys = pane.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            // Paging keys reach the messages from the composer too.
            .on_key_down(move |event: &KeyDownEvent, window, cx| {
                if matches!(event.keystroke.key.as_str(), "pageup" | "pagedown") {
                    keys.update(cx, |pane, cx| pane.key_down(event, window, cx));
                }
            })
            .child(self.strip(open, link, window, cx))
            .child(div().flex_1().min_h_0().child(pane))
            .child(match open.special {
                Special::Service => self.no_composer(open, cx),
                _ => self.composer(open, link, cx),
            })
    }

    /// The network's own account (Telegram's service notifications) takes no messages: a line
    /// says so where the composer would be. The card and the sweep the composer hosts still run.
    fn no_composer(&self, open: &Conversation, cx: &mut Context<Self>) -> Div {
        let floating = self.floating(cx);
        self.sweep(cx);
        let network = open.key.network.label();
        div()
            .flex_none()
            .h(px(52.))
            .px(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .border_t_1()
            .border_color(theme::hairline())
            .type_ui()
            .text_color(theme::text_faint())
            .child(format!("Only {network} posts here."))
            .children(floating)
    }
}

/// My pending messages as they were first drawn: chatd may stamp one again while it goes,
/// and a row that moves or changes its time under the user is not where they left it. What
/// is no longer pending is let go.
fn held_in_place(messages: &[Rc<Message>], first: &mut HashMap<String, i64>) -> Vec<Rc<Message>> {
    first.retain(|client, _| messages.iter().any(|m| m.state == SendState::Pending && m.client_id.as_deref() == Some(client)));
    let mut moved = false;
    let mut held: Vec<Rc<Message>> = messages
        .iter()
        .map(|message| {
            let Some(client) = message.client_id.as_ref().filter(|_| message.state == SendState::Pending) else {
                return message.clone();
            };
            let sent = *first.entry(client.clone()).or_insert(message.sent);
            if sent == message.sent {
                return message.clone();
            }
            moved = true;
            Rc::new(Message { sent, ..(**message).clone() })
        })
        .collect();
    if moved {
        held.sort_by_key(|message| message.sent);
    }
    held
}

/// Where the list's top goes so the "New" line at row `new` sits a third of the way down a
/// viewport `height` tall, given the height of everything from the line down (`below`, at
/// least until it passes the viewport) and each row's height above it. `None` when what is
/// unread fits: the list stays at the bottom.
fn landing(below: f32, height: f32, new: usize, mut tall: impl FnMut(usize) -> f32) -> Option<ListOffset> {
    if below <= height {
        return None;
    }
    let want = height / 3.;
    let (mut at, mut above) = (new, 0.);
    while at > 0 && above < want {
        at -= 1;
        above += tall(at);
    }
    let offset = if above >= want { above - want } else { 0. };
    Some(ListOffset { item_ix: at, offset_in_item: px(offset) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_row_keeps_its_time_and_place_until_it_is_sent() {
        let said = |id: &str, sent: i64| Rc::new(Message::plain(id, "Ines", id, sent, false));
        let mut mine = Message::plain("p1", "Me", "hi", 100, true);
        mine.state = SendState::Pending;
        mine.client_id = Some("c1".into());
        let mut first = HashMap::new();
        let held = held_in_place(&[said("a", 90), Rc::new(mine.clone()), said("b", 110)], &mut first);
        assert_eq!(held.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a", "p1", "b"]);
        // Stamped again later: drawn where and when it was.
        mine.sent = 130;
        let held = held_in_place(&[said("a", 90), said("b", 110), Rc::new(mine.clone())], &mut first);
        assert_eq!(held.iter().map(|m| (m.id.as_str(), m.sent)).collect::<Vec<_>>(), [("a", 90), ("p1", 100), ("b", 110)]);
        // Sent: its own time from then on, and nothing is held for it.
        mine.state = SendState::Sent;
        let held = held_in_place(&[said("a", 90), said("b", 110), Rc::new(mine)], &mut first);
        assert_eq!(held.last().map(|m| m.sent), Some(130));
        assert!(first.is_empty());
    }

    #[test]
    fn unread_that_fits_opens_at_the_bottom_and_more_puts_the_line_a_third_down() {
        let place = |at: Option<ListOffset>| at.map(|at| (at.item_ix, f32::from(at.offset_in_item)));
        // A 600 tall viewport: rows of 40 above the line at row 20.
        assert_eq!(place(landing(500., 600., 20, |_| 40.)), None);
        // 200 above the line: five rows of 40, so row 15 at the top with nothing cut off.
        assert_eq!(place(landing(900., 600., 20, |_| 40.)), Some((15, 0.)));
        // Rows that do not divide evenly: the top one is cut so the line still lands at 200.
        let (ix, offset) = place(landing(900., 600., 20, |_| 47.)).unwrap();
        assert_eq!(ix, 15);
        assert!((offset - (47. * 5. - 200.)).abs() < 0.01);
        // Too little history above: the top of what is held.
        assert_eq!(place(landing(900., 600., 2, |_| 40.)), Some((0, 0.)));
    }

    #[test]
    fn nothing_is_marked_read_from_a_screenshot_an_inactive_window_or_off_screen() {
        // The one case that tells the network: a real, active window with the message on show.
        assert!(may_mark_read(false, true, true, true));
        // A build that takes screenshots never does, however the window looks to it.
        for (active, open, on_screen) in [(true, true, true), (false, true, true), (true, false, true), (true, true, false)] {
            assert!(!may_mark_read(true, active, open, on_screen));
        }
        // A real window in the background, with no conversation, or scrolled away from it.
        assert!(!may_mark_read(false, false, true, true));
        assert!(!may_mark_read(false, true, false, true));
        assert!(!may_mark_read(false, true, true, false));
        // This build is the one the feature says it is.
        assert_eq!(SCREENSHOTS, cfg!(feature = "gui-shot"));
    }

    #[test]
    fn a_selection_covers_the_blocks_between_its_ends_whichever_way_it_was_dragged() {
        let dragged = |anchor, head| Selection { message: "m".into(), anchor, head: Some(head) };
        let down = dragged((1, 4), (3, 2));
        assert_eq!(down.within(0), None);
        assert_eq!(down.within(1), Some(4..usize::MAX));
        assert_eq!(down.within(2), Some(0..usize::MAX));
        assert_eq!(down.within(3), Some(0..2));
        assert_eq!(down.within(4), None);
        // Backwards is the same stretch.
        let up = dragged((3, 2), (1, 4));
        assert_eq!((up.within(1), up.within(3)), (Some(4..usize::MAX), Some(0..2)));
        // A press that has not moved selects nothing.
        let pressed = Selection { message: "m".into(), anchor: (1, 4), head: None };
        assert_eq!(pressed.within(1), None);
        assert_eq!(dragged((1, 4), (1, 4)).within(1), None);
    }
}
