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
//! lifetime of the process. With a shared connection the worst case is a
//! stale entry in the bounded in-flight table, never a leaked connection.

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

type ResponseCallback = Rc<RefCell<Option<Box<dyn FnMut(SystemNotificationResponse)>>>>;

/// Everything the reader needs to answer signals for one notification.
struct Inflight {
    tag: SharedString,
    action_ids: HashMap<String, SharedString>,
}

#[derive(Default)]
struct InflightTable {
    by_id: HashMap<u32, Inflight>,
    order: VecDeque<u32>,
}

impl InflightTable {
    fn insert(&mut self, id: u32, entry: Inflight) {
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

    fn clear(&mut self) {
        self.by_id.clear();
        self.order.clear();
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
    /// bus on the calling thread.
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
                    if let Err(error) = spawn_result {
                        log::warn!("failed to spawn system notification thread: {error}");
                    }
                    let _ = guard.insert(post_sender.clone());
                    post_sender
                }
            }
        };
        if sender.unbounded_send(WorkerPost::Notify(post)).is_err() {
            log::warn!("system notification poster is gone");
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
                        inflight
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(
                                id,
                                Inflight {
                                    tag: post.tag,
                                    action_ids: post.action_ids,
                                },
                            );
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
                let entry = inflight
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(id);
                let Some(entry) = entry else {
                    continue;
                };
                if let Some(action_id) = response_action_id(&transport, &entry.action_ids) {
                    let _ = response_sender.unbounded_send(SystemNotificationResponse {
                        tag: entry.tag,
                        action_id,
                    });
                }
            }
            Some(member) if member == "NotificationClosed" => {
                let Ok((id, _reason)) = message.body().deserialize::<(u32, u32)>() else {
                    continue;
                };
                // A close without activation sends no response, exactly as
                // the per-notification wait used to. A close that arrives
                // before the poster recorded the id leaves a stale entry,
                // which the in-flight cap evicts.
                inflight
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(id);
            }
            _ => {}
        }
    }
    let _ = reset_sender.unbounded_send(WorkerPost::Reset);
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    zbus::block_on(future)
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
        assert_eq!(table.remove(0), None);
        assert!(table.remove(INFLIGHT_LIMIT as u32).is_some());
    }
}
