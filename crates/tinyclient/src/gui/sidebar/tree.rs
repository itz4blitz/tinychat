//! A Space's tree as the flat list of rows on show, every row 28: a Discord server's channels,
//! a Slack workspace's sections, a team's channels. Rebuilt on collapse and on change. The
//! network's settings are kept here (`design/polish/02-discord-slack.md` §4–5): opt-in and
//! "hide muted channels", muted categories collapsed, only followed threads with news, Slack's
//! "unread only" sections, its quiet channels folded, its apps in their own group. A collapsed
//! group never hides what needs me (rule 3).

use protocol::{Badge, Conversation, ConversationKind, Group, GroupKind, Key, Network, Occupant, Special};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// People listed under a voice channel before `+n`.
pub const OCCUPANTS_SHOWN: usize = 6;
/// Followed threads listed under a channel before `n more threads`.
pub const THREADS_SHOWN: usize = 3;
/// Past this many channels on show, every category without a number starts collapsed.
pub const BIG_TREE: usize = 80;
/// The id of the group Slack's apps are folded into.
pub const APPS_GROUP: &str = "\u{0}apps";

/// What a collapsed Group or a Space says about what is inside it.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Rollup {
    /// The sum of the numbers inside.
    pub attention: u32,
    /// Every one of those numbers is a mention in something muted.
    pub quiet: bool,
    /// Something unmuted inside is unread with no number.
    pub unread: bool,
}

impl Rollup {
    pub fn of<'a>(conversations: impl IntoIterator<Item = &'a Conversation>) -> Rollup {
        let mut rollup = Rollup { quiet: true, ..Rollup::default() };
        for conversation in conversations {
            match conversation.badge() {
                Badge::Count(count) => {
                    rollup.attention += count;
                    rollup.quiet &= conversation.muted;
                }
                Badge::Dot => rollup.unread = true,
                Badge::None => {}
            }
        }
        rollup.quiet &= rollup.attention > 0;
        rollup
    }
}

/// What a fold row reveals when clicked.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Reveal {
    /// "Show 41 muted channels" (Discord's hide-muted).
    Muted,
    /// "Browse 212 more channels" (Discord's opt-in).
    OptedOut,
    /// "12 quiet channels" at the end of a Slack section.
    Quiet(String),
    /// Toggle followed threads under this channel.
    Threads(Key),
}

#[derive(Clone, PartialEq, Debug)]
pub enum Row {
    /// A Slack workspace's Threads: followed threads with new replies.
    Threads { space: Key, count: u32, mention: bool },
    Group { space: Key, id: String, name: String, collapsed: bool, rollup: Rollup, depth: u8 },
    Conversation { conversation: Rc<Conversation>, depth: u8 },
    Occupant { occupant: Occupant, depth: u8 },
    /// `+4` under a voice channel.
    More { words: String, depth: u8 },
    Fold { words: String, reveal: Reveal, depth: u8 },
    /// A Space's rows are still on their way.
    Loading { depth: u8 },
}

impl Row {
    pub fn conversation(&self) -> Option<&Rc<Conversation>> {
        match self {
            Row::Conversation { conversation, .. } => Some(conversation),
            _ => None,
        }
    }
}

/// How a tree is drawn: the network's settings and the user's own reveals this session.
#[derive(Default)]
pub struct Rules<'a> {
    pub network: Option<Network>,
    /// The Space's "hide muted channels", and whether the user revealed them.
    pub hide_muted: bool,
    pub muted_shown: bool,
    pub opted_out_shown: bool,
    /// Groups the user opened against how they start, and closed against it.
    pub opened: Option<&'a HashSet<String>>,
    pub closed: Option<&'a HashSet<String>>,
    /// Slack sections whose quiet channels are shown.
    pub quiet_shown: Option<&'a HashSet<String>>,
    /// Channels whose followed threads are expanded this session.
    pub threads_shown: Option<&'a HashSet<Key>>,
    /// The home's Unread segment: only rows with news, the open one and starred ones.
    pub unread: bool,
}

impl Rules<'_> {
    fn slack(&self) -> bool {
        self.network == Some(Network::Slack)
    }
}

/// Needs me, or is where I am: never hidden by a fold.
fn needs_me(c: &Conversation, open: Option<&Key>) -> bool {
    open == Some(&c.key) || matches!(c.badge(), Badge::Count(_)) || (c.badge() == Badge::Dot && !c.muted)
}

