//! The right pane on the window: `pane`'s rules applied as the open conversation changes, the
//! layout's one slot kept in step with them, and the pane's body.
//!
//! The rules run in an observer of the shell, which GPUI calls after every change and before
//! the next frame is drawn: a switch and the pane that follows it land in the same frame, and
//! the old subject is never drawn beside the new conversation.

use crate::gui::pane::{self, Layer, Showing, Stack, Step};
use super::{Pane, PaneEvent};
use crate::client::ask;
use crate::gui::components::{empty_state, glyph};
use crate::gui::layout::{RightKind, RightPane};
use crate::gui::state::Shell;
use crate::gui::theme::{self, Type};
use crate::gui::time;
use crate::view::{age, now};
use gpui::{div, prelude::*, px, AnyElement, Context, Entity, MouseButton, Window};
use protocol::{ConversationKind, Key, Link, Message, MessageHit, Request, Response};
use std::rc::Rc;

/// The window keeps of the right pane.
#[derive(Default)]
pub struct Side {
    pub stack: Stack,
    pub switches: pane::Switches,
    /// Threads put away on leaving their conversation, by conversation.
    threads: pane::Memory<Rc<Message>>,
    /// The message search's page: what was typed, and what the cache holds for it.
    pub search: Option<SearchPage>,
    /// The conversation the stack was last fitted to.
    fitted: Option<Key>,
    /// The kind this view last put in the layout's slot, so another view's change can be told
    /// from its own.
    installed: Option<RightKind>,
    /// The pins of a conversation, once asked for.
    pins: Option<Pins>,
    /// The switches were read from what the window remembers.
    restored: bool,
    /// The window is too narrow for the pane beside the conversation: the layout says so each
    /// frame. Details hides then, and a thread waits to be asked for (07 §4).
    narrow: std::cell::Cell<bool>,
    /// Details was opened by hand at this width, as a one-off that replaces the conversation.
    asked: bool,
    /// The conversation region's width in the last frame, for what its strip can hold.
    room: std::cell::Cell<f32>,
}

/// One message search's page: what was typed, and what the cache holds for it.
#[derive(Default)]
pub struct SearchPage {
    pub words: String,
    pub hits: Vec<MessageHit>,
}

impl Side {
    /// A Details or Members button pressed in a window too narrow for the pane beside the
    /// conversation: it opens once, in place of the conversation, and a second press puts it
    /// away again. The switches are left as they were. False when the window has room.
    fn reveal_once(&mut self, wanted: pane::ChannelSwitch, open: &pane::Open) -> bool {
        if !self.narrow.get() || self.stack.push.is_some() {
            return false;
        }
        let layer = match (wanted, &open.direct) {
            (_, Some(peer)) => peer.clone().map(Layer::Profile),
            (pane::ChannelSwitch::Members, None) => Some(Layer::Members(open.key.clone())),
            (_, None) => Some(Layer::Details(open.key.clone())),
        };
        let Some(layer) = layer else {
            return false;
        };
        let before = self.stack.clone();
        if self.asked && self.stack.base.as_ref() == Some(&layer) {
            self.stack = Stack { base: pane::derived(self.switches, open), push: None };
            self.asked = false;
        } else {
            self.stack = Stack { base: Some(layer), push: None };
            self.asked = true;
        }
        pane::keep_thread(&before, &self.stack, &mut self.threads);
        true
    }
}

struct Pins {
    conversation: Key,
    messages: Option<Result<Vec<Rc<Message>>, String>>,
}

fn kind_of(layer: &Layer) -> RightKind {
    match layer {
        Layer::Profile(_) | Layer::Details(_) | Layer::Space(_) => RightKind::Info,
        Layer::Members(_) => RightKind::Members,
        Layer::Thread { .. } | Layer::Pinned(_) => RightKind::Thread,
        Layer::Search => RightKind::Search,
    }
}

