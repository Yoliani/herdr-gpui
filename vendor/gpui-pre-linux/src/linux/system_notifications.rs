//! System notifications over the XDG notifications D-Bus interface.
//!
//! All notifications share ONE session-bus connection. The connection is
//! opened lazily on the first post, its `ActionInvoked` and
//! `NotificationClosed` match rules are registered before the first `Notify`
//! is sent, and a single reader thread dispatches those signals by
//! notification id. `Notify` calls run on a dedicated poster thread, so
//! posting never blocks the caller.
//!
//! This shape replaces the previous one-connection-and-one-thread per
//! notification: a daemon that closes a notification without an observable
//! signal (instant close, history-only or do-not-disturb delivery), or never
//! closes it, used to pin that connection and its waiting thread for the
//! lifetime of the process. With a shared connection and a bounded signal
//! buffer, the worst case is a few hundred bytes of bookkeeping, never a
//! leaked connection.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    ForegroundExecutor, SharedString, SystemNotification, SystemNotificationResponse, Task,
};
use zbus::MatchRule;

/// The XDG action key invoked when the user activates the notification body
/// rather than a specific action button.
const DEFAULT_ACTION: &str = "default";
/// Transport prefix for caller-provided actions, mirroring notify-rust's scheme.
const ACTION_PREFIX: &str = "gpui-action-";
const NOTIFICATIONS_DEST: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const NOTIFICATIONS_INTERFACE: &str = "org.freedesktop.Notifications";
/// In-flight notifications remembered for responses. A notification whose
/// signals never arrive would otherwise pin its entry forever; the cap bounds
/// it the same way the click registry bounds tags.
const INFLIGHT_LIMIT: usize = 1024;
/// Signals kept for ids whose `Notify` reply has not landed yet. The daemon
/// can signal an id (assign it, then close or activate it) before the client
/// sees the reply; buffering keeps such clicks alive without letting a reply
/// that never arrives pin anything.
const EARLY_LIMIT: usize = 64;

type ResponseCallback = Rc<RefCell<Option<Box<dyn FnMut(SystemNotificationResponse)>>>>;

/// Everything the reader needs to answer signals for one notification.
struct Inflight {
    tag: SharedString,
    action_ids: HashMap<String, SharedString>,
}

/// A signal that arrived before the `Notify` reply recorded its id.
enum Early {
    Action(String),
    Closed,
}

#[derive(Default)]
struct InflightTable {
    by_id: HashMap<u32, Inflight>,
    order: VecDeque<u32>,
    early: HashMap<u32, Early>,
}

impl InflightTable {
    fn insert(&mut self, id: u32, entry: Inflight) {
        if self.by_id.remove(&id).is_some() {
            // A restarted daemon can reuse ids; drop the stale queue position
            // so eviction stays oldest-first and `order` holds each id once.
            self.order.retain(|queued| *queued != id);
        }
        while self.by_id.len() >= INFLIGHT_LIMIT {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.by_id.remove(&oldest);
        }
        self.by_id.insert(id, entry);
        self.order.push_back(id);
    }

    fn remove(&mut self, id: u32) -> Option<Inflight> {
        let entry = self.by_id.remove(&id)?;
        self.order.retain(|queued| *queued != id);
        Some(entry)
    }

    /// Keeps a signal for an id the poster has not recorded yet. The first
    /// arrival wins: an activation followed by a close must not lose the
    /// click.
    fn remember_early(&mut self, id: u32, signal: Early) {
        if self.early.len() >= EARLY_LIMIT {
            // Safety valve for ids whose reply never lands; the victim is
            // arbitrary.
            if let Some(&victim) = self.early.keys().next() {
                self.early.remove(&victim);
            }
        }
        self.early.entry(id).or_insert(signal);
    }

    fn take_early(&mut self, id: u32) -> Option<Early> {
        self.early.remove(&id)
    }

    fn clear(&mut self) {
        self.by_id.clear();
        self.order.clear();
        self.early.clear();
    }
}

/// One posted notification waiting to be sent.
struct Post {
    app_name: Option<String>,
    title: SharedString,
    body: SharedString,
    actions: Vec<String>,
    tag: SharedString,
    action_ids: HashMap<String, SharedString>,
}

enum WorkerPost {
    Notify(Post),
    /// The reader lost the connection; drop it so the next post reconnects.
    Reset,
}