fn occupied(c: &Conversation) -> bool {
    c.kind == ConversationKind::Voice && c.occupants.as_ref().is_some_and(|inside| inside.total > 0)
}

/// One Space's rows: what has no Group first, then each Group under its header, in the
/// network's order. Followed threads with news sit under their channel, a voice channel's
/// occupants under it.
pub fn flatten(space: &Key, groups: &[Group], conversations: &[Rc<Conversation>], open: Option<&Key>, depth: u8, rules: &Rules) -> Vec<Row> {
    let slack = rules.slack();
    let mut rows = Vec::with_capacity(conversations.len() + groups.len());
    let known = |id: &Option<String>| id.as_ref().is_some_and(|id| groups.iter().any(|g| &g.id == id));
    let in_tree = |parent: &String| conversations.iter().any(|other| &other.key.id == parent);
    let is_thread = |c: &Conversation| c.kind == ConversationKind::Thread;
    // Followed threads with news, or the open one, under their channel. Slack keeps its
    // threads in the Threads view.
    let mut threads: HashMap<&str, Vec<&Rc<Conversation>>> = HashMap::new();
    let mut quiet_threads: HashMap<&str, usize> = HashMap::new();
    for thread in conversations.iter().filter(|c| is_thread(c) && c.parent.as_ref().is_some_and(in_tree)) {
        let parent = thread.parent.as_deref().unwrap_or_default();
        let expanded = rules.threads_shown.is_some_and(|shown| shown.contains(&Key { id: parent.into(), ..space.clone() }));
        let shown = open == Some(&thread.key) || (!slack && thread.followed && (expanded || thread.badge() != Badge::None));
        if shown {
            threads.entry(parent).or_default().push(thread);
        } else if !slack && thread.followed {
            *quiet_threads.entry(parent).or_default() += 1;
        }
    }
    let (mut muted_hidden, mut opted_hidden) = (0usize, 0usize);
    let mut visible = |c: &Conversation| {
        if needs_me(c, open) || c.starred {
            return true;
        }
        if c.opted_out && !rules.opted_out_shown {
            opted_hidden += 1;
            return false;
        }
        if c.muted && rules.hide_muted && !rules.muted_shown {
            muted_hidden += 1;
            return false;
        }
        true
    };
    let listed: Vec<&Rc<Conversation>> = conversations.iter().filter(|c| !is_thread(c) && visible(c)).collect();

    let emit = |rows: &mut Vec<Row>, members: &[&Rc<Conversation>], depth: u8, collapsed: bool| {
        for conversation in members {
            let under = threads.get(conversation.key.id.as_str()).map(Vec::as_slice).unwrap_or_default();
            if collapsed && !needs_me(conversation, open) && !occupied(conversation) {
                // A thread that needs me shows its channel, so it is not orphaned.
                if !under.iter().any(|thread| needs_me(thread, open)) {
                    continue;
                }
            }
            rows.push(Row::Conversation { conversation: Rc::clone(conversation), depth });
            let expanded = rules.threads_shown.is_some_and(|shown| shown.contains(&conversation.key));
            for (ix, thread) in under.iter().enumerate() {
                if expanded || ix < THREADS_SHOWN || open == Some(&thread.key) {
                    rows.push(Row::Conversation { conversation: Rc::clone(thread), depth: depth + 1 });
                }
            }
            let more = under.iter().enumerate().filter(|(ix, thread)| *ix >= THREADS_SHOWN && open != Some(&thread.key)).count() + quiet_threads.get(conversation.key.id.as_str()).copied().unwrap_or(0);
            if !slack && !collapsed && (more > 0 || expanded) {
                let words = if expanded { "Show fewer threads".to_owned() } else if more == 1 { "1 more thread".to_owned() } else { format!("{more} more threads") };
                rows.push(Row::Fold { words, reveal: Reveal::Threads(conversation.key.clone()), depth: depth + 1 });
            }
            if let Some(inside) = conversation.occupants.as_ref().filter(|_| occupied(conversation)) {
                rows.extend(inside.people.iter().take(OCCUPANTS_SHOWN).map(|occupant| Row::Occupant { occupant: occupant.clone(), depth: depth + 1 }));
                let more = (inside.total as usize).saturating_sub(OCCUPANTS_SHOWN.min(inside.people.len()));
                if more > 0 {
                    rows.push(Row::More { words: format!("+{more}"), depth: depth + 1 });
                }
            }
        }
    };

    let big = listed.len() > BIG_TREE;
    let loose: Vec<&Rc<Conversation>> = listed.iter().copied().filter(|c| !known(&c.group) && (!rules.unread || needs_me(c, open) || c.starred)).collect();
    emit(&mut rows, &loose, depth, false);
    let mut ordered: Vec<&Group> = groups.iter().collect();
    ordered.sort_by_key(|group| group.position);
    let mut apps: Vec<&Rc<Conversation>> = Vec::new();
    for group in ordered {
        let mut members: Vec<&Rc<Conversation>> = listed.iter().copied().filter(|c| c.group.as_deref() == Some(&group.id)).collect();
        if slack {
            apps.extend(members.iter().copied().filter(|c| c.bot));
            members.retain(|c| !c.bot);
            if group.kind == GroupKind::Directs {
                // Direct messages by recency, the user's own first.
                members.sort_by_key(|c| (c.special != Special::Saved, std::cmp::Reverse(c.updated)));
            } else {
                // A to Z without the `#`, muted last (Slack's own order).
                members.sort_by_cached_key(|c| (c.muted, c.kind.is_direct(), c.title.trim_start_matches('#').to_lowercase()));
            }
            if group.unread_only {
                members.retain(|c| needs_me(c, open) || c.starred);
            }
        }
        if rules.unread {
            members.retain(|c| needs_me(c, open) || c.starred);
        }
        let quiet: Vec<&Rc<Conversation>> = if slack && !rules.quiet_shown.is_some_and(|shown| shown.contains(&group.id)) {
            let (quiet, kept): (Vec<_>, Vec<_>) = members.iter().copied().partition(|c| c.quiet && !needs_me(c, open) && !c.starred);
            members = kept;
            quiet
        } else {
            Vec::new()
        };
        if members.is_empty() && quiet.is_empty() {
            continue;
        }
        let inside = members.iter().map(|c| &***c).chain(threads.values().flatten().filter(|t| members.iter().any(|m| Some(&m.key.id) == t.parent.as_ref())).map(|t| &***t));
        let rollup = Rollup::of(inside);
        let starts_collapsed = group.muted || (big && rollup.attention == 0);
        let collapsed = if rules.opened.is_some_and(|opened| opened.contains(&group.id)) {
            false
        } else if rules.closed.is_some_and(|closed| closed.contains(&group.id)) {
            true
        } else {
            group.collapsed || starts_collapsed
        };
        rows.push(Row::Group {
            space: space.clone(),
            id: group.id.clone(),
            name: group.name.clone(),
            collapsed,
            rollup: if collapsed { rollup } else { Rollup::default() },
            depth,
        });
        emit(&mut rows, &members, depth, collapsed);
        if !quiet.is_empty() && !collapsed {
            let words = if quiet.len() == 1 { "1 quiet channel".to_owned() } else { format!("{} quiet channels", quiet.len()) };
            rows.push(Row::Fold { words, reveal: Reveal::Quiet(group.id.clone()), depth });
        }
    }
    if !apps.is_empty() {
        apps.sort_by_key(|app| std::cmp::Reverse(app.updated));
        let opened = rules.opened.is_some_and(|opened| opened.contains(APPS_GROUP));
        let rollup = Rollup::of(apps.iter().map(|c| &***c));
        rows.push(Row::Group { space: space.clone(), id: APPS_GROUP.into(), name: "Apps".into(), collapsed: !opened, rollup: if opened { Rollup::default() } else { rollup }, depth });
        // Folded apps stay folded unless one names me: their plain unread is the header's dot.
        if !opened {
            apps.retain(|app| app.mentions > 0 || open == Some(&app.key));
        }
        emit(&mut rows, &apps, depth, false);
    }
    if muted_hidden > 0 {
        let words = if muted_hidden == 1 { "Show 1 muted channel".to_owned() } else { format!("Show {muted_hidden} muted channels") };
        rows.push(Row::Fold { words, reveal: Reveal::Muted, depth });
    }
    if opted_hidden > 0 {
        let words = if opted_hidden == 1 { "Browse 1 more channel".to_owned() } else { format!("Browse {opted_hidden} more channels") };
        rows.push(Row::Fold { words, reveal: Reveal::OptedOut, depth });
    }
    rows
}

