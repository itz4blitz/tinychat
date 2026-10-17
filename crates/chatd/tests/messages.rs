//! The Messages adapter through chatd's real socket, with a scripted phone
//! (`adapters/messages/fake.rs`). It proves what the adapter asks for and how that maps into
//! the contract. It proves nothing about Google's relay, which needs a real phone.

use protocol::{
    read_frame, write_frame, AccountKey, Change, ChatStream, ConversationKind, Key, Link,
    MediaKind, Message, MessageKind, Network, Outgoing, OutgoingFile, Page, ReactionSet, Request,
    Response, SendState, SignInInput, SignInProblem, SignInStep,
};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

const ME: &str = "phone-ada";
const GRACE: &str = "phone-grace";
const CLUB: &str = "phone-club";
const SESSION: &str = r#"{"account":"phone-ada","name":"Ada Quill"}"#;

fn key(id: &str) -> Key {
    Key {
        network: Network::Messages,
        account: ME.into(),
        id: id.into(),
    }
}

fn account() -> AccountKey {
    AccountKey {
        network: Network::Messages,
        account: ME.into(),
    }
}

struct World {
    child: Child,
    dir: PathBuf,
}

impl World {
    fn linked() -> World {
        World::start(Some(SESSION))
    }

    fn start(session: Option<&str>) -> World {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::path::absolute(format!(
            "/tmp/tinyclient-msg-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = std::fs::File::create(dir.join("stderr.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_chatd"));
        command
            .env("TINYCLIENT_DIR", &dir)
            .env("TINYCLIENT_MESSAGES_FAKE", "1")
            .env_remove("TINYCLIENT_SESSION_DISCORD")
            .env_remove("TINYCLIENT_SESSION_SLACK")
            .env_remove("TINYCLIENT_SESSION_TELEGRAM")
            .env_remove("TINYCLIENT_SESSION_TEAMS")
            .env_remove("TINYCLIENT_SESSION_SIGNAL")
            .env_remove("TINYCLIENT_SESSION_MESSAGES")
            .stderr(Stdio::from(log));
        if let Some(session) = session {
            command.env("TINYCLIENT_SESSION_MESSAGES", session);
        }
        let child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while ChatStream::connect(&dir.join("chat.sock")).is_err() {
            assert!(Instant::now() < deadline, "chatd did not open its socket");
            sleep(Duration::from_millis(10));
        }
        World { child, dir }
    }

    fn ask(&self, request: Request) -> Response {
        let mut stream = ChatStream::connect(&self.dir.join("chat.sock")).unwrap();
        write_frame(&mut stream, &request).unwrap();
        read_frame(&mut stream).unwrap()
    }

    fn until<T>(&self, what: &str, mut look: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(found) = look() {
                return found;
            }
            if Instant::now() > deadline {
                panic!(
                    "waited for {what}; Messages was asked: {:#?}; chatd said: {}",
                    self.calls(),
                    self.stderr()
                );
            }
            sleep(Duration::from_millis(30));
        }
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("messages.calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn heard(&self, line: &str) {
        self.until(&format!("Messages to hear {line:?}"), || {
            self.calls().iter().any(|call| call == line).then_some(())
        });
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(self.dir.join("stderr.log")).unwrap_or_default()
    }

    fn steer(&self, line: &str) {
        self.write_to("messages.ctl", line);
    }

    fn steer_link(&self, line: &str) {
        self.write_to("messages.signin.ctl", line);
    }

    fn write_to(&self, control: &str, line: &str) {
        let path = self.dir.join(control);
        self.until("the fake to listen", || path.exists().then_some(()));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{line}").unwrap();
        drop(file);
        let end = std::fs::metadata(&path).unwrap().len();
        let mut taken = path.clone().into_os_string();
        taken.push(".taken");
        let taken = PathBuf::from(taken);
        let read = || {
            std::fs::read_to_string(&taken)
                .ok()
                .and_then(|at| at.trim().parse::<u64>().ok())
                .unwrap_or(0)
        };
        self.until("the fake to take the line", || {
            (read() >= end).then_some(())
        });
    }

    fn status(&self) -> (Vec<protocol::AccountStatus>, u64) {
        match self.ask(Request::Status) {
            Response::Status { accounts, seq, .. } => (accounts, seq),
            other => panic!("{other:?}"),
        }
    }

    fn connected(&self) {
        self.until("Messages to connect", || {
            self.status()
                .0
                .iter()
                .any(|account| account.link == Link::Connected)
                .then_some(())
        });
        self.until("the group", || self.conversation(CLUB).map(drop));
        self.heard("list");
    }

    fn conversation(&self, id: &str) -> Option<protocol::Conversation> {
        match self.ask(Request::Conversation {
            conversation: key(id),
        }) {
            Response::Conversation { conversation } => Some(conversation),
            _ => None,
        }
    }

    fn messages(&self, conversation: &str) -> Vec<Message> {
        match self.ask(Request::Messages {
            conversation: key(conversation),
            page: Page::Newest,
            limit: 100,
        }) {
            Response::Messages { messages, .. } => messages,
            _ => Vec::new(),
        }
    }

    fn arrived(&self, conversation: &str, text: &str) -> Message {
        self.until(&format!("message {text:?} in {conversation}"), || {
            self.messages(conversation)
                .into_iter()
                .find(|message| message.text == text)
        })
    }

    fn send(&self, conversation: &str, client_id: &str, text: &str) -> Message {
        let asked = Request::Send {
            conversation: key(conversation),
            client_id: client_id.into(),
            text: text.into(),
            reply_to: None,
            with: Default::default(),
        };
        match self.ask(asked) {
            Response::Sent { message } => message,
            other => panic!("{other:?}"),
        }
    }

    fn changed<T>(
        &self,
        mut since: u64,
        what: &str,
        mut find: impl FnMut(&Change) -> Option<T>,
    ) -> T {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                Instant::now() < deadline,
                "no change: {what}; chatd said: {}",
                self.stderr()
            );
            let Response::Changes { seq, changes, .. } = self.ask(Request::Wait { since }) else {
                panic!("no changes");
            };
            since = seq;
            if let Some(found) = changes.iter().find_map(&mut find) {
                return found;
            }
        }
    }