/// The open conversation as the pane's rules need it.
fn pane_open(open: &protocol::Conversation) -> pane::Open {
    let direct = (open.kind == ConversationKind::Direct)
        .then(|| open.peer.as_ref().map(|peer| Key { id: peer.clone(), ..open.key.clone() }));
    pane::Open { key: open.key.clone(), direct }
}

/// The key a thread is read and posted under: its own id, on its conversation's account.
fn thread_key(conversation: &Key, root: &Message) -> Option<Key> {
    root.thread.as_ref().map(|thread| Key { id: thread.id.clone(), ..conversation.clone() })
}

/// Whether a thread of `conversation` may be drawn, composer and all, beside `open`. The one
/// gate the pane's body passes before it draws a thread: never beside another conversation.
pub(super) fn thread_belongs(conversation: &Key, open: Option<&Key>) -> bool {
    open == Some(conversation)
}

impl Shell {
    fn open_key(&self) -> Option<&Key> {
        self.snapshot.as_ref().and_then(|snapshot| snapshot.open.as_ref()).map(|open| &open.key)
    }

    /// Starts applying the pane's rules after every change. Called while drawing the window,
    /// whatever it shows: a pane standing in for the conversation must follow too.
    pub(in crate::gui) fn follow_changes(&self, cx: &mut Context<Self>) {
        self.conversation_state.following.get_or_init(|| {
            #[cfg(feature = "gui-shot")]
            super::shot::steps(cx);
            cx.observe_self(|shell, cx| shell.follow_open(cx))
        });
    }

    /// After any change: a new conversation gets the pane its rules say; otherwise another
    /// view's change to the slot is taken in.
    fn follow_open(&mut self, cx: &mut Context<Self>) {
        self.restore_switches();
        let open = self.snapshot.as_ref().and_then(|snapshot| snapshot.open.as_ref());
        let key = open.map(|open| open.key.clone());
        if key == self.conversation_state.side.fitted {
            self.take_in(cx);
            return;
        }
        let to = open.map(pane_open);
        let side = &mut self.conversation_state.side;
        side.stack = pane::follow(&side.stack, side.switches, to.as_ref(), &mut side.threads, side.narrow.get());
        side.fitted = key;
        side.asked = false;
        self.install(cx);
    }

    /// Reads the two switches from what the window remembers, once, before anything uses them:
    /// a header button pressed before the first change would otherwise be undone by it.
    fn restore_switches(&mut self) {
        if self.conversation_state.side.restored {
            return;
        }
        let state = &self.layout_state;
        let side = &mut self.conversation_state.side;
        side.restored = true;
        side.switches.direct = state.details_directs;
        side.switches.channel = if state.details_channels { pane::ChannelSwitch::Members } else { pane::ChannelSwitch::Off };
    }

    /// For `--shot`: the channel switch on Members, as a wider window would have left it.
    #[cfg(feature = "gui-shot")]
    pub(super) fn members_switch_on(&mut self, cx: &mut Context<Self>) {
        self.restore_switches();
        self.conversation_state.side.switches.channel = pane::ChannelSwitch::Members;
        self.conversation_state.side.fitted = None;
        self.follow_open(cx);
    }

    /// Puts what the stack says in the layout's slot, and builds the views it needs.
    fn install(&mut self, cx: &mut Context<Self>) {
        self.remember_switches(cx);
        let stack = self.conversation_state.side.stack.clone();
        match stack.thread() {
            Some((conversation, root)) => self.keep_thread(conversation, root, cx),
            None => drop(self.conversation_state.thread.borrow_mut().take()),
        }
        let top = stack.top().cloned();
        if let Some(layer @ (Layer::Profile(_) | Layer::Details(_))) = &top {
            // A person picked inside a Space is shown as that Space knows them.
            let space = self.snapshot.as_ref().and_then(|s| s.open.as_ref()).filter(|open| open.kind != ConversationKind::Direct);
            let space = space.and_then(|open| open.space.clone().map(|id| Key { id, ..open.key.clone() }));
            self.pane_show(layer, space, cx);
        }
        if let Some(Layer::Pinned(conversation)) = &top {
            self.ask_pins(conversation, cx);
        }
        match top {
            Some(layer) => {
                let kind = kind_of(&layer);
                self.conversation_state.side.installed = Some(kind);
                self.open_right(RightPane { kind, title: layer.title().into(), count: None, body: draw_side }, cx);
            }
            None => {
                if self.conversation_state.side.installed.take().is_some() && self.right_kind().is_some() {
                    self.close_right(cx);
                }
            }
        }
    }