/// The rows after `from` (or before it, going up) that a keyboard walk may land on: the next
/// conversation, or the next unread one.
pub fn walk(rows: &[Row], from: Option<usize>, down: bool, unread_only: bool) -> Option<usize> {
    let wanted = |row: &Row| row.conversation().is_some_and(|c| !unread_only || c.badge() != Badge::None);
    if down {
        let start = from.map_or(0, |from| from + 1);
        (start..rows.len()).find(|ix| wanted(&rows[*ix]))
    } else {
        let end = from.unwrap_or(rows.len());
        (0..end).rev().find(|ix| wanted(&rows[*ix]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> Key {
        Key { network: Network::Discord, account: "me".into(), id: id.into() }
    }

    fn channel(id: &str, group: Option<&str>, kind: ConversationKind) -> Conversation {
        let mut c = Conversation::new(key(id), kind, id);
        c.space = Some("s".into());
        c.group = group.map(str::to_owned);
        c
    }

    fn group(id: &str, position: i32, collapsed: bool) -> Group {
        Group { collapsed, ..Group::new(id, id, position) }
    }

    /// One word per row, so a whole tree reads as a line.
    fn shape(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Group { id, collapsed, rollup, .. } => {
                    let mark = if *collapsed { ">" } else { "v" };
                    let id = if id == APPS_GROUP { "apps" } else { id };
                    match (rollup.attention, rollup.unread) {
                        (0, false) => format!("{mark}{id}"),
                        (0, true) => format!("{mark}{id}*"),
                        (count, _) => format!("{mark}{id}({count}{})", if rollup.quiet { "q" } else { "" }),
                    }
                }
                Row::Conversation { conversation, depth } => format!("{}{}", ".".repeat(*depth as usize), conversation.key.id),
                Row::Occupant { occupant, depth } => format!("{}@{}", ".".repeat(*depth as usize), occupant.name),
                Row::More { words, .. } | Row::Fold { words, .. } => words.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    fn rows_with(groups: &[Group], conversations: Vec<Conversation>, open: Option<&str>, rules: &Rules) -> Vec<String> {
        let conversations: Vec<Rc<Conversation>> = conversations.into_iter().map(Rc::new).collect();
        let open = open.map(key);
        shape(&flatten(&key("s"), groups, &conversations, open.as_ref(), 0, rules))
    }

    fn rows(groups: &[Group], conversations: Vec<Conversation>, open: Option<&str>) -> Vec<String> {
        rows_with(groups, conversations, open, &Rules::default())
    }

    #[test]
    fn groups_come_in_the_networks_order_with_loose_rows_first() {
        use ConversationKind::Channel;
        let tree = rows(
            &[group("voice", 2, false), group("text", 1, false), group("empty", 0, false)],
            vec![
                channel("rules", None, Channel),
                channel("lounge", Some("voice"), ConversationKind::Voice),
                channel("general", Some("text"), Channel),
                channel("help", Some("text"), Channel),
                channel("orphan", Some("gone"), Channel),
            ],
            None,
        );
        assert_eq!(tree, ["rules", "orphan", "vtext", "general", "help", "vvoice", "lounge"]);
    }

    #[test]
    fn a_collapsed_group_keeps_what_needs_me_and_says_what_else_is_inside() {
        use ConversationKind::Channel;
        let mut mentioned = channel("design", Some("text"), Channel);
        (mentioned.unread, mentioned.mentions) = (4, 2);
        let mut unread = channel("general", Some("text"), Channel);
        unread.unread = 1;
        let mut lounge = channel("lounge", Some("text"), ConversationKind::Voice);
        lounge.occupants = Some(protocol::Occupants { people: Vec::new(), total: 2 });
        let mut muted = channel("bots", Some("quiet"), Channel);
        (muted.muted, muted.unread) = (true, 9);
        let groups = [group("text", 0, true), group("quiet", 1, true)];
        let all = vec![mentioned, unread, channel("help", Some("text"), Channel), lounge, muted];
        // The mention, the unread channel, voice with people and the open row stay.
        assert_eq!(rows(&groups, all, Some("help")), [">text(2)", "design", "general", "help", "lounge", "+2", ">quiet"]);
    }

    #[test]
    fn muted_categories_and_quiet_categories_of_a_big_server_start_collapsed_until_opened() {
        use ConversationKind::Channel;
        let mut muted = group("muted", 0, false);
        muted.muted = true;
        let tree = vec![channel("a", Some("muted"), Channel), channel("b", Some("text"), Channel)];
        assert_eq!(rows(&[muted.clone(), group("text", 1, false)], tree.clone(), None), [">muted", "vtext", "b"]);
        let opened = HashSet::from(["muted".to_owned()]);
        let rules = Rules { opened: Some(&opened), ..Rules::default() };
        assert_eq!(rows_with(&[muted, group("text", 1, false)], tree, None, &rules), ["vmuted", "a", "vtext", "b"]);

        let mut many: Vec<Conversation> = (0..90).map(|n| channel(&format!("c{n}"), Some("big"), Channel)).collect();
        many[3].mentions = 1;
        many.push(channel("x", Some("other"), Channel));
        let got = rows(&[group("big", 0, false), group("other", 1, false)], many, None);
        // The category with a number stays open; the one without starts collapsed.
        assert_eq!((got[0].as_str(), got.len()), ("vbig", 92));
        assert_eq!(got.last().map(String::as_str), Some(">other"));
    }

    #[test]
    fn only_followed_threads_with_news_show_three_then_a_count() {
        use ConversationKind::{Channel, Thread};
        let mut all = vec![channel("general", None, Channel)];
        for n in 0..6 {
            let mut thread = channel(&format!("t{n}"), None, Thread);
            thread.parent = Some("general".into());
            thread.followed = n < 5;
            thread.unread = u32::from(n < 4);
            all.push(thread);
        }
        // t0–t3 have news, t4 is followed and read, t5 was never joined.
        assert_eq!(rows(&[], all.clone(), None), ["general", ".t0", ".t1", ".t2", "2 more threads"]);
        assert_eq!(rows(&[], all, Some("t5")), ["general", ".t0", ".t1", ".t2", ".t5", "2 more threads"]);
    }

    #[test]
    fn expanding_threads_reveals_read_and_overflow_threads_only_for_that_channel() {
        use ConversationKind::{Channel, Thread};
        let mut all = vec![channel("general", None, Channel), channel("other", None, Channel)];
        for n in 0..7 {
            let mut thread = channel(&format!("t{n}"), None, Thread);
            thread.parent = Some(if n == 6 { "other" } else { "general" }.into());
            thread.followed = n != 5;
            thread.unread = u32::from(n < 4);
            all.push(thread);
        }
        let shown = HashSet::from([key("general")]);
        let rules = Rules { threads_shown: Some(&shown), ..Rules::default() };
        assert_eq!(rows_with(&[], all.clone(), None, &rules), ["general", ".t0", ".t1", ".t2", ".t3", ".t4", "Show fewer threads", "other", "1 more thread"]);
        // Collapsing keeps an open overflow thread visible, and hides read threads again.
        assert_eq!(rows(&[], all.clone(), Some("t3")), ["general", ".t0", ".t1", ".t2", ".t3", "1 more thread", "other", "1 more thread"]);
        let foreign = HashSet::from([Key { account: "another-account".into(), ..key("general") }]);
        let rules = Rules { threads_shown: Some(&foreign), ..Rules::default() };
        assert_eq!(rows_with(&[], all, None, &rules), ["general", ".t0", ".t1", ".t2", "2 more threads", "other", "1 more thread"]);
    }

    #[test]
    fn one_read_thread_can_be_revealed_and_its_fold_targets_the_parent() {
        use ConversationKind::{Channel, Thread};
        let mut thread = channel("read", None, Thread);
        thread.parent = Some("general".into());
        thread.followed = true;
        let all = vec![Rc::new(channel("general", None, Channel)), Rc::new(thread)];
        let collapsed = flatten(&key("s"), &[], &all, None, 0, &Rules::default());
        assert_eq!(shape(&collapsed), ["general", "1 more thread"]);
        assert!(matches!(&collapsed[1], Row::Fold { reveal: Reveal::Threads(parent), .. } if parent == &key("general")));
        let shown = HashSet::from([key("general")]);
        let rules = Rules { threads_shown: Some(&shown), ..Rules::default() };
        let expanded = flatten(&key("s"), &[], &all, None, 0, &rules);
        assert_eq!(shape(&expanded), ["general", ".read", "Show fewer threads"]);
        assert!(matches!(&expanded[2], Row::Fold { reveal: Reveal::Threads(parent), .. } if parent == &key("general")));
    }

    #[test]
    fn hide_muted_and_opt_in_hide_rows_behind_footers_that_reveal_them() {
        use ConversationKind::Channel;
        let mut muted = channel("muted", None, Channel);
        muted.muted = true;
        let mut pinged = channel("pinged", None, Channel);
        (pinged.muted, pinged.mentions) = (true, 1);
        let mut skipped = channel("skipped", None, Channel);
        skipped.opted_out = true;
        let all = vec![channel("general", None, Channel), muted, pinged, skipped];
        let hidden = Rules { hide_muted: true, ..Rules::default() };
        assert_eq!(rows_with(&[], all.clone(), None, &hidden), ["general", "pinged", "Show 1 muted channel", "Browse 1 more channel"]);
        let shown = Rules { hide_muted: true, muted_shown: true, opted_out_shown: true, ..Rules::default() };
        assert_eq!(rows_with(&[], all, None, &shown), ["general", "muted", "pinged", "skipped"]);
    }

    #[test]
    fn slack_sorts_its_sections_folds_quiet_channels_and_keeps_apps_and_threads_apart() {
        use ConversationKind::{Channel, Direct, Thread};
        let slack = |mut c: Conversation| {
            c.key.network = Network::Slack;
            c
        };
        let mut directs = group("dms", 2, false);
        directs.kind = GroupKind::Directs;
        let mut unread_only = group("starred", 0, false);
        unread_only.unread_only = true;
        let mut quiet = channel("old", Some("chan"), Channel);
        quiet.quiet = true;
        let mut muted = channel("alpha", Some("chan"), Channel);
        muted.muted = true;
        let mut mara = channel("mara", Some("dms"), Direct);
        mara.updated = 5;
        let mut dev = channel("dev", Some("dms"), Direct);
        dev.updated = 9;
        let mut me = channel("me", Some("dms"), Direct);
        me.special = Special::Saved;
        let mut github = channel("github", Some("dms"), Direct);
        (github.bot, github.unread) = (true, 1);
        let mut thread = channel("thread", Some("chan"), Thread);
        thread.parent = Some("zeta".into());
        let mut read = channel("read", Some("starred"), Channel);
        read.unread = 0;
        let mut news = channel("news", Some("starred"), Channel);
        news.unread = 1;
        let all = [read, news, channel("zeta", Some("chan"), Channel), muted, quiet, channel("beta", Some("chan"), Channel), mara, dev, me, github, thread];
        let rules = Rules { network: Some(Network::Slack), ..Rules::default() };
        let got = rows_with(&[unread_only, group("chan", 1, false), directs], all.into_iter().map(slack).collect(), None, &rules);
        assert_eq!(got, ["vstarred", "news", "vchan", "beta", "zeta", "alpha", "1 quiet channel", "vdms", "me", "dev", "mara", ">apps*"]);
    }

    #[test]
    fn the_unread_walk_skips_headers_and_read_rows_in_both_directions() {
        use ConversationKind::Channel;
        let unread = |id: &str| {
            let mut row = channel(id, Some("text"), Channel);
            row.unread = 1;
            row
        };
        let conversations: Vec<Rc<Conversation>> =
            [channel("a", Some("text"), Channel), unread("b"), channel("c", Some("text"), Channel), unread("d")].into_iter().map(Rc::new).collect();
        let rows = flatten(&key("s"), &[group("text", 0, false)], &conversations, None, 0, &Rules::default());
        assert_eq!(walk(&rows, None, true, false), Some(1));
        assert_eq!(walk(&rows, None, true, true), Some(2));
        assert_eq!(walk(&rows, Some(2), true, true), Some(4));
        assert_eq!(walk(&rows, Some(4), true, true), None);
        assert_eq!(walk(&rows, Some(4), false, true), Some(2));
        assert_eq!(walk(&rows, Some(1), false, false), None);
    }
}