type PostSender = mpsc::UnboundedSender<WorkerPost>;
type PostReceiver = mpsc::UnboundedReceiver<WorkerPost>;
type ResponseSender = mpsc::UnboundedSender<SystemNotificationResponse>;

pub(crate) struct SystemNotificationState {
    response_sender: ResponseSender,
    response_receiver: Option<mpsc::UnboundedReceiver<SystemNotificationResponse>>,
    callback: ResponseCallback,
    _response_task: Option<Task<()>>,
    /// `Some` once the poster thread exists; posting then never touches the
    /// bus on the calling thread. Cleared when the worker is gone so the
    /// next post starts a new one.
    post_sender: Mutex<Option<PostSender>>,
}

impl SystemNotificationState {
    pub(crate) fn new() -> Self {
        let (response_sender, response_receiver) = mpsc::unbounded();
        Self {
            response_sender,
            response_receiver: Some(response_receiver),
            callback: Rc::new(RefCell::new(None)),
            _response_task: None,
            post_sender: Mutex::new(None),
        }
    }

    pub(crate) fn show(&self, app_name: Option<&str>, notification: SystemNotification) {
        let mut action_ids = HashMap::new();
        let mut actions = vec![DEFAULT_ACTION.to_string(), DEFAULT_ACTION.to_string()];
        for (index, action) in notification.actions.iter().enumerate() {
            let transport_id = format!("{ACTION_PREFIX}{index}");
            actions.push(transport_id.clone());
            actions.push(action.label.to_string());
            action_ids.insert(transport_id, action.id.clone());
        }
        let post = Post {
            app_name: app_name.map(str::to_string),
            title: notification.title,
            body: notification.body,
            actions,
            tag: notification.tag,
            action_ids,
        };
        let sender = {
            let mut guard = self.poisonable_post_sender();
            match guard.as_ref().cloned() {
                Some(sender) => sender,
                None => {
                    let (post_sender, post_receiver) = mpsc::unbounded();
                    // The poster keeps a sender for the reader's Reset
                    // reports, so the loop runs for the app's lifetime.
                    let worker_post_sender = post_sender.clone();
                    let worker_response_sender = self.response_sender.clone();
                    let spawn_result = std::thread::Builder::new()
                        .name("notify-post".to_string())
                        .spawn(move || {
                            run_poster(post_receiver, worker_post_sender, worker_response_sender)
                        });
                    match spawn_result {
                        Ok(_poster) => {
                            let _ = guard.insert(post_sender.clone());
                            post_sender
                        }
                        // A failed spawn drops the receiver, so a stored
                        // sender would be dead forever; leaving it unset
                        // makes the next post retry.
                        Err(error) => {
                            log::warn!("failed to spawn system notification thread: {error}");
                            return;
                        }
                    }
                }
            }
        };
        if sender.unbounded_send(WorkerPost::Notify(post)).is_err() {
            log::warn!("system notification poster is gone");
            // The worker is gone; let the next post start a new one.
            self.poisonable_post_sender().take();
        }
    }

    pub(crate) fn dismiss(&self, _tag: &str) {
        // The XDG notifications protocol only allows closing via the
        // server-assigned id, which the click registry does not carry;
        // stale notifications simply age out of the shade.
    }

    pub(crate) fn on_response(
        &mut self,
        executor: &ForegroundExecutor,
        callback: Box<dyn FnMut(SystemNotificationResponse)>,
    ) {
        *self.callback.borrow_mut() = Some(callback);

        // Responses arrive from the reader thread; this task hands them to
        // the registered callback on the main thread.
        if let Some(mut receiver) = self.response_receiver.take() {
            let callback = self.callback.clone();
            self._response_task = Some(executor.spawn(async move {
                while let Some(response) = receiver.next().await {
                    // Take the callback out for the call: it may re-enter the
                    // platform or replace itself.
                    let taken = callback.borrow_mut().take();
                    if let Some(mut taken) = taken {
                        taken(response);
                        callback.borrow_mut().get_or_insert(taken);
                    }
                }
            }));
        }
    }