    /// Another view changed the slot: the ✕, `cmd-.`, a member row's profile, the members
    /// toggle. The stack takes the change in and the slot gets this view's body back.
    fn take_in(&mut self, cx: &mut Context<Self>) {
        let side = &self.conversation_state.side;
        let slot = self.right_kind();
        let mine = side.stack.top().map(kind_of);
        let shown = self.pane_subject(cx);
        if slot == mine {
            // The same kind, but another view may have put someone else in Details.
            let top = side.stack.top().filter(|top| matches!(top, Layer::Profile(_) | Layer::Details(_) | Layer::Space(_)));
            if let (Some(top), Some(shown)) = (top, shown) {
                if *top != shown {
                    let side = &mut self.conversation_state.side;
                    side.stack = pane::open(&side.stack, shown);
                    self.install(cx);
                }
            }
            return;
        }
        let side = &mut self.conversation_state.side;
        match slot {
            // In a narrow window the pane stood in for the conversation, and "‹ #channel" or ✕
            // went back to it: sticky Details goes back to hiding with its switch still on.
            None if side.installed.is_some() && side.narrow.get() && side.stack.base.as_ref().is_some_and(Layer::is_details) => {
                let open = self.snapshot.as_ref().and_then(|snapshot| snapshot.open.as_ref()).map(pane_open);
                let side = &mut self.conversation_state.side;
                side.stack = Stack { base: open.and_then(|open| pane::derived(side.switches, &open)), push: None };
                side.asked = false;
                self.install(cx);
            }
            None if side.installed.is_some() => {
                side.switches = pane::close(&side.stack, side.switches);
                side.stack = Stack::default();
                side.installed = None;
                self.conversation_state.thread.borrow_mut().take();
            }
            Some(RightKind::Info) => {
                if let Some(shown) = shown {
                    side.asked = true;
                    side.stack = pane::open(&side.stack, shown);
                    self.install(cx);
                }
            }
            Some(RightKind::Members) => {
                if let Some(open) = self.snapshot.as_ref().and_then(|snapshot| snapshot.open.as_ref()).map(pane_open) {
                    let side = &mut self.conversation_state.side;
                    side.switches.channel = pane::ChannelSwitch::Members;
                    side.stack = Stack { base: pane::derived(side.switches, &open), push: None };
                    self.install(cx);
                }
            }
            // Something this view does not draw (search, a sheet): the stack makes way, and a
            // thread in it is kept for its conversation.
            _ => {
                pane::keep_thread(&side.stack, &Stack::default(), &mut side.threads);
                side.stack = Stack::default();
                side.installed = None;
            }
        }
    }

    /// The thread view for this root, made once and kept while the stack holds it.
    fn keep_thread(&mut self, conversation: &Key, root: &Rc<Message>, cx: &mut Context<Self>) {
        let Some(key) = thread_key(conversation, root) else {
            return;
        };
        let held = self.conversation_state.thread.borrow().as_ref().and_then(|thread| thread.read(cx).key().cloned());
        if held.as_ref() == Some(&key) {
            return;
        }
        let title = self.snapshot.as_ref().and_then(|s| s.open.as_ref()).map(|open| open.title.clone()).unwrap_or_default();
        let me = self.me(conversation);
        let shell = cx.weak_entity();
        let root = root.clone();
        let view = cx.new(|cx| {
            let mut view = Pane::new(super::tokens::Metrics::DENSE, shell, cx);
            view.show_thread(key, &title, &me, root, cx);
            view
        });
        *self.conversation_state.thread.borrow_mut() = Some(view);
    }