    fn sign_in(&self, input: Option<SignInInput>) -> SignInStep {
        let request = match input {
            None => Request::SignInBegin {
                network: Network::Messages,
            },
            Some(input) => Request::SignInAnswer {
                network: Network::Messages,
                input,
            },
        };
        match self.ask(request) {
            Response::SignIn {
                network: Network::Messages,
                step,
            } => step,
            other => panic!("{other:?}"),
        }
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[test]
fn a_paired_phone_lists_its_chats_and_a_text_arrives() {
    let world = World::linked();
    world.connected();
    let (accounts, _) = world.status();
    let mine = accounts
        .iter()
        .find(|status| status.key == account())
        .expect("the Messages account");
    assert_eq!((mine.name.as_str(), mine.me.as_str()), ("Ada Quill", ME));
    let club = world.conversation(CLUB).unwrap();
    assert_eq!(
        (club.kind, club.title.as_str(), club.unread),
        (ConversationKind::Group, "Bridge club", 1)
    );
    let grace = world.conversation(GRACE).unwrap();
    assert_eq!(
        (grace.kind, grace.title.as_str()),
        (ConversationKind::Direct, "Grace Ombra")
    );

    world.steer("incoming grace Are we still on for Tuesday?");
    let first = world.arrived(GRACE, "Are we still on for Tuesday?");
    assert_eq!(
        (first.sender.name.as_str(), first.outgoing),
        ("Grace Ombra", false)
    );

    let pending = world.send(GRACE, "1001", "Yes, seven sharp.");
    assert!(matches!(pending.state, SendState::Pending), "{pending:?}");
    world.heard(&format!("send {GRACE} Yes, seven sharp."));

    world.steer("photo grace");
    let photo = world.until("the photo", || {
        world.messages(GRACE).into_iter().find(|message| {
            message
                .attachments
                .iter()
                .any(|file| file.name == "photo.png")
        })
    });
    assert_eq!(photo.attachments.len(), 1);
    let file = &photo.attachments[0].file;
    assert_eq!(file.kind, MediaKind::Image);
    let path = file.path.as_deref().expect("the photo is on disk");
    assert_eq!(&std::fs::read(path).unwrap()[..8], b"\x89PNG\r\n\x1a\n");

    let Response::Reactions { rules, .. } = world.ask(Request::Reactions {
        conversation: key(GRACE),
    }) else {
        panic!("no rules");
    };
    assert_eq!(
        (rules.set, rules.mine, rules.replaces),
        (ReactionSet::Any, 1, true)
    );
}

#[test]
fn a_named_file_stays_a_line_a_caption_keeps_the_photo_and_a_later_file_is_fetched() {
    let world = World::linked();
    world.connected();
    world.steer("file grace report.pdf");
    let named = world.arrived(GRACE, "report.pdf");
    match named.kind {
        MessageKind::Unsupported { link } => assert!(link.contains(GRACE), "{link}"),
        other => panic!("{other:?}"),
    }
    assert!(named.attachments.is_empty());

    world.steer("caption grace look");
    let caption = world.arrived(GRACE, "look");
    assert_eq!(caption.attachments[0].file.kind, MediaKind::Image);
    assert!(caption.attachments[0].file.path.is_some());

    world.steer("remote grace");
    let remote = world.until("the remote photo", || {
        world
            .messages(GRACE)
            .into_iter()
            .find(|message| message.id.contains("remote"))
    });
    assert!(remote.attachments[0].file.path.is_none());
    let id = remote.attachments[0].file.id.clone();
    let _ = world.ask(Request::Fetch {
        account: account(),
        ids: vec![id.clone()],
        size: None,
        retry: false,
    });
    world.heard(&format!("fetch {} fake-photo", remote.id));
    world.until("the file on disk", || {
        match world.ask(Request::Fetch {
            account: account(),
            ids: vec![id.clone()],
            size: None,
            retry: false,
        }) {
            Response::Media { files } => files
                .into_iter()
                .find(|file| file.id == id)
                .and_then(|file| file.path)
                .filter(|path| std::fs::read(path).unwrap().starts_with(b"\x89PNG")),
            _ => None,
        }
    });
}

#[test]
fn a_reaction_is_shown_and_one_we_send_is_asked_of_the_phone() {
    let world = World::linked();
    world.connected();
    world.steer("react grace Thanks");
    let message = world.arrived(GRACE, "Thanks");
    assert_eq!(message.reactions.len(), 1);
    assert_eq!(
        (
            message.reactions[0].emoji.as_str(),
            message.reactions[0].count,
            message.reactions[0].mine
        ),
        ("👍", 1, false)
    );
    match world.ask(Request::React {
        conversation: key(GRACE),
        message_id: message.id.clone(),
        emoji: "😂".into(),
        on: true,
    }) {
        Response::Ok => {}
        other => panic!("{other:?}"),
    }
    world.heard(&format!("react {} 😂 1", message.id));
    match world.ask(Request::React {
        conversation: key(GRACE),
        message_id: message.id.clone(),
        emoji: "😂".into(),
        on: false,
    }) {
        Response::Ok => {}
        other => panic!("{other:?}"),
    }
    world.heard(&format!("react {} 😂 0", message.id));
}

#[test]
fn a_file_we_send_is_read_and_shown_back() {
    let world = World::linked();
    world.connected();
    let dir = world.dir.join("outbox");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("note.txt");
    std::fs::write(&path, b"hello").unwrap();
    let asked = Request::Send {
        conversation: key(GRACE),
        client_id: "file-1".into(),
        text: "see this".into(),
        reply_to: None,
        with: Outgoing {
            files: vec![OutgoingFile {
                path: path.to_string_lossy().into_owned(),
                name: "note.txt".into(),
                spoiler: false,
                as_file: false,
                original: false,
            }],
            ..Outgoing::default()
        },
    };
    match world.ask(asked) {
        Response::Sent { message } => {
            assert!(matches!(message.state, SendState::Pending), "{message:?}")
        }
        other => panic!("{other:?}"),
    }
    world.heard(&format!("file {GRACE} note.txt 5 see this"));
    let sent = world.until("the sent file", || {
        world.messages(GRACE).into_iter().find(|message| {
            message.text == "see this"
                && message
                    .attachments
                    .iter()
                    .any(|file| file.name == "note.txt")
        })
    });
    let path = sent.attachments[0].file.path.as_deref().expect("sent file");
    assert_eq!(std::fs::read(path).unwrap(), b"hello");
}

#[test]
fn pairing_shows_a_code_replaces_it_and_scanning_it_signs_in() {
    let world = World::start(None);
    assert!(world
        .status()
        .0
        .iter()
        .all(|status| status.key.network != Network::Messages || status.link == Link::SignedOut));
    let (_, seq) = world.status();
    assert!(matches!(world.sign_in(None), SignInStep::Qr { .. }));
    let first = world.until("a code", || match world.sign_in(None) {
        SignInStep::Qr {
            link,
            problem: None,
            ..
        } if !link.is_empty() => Some(link),
        _ => None,
    });
    assert!(
        first.starts_with("https://support.google.com/messages/"),
        "{first}"
    );
    let typed = world.sign_in(Some(SignInInput::UsePhone));
    assert!(
        matches!(&typed, SignInStep::Qr { link, problem: Some(SignInProblem::Other { .. }), .. } if *link == first),
        "{typed:?}"
    );
    world.steer_link("expire");
    let second = world.changed(seq, "a fresh code", |change| match change {
        Change::SignIn {
            network: Network::Messages,
            step: SignInStep::Qr { link, .. },
        } if !link.is_empty() && *link != first => Some(link.clone()),
        _ => None,
    });
    assert_ne!(first, second);

    let (_, seq) = world.status();
    world.steer_link("scan");
    let done = world.changed(seq, "the scan", |change| match change {
        Change::SignIn {
            network: Network::Messages,
            step: SignInStep::Done { account },
        } => Some(account.clone()),
        _ => None,
    });
    assert_eq!(done, Some(account()));
    world.connected();
}