    fn poisonable_post_sender(&self) -> MutexGuard<'_, Option<PostSender>> {
        self.post_sender
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Owns the shared connection for the app's lifetime. Connects lazily, keeps
/// the in-flight table, forwards posts as `Notify` calls, and respawns the
/// reader after a connection loss.
fn run_poster(
    mut post_receiver: PostReceiver,
    post_sender: PostSender,
    response_sender: ResponseSender,
) {
    let inflight: Arc<Mutex<InflightTable>> = Arc::new(Mutex::new(InflightTable::default()));
    let mut connection: Option<zbus::Connection> = None;
    while let Some(post) = block_on(post_receiver.next()) {
        match post {
            WorkerPost::Reset => {
                connection = None;
                // Entries of a dead connection can never receive signals.
                inflight
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clear();
            }
            WorkerPost::Notify(post) => {
                if connection.is_none() {
                    match connect_bus(
                        Arc::clone(&inflight),
                        response_sender.clone(),
                        post_sender.clone(),
                    ) {
                        Ok(connected) => connection = Some(connected),
                        Err(error) => {
                            log::warn!("failed to connect to the session bus: {error}");
                            continue;
                        }
                    }
                }
                let Some(connection) = connection.as_ref() else {
                    continue;
                };
                match block_on(send_notify(connection, &post)) {
                    Ok(id) => {
                        let mut table = inflight.lock().unwrap_or_else(PoisonError::into_inner);
                        // The daemon may have signalled this id before the
                        // reply arrived; drain that signal now so an early
                        // click is not lost and an early close tracks
                        // nothing.
                        match table.take_early(id) {
                            Some(Early::Action(transport)) => {
                                let entry = Inflight {
                                    tag: post.tag,
                                    action_ids: post.action_ids,
                                };
                                if let Some(response) = invoked_response(entry, &transport) {
                                    let _ = response_sender.unbounded_send(response);
                                }
                            }
                            Some(Early::Closed) => {}
                            None => {
                                table.insert(
                                    id,
                                    Inflight {
                                        tag: post.tag,
                                        action_ids: post.action_ids,
                                    },
                                );
                            }
                        }
                    }
                    Err(error) => {
                        log::warn!("failed to show system notification: {error}");
                    }
                }
            }
        }
    }
}

/// Opens the session bus, registers the notification signal rules and starts
/// the reader thread. The rules exist before any `Notify` is sent, so a
/// daemon that closes a notification immediately still delivers the signal.
fn connect_bus(
    inflight: Arc<Mutex<InflightTable>>,
    response_sender: ResponseSender,
    post_sender: PostSender,
) -> Result<zbus::Connection, zbus::Error> {
    block_on(async {
        let connection = zbus::Connection::session().await?;
        let proxy = zbus::fdo::DBusProxy::new(&connection).await?;
        for member in ["ActionInvoked", "NotificationClosed"] {
            let rule = MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(NOTIFICATIONS_INTERFACE)
                .unwrap()
                .member(member)
                .unwrap()
                .build();
            proxy.add_match_rule(rule).await?;
        }
        let reader_connection = connection.clone();
        std::thread::Builder::new()
            .name("notify-read".to_string())
            .spawn(move || run_reader(reader_connection, inflight, response_sender, post_sender))
            .map_err(|error| zbus::Error::InputOutput(std::io::Error::other(error).into()))?;
        Ok(connection)
    })
}

async fn send_notify(connection: &zbus::Connection, post: &Post) -> Result<u32, zbus::Error> {
    let hints: HashMap<String, zbus::zvariant::Value<'_>> = HashMap::new();
    let reply = connection
        .call_method(
            Some(NOTIFICATIONS_DEST),
            NOTIFICATIONS_PATH,
            Some(NOTIFICATIONS_INTERFACE),
            "Notify",
            &(
                post.app_name.as_deref().unwrap_or(""),
                0u32,
                "",
                &*post.title,
                &*post.body,
                post.actions.clone(),
                hints,
                -1i32,
            ),
        )
        .await?;
    reply.body().deserialize()
}

/// Consumes signals for the shared connection and dispatches them by
/// notification id. Exits when the connection dies, then asks the poster to
/// reconnect on the next post.
fn run_reader(
    connection: zbus::Connection,
    inflight: Arc<Mutex<InflightTable>>,
    response_sender: ResponseSender,
    reset_sender: PostSender,
) {
    let mut stream = zbus::MessageStream::from(&connection);
    while let Some(message) = block_on(stream.next()) {
        let Ok(message) = message else {
            break;
        };
        let header = message.header();
        if header.message_type() != zbus::message::Type::Signal {
            continue;
        }
        match header.member() {
            Some(member) if member == "ActionInvoked" => {
                let Ok((id, transport)) = message.body().deserialize::<(u32, String)>() else {
                    continue;
                };
                let mut table = inflight.lock().unwrap_or_else(PoisonError::into_inner);
                let Some(entry) = table.remove(id) else {
                    // The reply has not landed yet; keep the signal so the
                    // click survives until the poster records the id.
                    table.remember_early(id, Early::Action(transport));
                    continue;
                };
                if let Some(response) = invoked_response(entry, &transport) {
                    let _ = response_sender.unbounded_send(response);
                }
            }
            Some(member) if member == "NotificationClosed" => {
                let Ok((id, _reason)) = message.body().deserialize::<(u32, u32)>() else {
                    continue;
                };
                // A close without activation sends no response, exactly as
                // the per-notification wait used to.
                let mut table = inflight.lock().unwrap_or_else(PoisonError::into_inner);
                if table.remove(id).is_none() {
                    // The reply has not landed yet; remember the close so
                    // the poster skips tracking this id entirely.
                    table.remember_early(id, Early::Closed);
                }
            }
            _ => {}
        }
    }
    let _ = reset_sender.unbounded_send(WorkerPost::Reset);
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    zbus::block_on(future)
}

/// The response for an invoked action, if the transport id maps to an action
/// the caller offered.
fn invoked_response(entry: Inflight, transport: &str) -> Option<SystemNotificationResponse> {
    response_action_id(transport, &entry.action_ids).map(|action_id| SystemNotificationResponse {
        tag: entry.tag,
        action_id,
    })
}

fn response_action_id(
    transport_id: &str,
    action_ids: &HashMap<String, SharedString>,
) -> Option<Option<SharedString>> {
    if transport_id == DEFAULT_ACTION {
        Some(None)
    } else {
        action_ids.get(transport_id).cloned().map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_action_ids_do_not_collide_with_transport_action_ids() {
        let action_ids = HashMap::from([
            ("gpui-action-0".to_string(), SharedString::from("default")),
            ("gpui-action-1".to_string(), SharedString::from("__closed")),
        ]);

        assert_eq!(response_action_id(DEFAULT_ACTION, &action_ids), Some(None));
        assert_eq!(
            response_action_id("gpui-action-0", &action_ids),
            Some(Some("default".into()))
        );
        assert_eq!(
            response_action_id("gpui-action-1", &action_ids),
            Some(Some("__closed".into()))
        );
        assert_eq!(response_action_id("unknown", &action_ids), None);
    }

    #[test]
    fn inflight_table_evicts_oldest_at_the_cap() {
        let mut table = InflightTable::default();
        for id in 0..(INFLIGHT_LIMIT as u32 + 1) {
            table.insert(
                id,
                Inflight {
                    tag: SharedString::from(format!("tag-{id}")),
                    action_ids: HashMap::new(),
                },
            );
        }
        assert!(table.by_id.contains_key(&(INFLIGHT_LIMIT as u32)));
        assert!(!table.by_id.contains_key(&0));
        assert!(table.remove(0).is_none());
        assert!(table.remove(INFLIGHT_LIMIT as u32).is_some());
    }

    #[test]
    fn inflight_table_keeps_one_queue_slot_per_reused_id() {
        let mut table = InflightTable::default();
        for tag in ["old", "new"] {
            table.insert(
                5,
                Inflight {
                    tag: SharedString::from(tag),
                    action_ids: HashMap::new(),
                },
            );
        }
        assert_eq!(table.by_id.len(), 1);
        assert_eq!(table.order.iter().filter(|queued| **queued == 5).count(), 1);
        assert_eq!(table.remove(5).map(|entry| entry.tag), Some("new".into()));
    }

    #[test]
    fn early_signals_keep_the_first_arrival() {
        let mut table = InflightTable::default();
        table.remember_early(7, Early::Action("gpui-action-0".to_string()));
        table.remember_early(7, Early::Closed);
        assert!(matches!(table.take_early(7), Some(Early::Action(_))));
        assert!(table.take_early(7).is_none());
    }
}