    /// The reply line under a message of the open conversation was clicked.
    pub(in crate::gui) fn open_thread_of(&mut self, conversation: Key, root: Rc<Message>, cx: &mut Context<Self>) {
        // A click that raced a switch belongs to a conversation no longer open.
        if self.open_key() != Some(&conversation) || root.thread.is_none() {
            return;
        }
        let side = &mut self.conversation_state.side;
        side.threads.forget(&conversation);
        side.stack = pane::open(&side.stack, Layer::Thread { conversation, root });
        self.install(cx);
    }

    /// The header's Details button.
    pub(super) fn toggle_details(&mut self, cx: &mut Context<Self>) {
        self.restore_switches();
        let Some(open) = self.snapshot.as_ref().and_then(|snapshot| snapshot.open.as_ref()).map(pane_open) else {
            return;
        };
        let side = &mut self.conversation_state.side;
        let wanted = if open.direct.is_some() { pane::ChannelSwitch::Off } else { pane::ChannelSwitch::Details };
        if !side.reveal_once(wanted, &open) {
            let before = side.stack.clone();
            (side.stack, side.switches) = pane::toggle_details(&side.stack, side.switches, &open);
            pane::keep_thread(&before, &side.stack, &mut side.threads);
        }
        self.install(cx);
    }

    /// The header's Members button.
    pub(super) fn toggle_member_list(&mut self, cx: &mut Context<Self>) {
        self.restore_switches();
        let Some(open) = self.snapshot.as_ref().and_then(|snapshot| snapshot.open.as_ref()).map(pane_open) else {
            return;
        };
        let side = &mut self.conversation_state.side;
        if !side.reveal_once(pane::ChannelSwitch::Members, &open) {
            let before = side.stack.clone();
            (side.stack, side.switches) = pane::toggle_members(&side.stack, side.switches, &open);
            pane::keep_thread(&before, &side.stack, &mut side.threads);
        }
        self.install(cx);
    }

    /// The switcher's "Search messages" row: the pane opens at once and the cache is asked.
    pub(in crate::gui) fn open_message_search(&mut self, words: String, cx: &mut Context<Self>) {
        let side = &mut self.conversation_state.side;
        side.search = Some(SearchPage { words: words.clone(), hits: Vec::new() });
        side.stack = pane::open(&side.stack, Layer::Search);
        self.install(cx);
        self.off_thread(
            cx,
            {
                let asked = words.clone();
                move || ask(&Request::SearchMessages { conversation: None, network: None, query: asked, limit: protocol::PAGE_LIMIT })
            },
            move |this, answer, cx| {
                let Ok(Response::MessageHits { hits }) = answer else {
                    return;
                };
                let Some(page) = this.conversation_state.side.search.as_mut() else {
                    return;
                };
                if page.words != words {
                    return;
                }
                page.hits = hits;
                cx.notify();
            },
        );
    }

    /// A search hit was clicked: its conversation opens on the message, or, being open already,
    /// jumps to it.
    pub(in crate::gui) fn open_search_hit(&mut self, hit: &MessageHit, cx: &mut Context<Self>) {
        let key = hit.conversation.clone();
        if self.open.as_ref() == Some(&key) {
            if let Some(pane) = self.conversation_state.pane.get() {
                pane.update(cx, |pane, cx| pane.jump_to(&hit.message_id, cx));
            }
            return;
        }
        if let Some(pane) = self.conversation_state.pane.get() {
            pane.update(cx, |pane, _| pane.land_next(hit.message_id.clone()));
        }
        let title = if hit.title.is_empty() { hit.conversation.id.clone() } else { hit.title.clone() };
        self.select(key, &title, cx);
    }

