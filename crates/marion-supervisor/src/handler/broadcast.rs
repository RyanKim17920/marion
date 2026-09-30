use marion_core::proto::ReplayPoint;
use marion_core::proto::notify::Event;

use super::Shared;
use crate::registry::LiveRegistry;

/// Send to every subscriber, dropping the ones that have gone.
///
/// [`Outbound::send`] never blocks, so this cannot be slowed by a client — see `serve.rs`: a full
/// queue is a verdict about that client, and §5.7 is what makes it the right one.
/// What the operator's `notify.toml` and `MARION_NOTIFY` ask for, titled with the project's
/// directory name: the notifier's seed, whether notices start on or off.
pub(super) fn notify_seed_for(project_root: &std::path::Path) -> crate::notify::NotifySeed {
    let config = crate::notify::NotifyConfig::load(
        crate::credentials::config_dir().ok().as_deref(),
        std::env::var(crate::notify::NOTIFY_ENV).ok().as_deref(),
    )
    .unwrap_or_else(|e| {
        eprintln!("marion: notifications are off: {e}");
        crate::notify::NotifyConfig::default()
    });
    let backend =
        crate::notify::Backend::resolve(std::env::var(crate::notify::BACKEND_ENV).ok().as_deref());
    let shown = match project_root.file_name().and_then(|f| f.to_str()) {
        Some(".git") => project_root.parent().unwrap_or(project_root),
        _ => project_root,
    };
    let project = shown
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("project")
        .to_string();
    crate::notify::NotifySeed {
        config,
        backend,
        project,
    }
}

/// Send `shown` to the first claimer still connected; a claimer whose send fails is gone, and the
/// next is tried.
pub(super) fn notify_claimer(
    g: &mut Shared,
    ring: crate::notify::TerminalRing,
    shown: &[crate::notify::Shown],
) {
    while let Some(head) = g.claimers.first() {
        let sent = shown.iter().all(|s| {
            head.send(&marion_core::proto::Frame::Notification(
                marion_core::proto::Notification::new(Event::NotifyNotice {
                    title: s.title.clone(),
                    body: s.body.clone(),
                    ring: ring.word().to_string(),
                }),
            ))
        });
        if sent {
            return;
        }
        g.claimers.remove(0);
    }
}

pub(super) fn deliver(g: &mut Shared, events: &[Event]) {
    if events.is_empty() {
        return;
    }
    g.subs.retain(|s| {
        events.iter().all(|e| {
            s.send(&marion_core::proto::Frame::Notification(
                marion_core::proto::Notification::new(e.clone()),
            ))
        })
    });
}

/// The read point the registry is currently serving, for a caller that wants it without a client.
pub fn read_point(live: &LiveRegistry) -> ReplayPoint {
    live.read(|r| r.read_point())
}
