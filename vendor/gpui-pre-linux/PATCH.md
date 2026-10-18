# Vendored gpui-pre-linux 0.3.6 with a notification connection fix

This directory vendors `gpui-pre-linux` 0.3.6 exactly as published on
crates.io (an Apache-2.0 snapshot of [zed-industries/zed]'s `gpui` Linux
platform, published by the `gpui-pre` maintainer), with one module rewritten:
`src/linux/system_notifications.rs`. See `LICENSE-APACHE` for the license of
the vendored code.

[zed-industries/zed]: https://github.com/zed-industries/zed

## Why

Every `cx.show_system_notification()` call used to open a **dedicated**
session-bus connection that was never closed in realistic conditions
(issue [#387] in this repository):

- The per-notification thread blocked in notify-rust's `wait_for_action`
  with no timeout, and `dismiss()` is a no-op on this platform, so the
  connection and thread lived until process exit whenever the notification
  produced no observable close signal.
- notify-rust registered its `ActionInvoked`/`NotificationClosed` match rules
  only **after** the `Notify` reply, so a daemon that closes a notification
  immediately (do-not-disturb or history-only delivery, or a close emitted
  before the reply) never delivered a signal the client could observe.
- The session bus caps clients at roughly 510 by default, and dbus-broker
  exits when it can no longer allocate a peer pidfd, taking the graphical
  session down with it. Reporters measured one leaked connection per
  notification on real sessions.

Still affected upstream as of `gpui-pre-linux` 0.3.9, which keeps the
per-notification connection-and-`wait_for_action` shape.

[#387]: https://github.com/penso/herdr-gpui/issues/387

## What changed

Only `src/linux/system_notifications.rs`; the public API is unchanged, and
response semantics are identical (body activation reports `None`, button
activation reports the caller's action id, unknown actions log a warning,
closes report nothing).

- One shared session-bus connection for all notifications, opened lazily on
  the first post; a connection that dies is re-established by the next post.
- Both signal match rules are registered **before** the first `Notify`, so a
  daemon-side instant close is still delivered.
- One poster thread (`notify-post`) sends `Notify` calls off the calling
  thread; one reader thread (`notify-read`) consumes signals and dispatches
  them by notification id.
- A bounded in-flight table (1024 entries, FIFO eviction) replaces the
  unbounded per-notification waits. A close signal that arrives before the
  `Notify` reply can leave a stale entry, which the cap evicts; a stale entry
  is a few hundred bytes, not a socket.
- Responses reach the app through the same channel and callback contract as
  before.

## Verification

On an isolated Linux VM against the pinned stack (notify-rust 4.18, zbus
5.19), with a stub `org.freedesktop.Notifications` daemon that emits
`NotificationClosed` **before** returning the `Notify` reply, which triggers
the leak deterministically:

- Unpatched: every notification leaked one bus connection, one
  `system-notification` thread, and one zbus connection thread, permanently.
- Patched: five notifications left exactly one shared connection plus the
  platform's pre-existing accessibility connection; two persistent threads
  total; no spurious responses for closed-without-activation notifications.
- The same daemon emitting `ActionInvoked(id, "default")` after the reply:
  every response was dispatched to the registered callback with the correct
  tag and `action_id`.
- A real daemon (dunst) closing notifications normally: no residue.
- The application built against the vendored crate and ran under Xvfb.

Not exercised here: Wayland compositors and a full desktop session; the
module's two unit tests require a Linux host
(`cargo test --lib` inside this directory; the crate is not built on macOS or
Windows, so CI never compiles or runs them).

## How to drop this vendor

When a `gpui-pre` snapshot ships the shared-connection fix (check its
`system_notifications.rs` for the one-connection/one-reader shape), delete
this directory and the `[patch.crates-io]` section in the workspace
`Cargo.toml`, and bump the `gpui-pre*` pins.