    /// For slice D's Details: the open conversation's pinned messages, over Details.
    #[allow(dead_code)]
    pub(in crate::gui) fn open_pins(&mut self, cx: &mut Context<Self>) {
        let Some(conversation) = self.open_key().cloned() else {
            return;
        };
        let side = &mut self.conversation_state.side;
        side.stack = pane::open(&side.stack, Layer::Pinned(conversation));
        self.install(cx);
    }

    /// The two switches go to the window's prefs, which keep them across a restart. Members
    /// or Details beside channels is this session's; the prefs say only on or off.
    fn remember_switches(&mut self, cx: &mut Context<Self>) {
        let switches = self.conversation_state.side.switches;
        let wanted = (switches.channel != pane::ChannelSwitch::Off, switches.direct);
        let state = &mut self.layout_state;
        if (state.details_channels, state.details_directs) != wanted {
            (state.details_channels, state.details_directs) = wanted;
            self.save_prefs(cx);
        }
    }

    /// What the header lights: the kind on screen now. Details hidden by a narrow window is
    /// not on screen, so its button is unlit.
    pub(super) fn pane_shows(&self, kind: fn(&Layer) -> bool) -> bool {
        !self.pane_tucked() && self.conversation_state.side.stack.top().is_some_and(kind)
    }

    /// The layout, each frame: whether the window is too narrow for the pane beside the
    /// conversation, and how wide the conversation is drawn.
    pub(in crate::gui) fn note_room(&self, narrow: bool, room: Option<f32>) {
        let side = &self.conversation_state.side;
        side.narrow.set(narrow);
        if let Some(room) = room {
            side.room.set(room);
        }
    }

    pub(in crate::gui) fn pane_narrow(&self) -> bool {
        self.conversation_state.side.narrow.get()
    }

    /// The conversation region's width in the last frame; 0 before the first.
    pub(super) fn conversation_room(&self) -> f32 {
        self.conversation_state.side.room.get()
    }

    /// Details is on but hidden, because the window is too narrow for it (07 §4).
    pub(in crate::gui) fn pane_tucked(&self) -> bool {
        let side = &self.conversation_state.side;
        pane::tucked(&side.stack, side.narrow.get(), side.asked)
    }

    /// The crumb's way back: the pushed layer goes.
    fn pop(&mut self, cx: &mut Context<Self>) {
        if self.conversation_state.side.stack.push.take().is_some() {
            self.install(cx);
        }
    }

    /// What is up that the Escape ladder (07 §6) passes before the pane. The composer's own
    /// list, edit and reply are its to close while it has the keys.
    fn showing(&self, cx: &mut Context<Self>) -> Showing {
        let viewer = self.conversation_state.pane.get().is_some_and(|pane| pane.read(cx).viewer.is_some());
        Showing {
            autocomplete: false,
            menu: self.layout_state.menu.is_some(),
            card: self.pane_card_open(cx),
            viewer,
            switcher: self.sidebar_state.switcher.is_some(),
            floating_sidebar: self.layout_state.floating,
            editing: self.composer_editing(cx).is_some(),
            replying: self.composer_replying(cx),
        }
    }

    /// Escape (07 §6), one step per press. False when the step belongs to a view that takes
    /// the key itself (the viewer, an edit, the card): nothing here closes in its place, so
    /// Details is never closed for it.
    pub(in crate::gui) fn pane_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let showing = self.showing(cx);
        let step = pane::escape(showing, &self.conversation_state.side.stack);
        self.escape_step(step, Some(window), cx)
    }

    /// For the composer: Escape found nothing of its own to close (no list, no edit, no reply),
    /// so the ladder goes on from there instead of the field letting go of the keys. One press
    /// closes a thread; with nothing open it reads to the newest, and the composer keeps focus.
    #[allow(dead_code)]
    pub(in crate::gui) fn composer_escaped(&mut self, cx: &mut Context<Self>) {
        let showing = Showing { autocomplete: false, editing: false, replying: false, ..self.showing(cx) };
        let step = pane::escape(showing, &self.conversation_state.side.stack);
        self.escape_step(step, None, cx);
    }

    fn escape_step(&mut self, step: Step, window: Option<&mut Window>, cx: &mut Context<Self>) -> bool {
        match step {
            Step::Menu => self.close_menu(cx),
            Step::Switcher => match window {
                Some(window) => self.close_switcher(window, cx),
                None => return false,
            },
            Step::FloatingSidebar => {
                self.layout_state.floating = false;
                cx.notify();
            }
            Step::Pop => self.pop(cx),
            Step::ClosePane => {
                let side = &mut self.conversation_state.side;
                side.stack = Stack { base: None, push: None };
                self.install(cx);
                // The thread's composer may have had the keys: they go back to the conversation's.
                if let Some(window) = window {
                    self.composer_focus(window, cx);
                }
            }
            Step::Latest => {
                // A pane this view did not put there (search results, a sheet) closes first.
                if self.conversation_state.side.installed.is_none() && self.right_kind().is_some() {
                    self.close_right(cx);
                    return true;
                }
                if let Some(pane) = self.conversation_state.pane.get().cloned() {
                    pane.update(cx, |pane, cx| pane.go_to_bottom(cx));
                }
                self.mark_open_read(cx);
                if let Some(window) = window {
                    self.composer_focus(window, cx);
                }
            }
            Step::Autocomplete | Step::Card | Step::Viewer | Step::Edit | Step::Reply => return false,
        }
        true
    }

    fn ask_pins(&mut self, conversation: &Key, cx: &mut Context<Self>) {
        let side = &mut self.conversation_state.side;
        if side.pins.as_ref().is_some_and(|pins| pins.conversation == *conversation) {
            return;
        }
        side.pins = Some(Pins { conversation: conversation.clone(), messages: None });
        let request = Request::Pinned { conversation: conversation.clone() };
        let conversation = conversation.clone();
        cx.spawn(async move |this, cx| {
            let answer = cx.background_executor().spawn(async move { ask(&request) }).await;
            let messages = match answer {
                Ok(Response::Pinned { messages, .. }) => Ok(messages.into_iter().map(Rc::new).collect()),
                Ok(Response::Error { detail, .. }) => Err(detail),
                Ok(_) => Err("The pins could not be read.".to_owned()),
                Err(err) => Err(crate::client::trouble_words(&err)),
            };
            let _ = this.update(cx, |shell, cx| {
                if let Some(pins) = shell.conversation_state.side.pins.as_mut().filter(|pins| pins.conversation == conversation) {
                    pins.messages = Some(messages);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// A pin was clicked: the conversation shows it, as a reply quote would.
    fn jump_to_pin(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(pane) = self.conversation_state.pane.get().cloned() {
            let id = id.to_owned();
            pane.update(cx, |pane, cx| pane.jump_to(&id, cx));
        }
    }

    pub(super) fn subscribe_pane(&self, pane: &Entity<Pane>, cx: &mut Context<Self>) {
        self.conversation_state.listening.get_or_init(|| {
            cx.subscribe(pane, |shell, pane, event: &PaneEvent, cx| match event {
                PaneEvent::OpenThread(root) => {
                    if let Some(conversation) = pane.read(cx).key().cloned() {
                        shell.open_thread_of(conversation, root.clone(), cx);
                    }
                }
            })
        });
    }
}

/// The right pane's body, whatever its kind. Nothing is drawn that is about a conversation
/// other than the open one, not even for a frame.
fn draw_side(shell: &Shell, window: &mut Window, cx: &mut Context<Shell>) -> AnyElement {
    let stack = &shell.conversation_state.side.stack;
    let open = shell.open_key();
    let Some(top) = stack.top() else {
        return div().into_any_element();
    };
    if stack.layers().filter_map(Layer::conversation).any(|conversation| Some(conversation) != open) {
        return div().into_any_element();
    }
    let body = match top {
        Layer::Profile(_) | Layer::Details(_) | Layer::Space(_) => shell.pane_details(window, cx),
        Layer::Members(_) => shell.pane_members(window, cx),
        Layer::Thread { conversation, .. } => draw_thread(shell, conversation, cx),
        Layer::Pinned(conversation) => draw_pins(shell, conversation, cx),
        Layer::Search => draw_search(shell, cx),
    };
    let crumb = stack.push.as_ref().and(stack.base.as_ref()).map(|base| crumb(base.title(), cx));
    div()
        .size_full()
        .flex()
        .flex_col()
        .children(crumb)
        .child(div().flex_1().min_h_0().child(body))
        .into_any_element()
}

/// The way back from a pushed layer to the base, at the pane's top.
// CONTRACT: the design puts the crumb in the pane's title strip, which `layout.rs` draws; it
// can call `side::crumb` there and this row goes.
fn crumb(base: &'static str, cx: &mut Context<Shell>) -> AnyElement {
    div()
        .flex_none()
        .h(px(36.))
        .px(px(8.))
        .flex()
        .items_center()
        .border_b_1()
        .border_color(theme::hairline())
        .child(
            div()
                .id("pane-crumb")
                .h(px(28.))
                .pl(px(4.))
                .pr(px(8.))
                .rounded(px(theme::R_CONTROL))
                .flex()
                .items_center()
                .gap(px(4.))
                .cursor_pointer()
                .hover(|style| style.bg(theme::hover()))
                .active(|style| style.bg(theme::pressed()))
                .child(glyph("icons/chevron-left.svg", 16., theme::text_soft()))
                .child(div().type_ui().font_weight(theme::MEDIUM).text_color(theme::text_soft()).child(base))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|shell, _, _, cx| shell.pop(cx))),
        )
        .into_any_element()
}

/// The message search's pane: what was asked, and the cache's hits, newest first. Every hit is
/// the cache's word for it, so a row says "cached"; the networks are a later ask.
fn draw_search(shell: &Shell, cx: &mut Context<Shell>) -> AnyElement {
    let Some(page) = &shell.conversation_state.side.search else {
        return div().into_any_element();
    };
    let head = div()
        .h(px(44.))
        .flex_none()
        .px(px(12.))
        .flex()
        .items_center()
        .gap(px(8.))
        .border_b_1()
        .border_color(theme::hairline())
        .child(glyph("icons/search.svg", 14., theme::text_soft()))
        .child(div().flex_1().min_w_0().truncate().type_ui().text_color(theme::text()).child(page.words.clone()))
        .child(div().type_small().text_color(theme::text_soft()).child("cached"));
    let body = if page.hits.is_empty() {
        empty_state("icons/search.svg", "Nothing in the cache yet.", "The networks are not asked yet.")
            .into_any_element()
    } else {
        let mut rows = div().id("search-hits").flex_1().min_h_0().overflow_y_scroll().flex().flex_col().p(px(4.));
        for (ix, hit) in page.hits.iter().enumerate() {
            let clicked = hit.clone();
            rows = rows.child(
                div()
                    .id(("hit", ix))
                    .flex_none()
                    .px(px(10.))
                    .py(px(8.))
                    .rounded(px(theme::R_CONTROL))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::hover()))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_search_hit(&clicked, cx)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().flex_1().min_w_0().truncate().type_ui().text_color(theme::text()).child(hit.title.clone()))
                            .child(div().flex_none().type_small().text_color(theme::text_soft()).child(age(now(), hit.sent))),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(div().flex_1().min_w_0().truncate().type_small().text_color(theme::text_soft()).child(hit.snippet.clone()))
                            .child(div().flex_none().type_micro().text_color(theme::text_soft()).child("cached")),
                    ),
            );
        }
        rows.into_any_element()
    };
    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(theme::canvas())
        .child(head)
        .child(body)
        .into_any_element()
}

/// The thread and its composer. The composer posts to the thread's own key, and only while
/// that thread belongs to the open conversation.
fn draw_thread(shell: &Shell, conversation: &Key, cx: &mut Context<Shell>) -> AnyElement {
    let thread = shell.conversation_state.thread.borrow().clone();
    let Some(thread) = thread.filter(|_| thread_belongs(conversation, shell.open_key())) else {
        return div().into_any_element();
    };
    let target = thread.read(cx).open.as_ref().map(|open| (open.key.clone(), open.title.clone()));
    let link = shell.snapshot.as_ref().map_or(Link::Offline, |snapshot| snapshot.account_link(conversation));
    div()
        .size_full()
        .flex()
        .flex_col()
        .bg(theme::canvas())
        .child(div().flex_1().min_h_0().child(thread))
        .children(target.map(|(key, title)| shell.thread_composer(&key, &title, link, cx)))
        .into_any_element()
}

fn draw_pins(shell: &Shell, conversation: &Key, cx: &mut Context<Shell>) -> AnyElement {
    let pins = shell.conversation_state.side.pins.as_ref().filter(|pins| pins.conversation == *conversation);
    let messages = match pins.and_then(|pins| pins.messages.as_ref()) {
        None => return div().into_any_element(),
        Some(Err(why)) => return empty_state("icons/pin.svg", "Pins aren't available", why).into_any_element(),
        Some(Ok(messages)) if messages.is_empty() => {
            return empty_state("icons/pin.svg", "Nothing pinned", "Pinned messages show here.").into_any_element();
        }
        Some(Ok(messages)) => messages,
    };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs() as i64);
    let rows = messages.iter().map(|message| {
        let id = message.id.clone();
        let said = if message.text.is_empty() { "Attachment".to_owned() } else { message.text.clone() };
        div()
            .id(gpui::ElementId::Name(format!("pin-{}", message.id).into()))
            .px(px(theme::PANE_PAD))
            .py(px(8.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .cursor_pointer()
            .hover(|style| style.bg(theme::hover()))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(8.))
                    .child(div().min_w_0().truncate().type_ui().font_weight(theme::SEMIBOLD).child(message.sender.name.clone()))
                    .child(div().flex_none().type_micro().text_color(theme::text_faint()).child(time::stamp(now, message.sent))),
            )
            .child(div().type_ui().text_color(theme::text_soft()).line_clamp(3).child(said))
            .on_click(cx.listener(move |shell, _, _, cx| shell.jump_to_pin(&id, cx)))
    });
    div().id("pins").size_full().overflow_y_scroll().py(px(4.)).children(rows).into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{AccountKey, Network, ThreadSummary};

    fn key(id: &str) -> Key {
        Key::new(&AccountKey { network: Network::Slack, account: "T1".into() }, id)
    }

    #[test]
    fn a_thread_posts_under_its_own_key_and_only_beside_its_conversation() {
        let mut root = Message::plain("m1", "Ines", "root", 0, false);
        assert_eq!(thread_key(&key("C_a"), &root), None);
        root.thread = Some(ThreadSummary { id: "C_a:m1".into(), replies: 2, last: 0, participants: Vec::new(), unread: 0 });
        let thread = thread_key(&key("C_a"), &root).unwrap();
        assert_eq!((thread.id.as_str(), thread.account()), ("C_a:m1", key("C_a").account()));
        // Drawn, composer and all, only while its conversation is the one open.
        assert!(thread_belongs(&key("C_a"), Some(&key("C_a"))));
        assert!(!thread_belongs(&key("C_a"), Some(&key("C_b"))));
        assert!(!thread_belongs(&key("C_a"), None));
    }
}
