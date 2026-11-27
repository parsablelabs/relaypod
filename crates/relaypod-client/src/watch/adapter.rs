use std::future::Future;
use std::pin::Pin;

use crate::RelayPodClient;
use relaypod_core::{ActivityState, UnreadItem};

use super::{ResolvedTarget, SessionStore};

/// Live activity update reported by a [`WatcherAdapter`] while a dispatch is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityUpdate {
    pub state: ActivityState,
    pub pane_id: Option<String>,
    pub detail: Option<String>,
}

impl ActivityUpdate {
    pub fn working() -> Self {
        Self {
            state: ActivityState::Working,
            pane_id: None,
            detail: None,
        }
    }

    pub fn blocked(pane_id: Option<String>, detail: Option<String>) -> Self {
        Self {
            state: ActivityState::Blocked,
            pane_id,
            detail,
        }
    }
}

/// Context passed to a [`WatcherAdapter`] for a single unread item.
pub struct DispatchContext<'a> {
    pub client: &'a RelayPodClient,
    pub target: &'a ResolvedTarget,
    pub identity_id: &'a str,
    pub handle: &'a str,
    pub item: &'a UnreadItem,
    pub thread_id: &'a str,
    pub sessions: &'a SessionStore,
    pub on_activity: &'a (dyn Fn(ActivityUpdate) + Send + Sync),
}

/// Result of dispatching one unread item through an adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchOutcome {
    /// Whether the dispatch itself succeeded without error.
    pub succeeded: bool,
    /// Whether `auto_ack` is safe to run (false e.g. if an interactive agent is `blocked`).
    pub allow_auto_ack: bool,
    /// Optional human-readable error or status detail (surfaced in UI watcher logs).
    pub detail: Option<String>,
}

impl DispatchOutcome {
    pub fn ok() -> Self {
        Self {
            succeeded: true,
            allow_auto_ack: true,
            detail: None,
        }
    }

    pub fn blocked() -> Self {
        Self {
            succeeded: true,
            allow_auto_ack: false,
            detail: None,
        }
    }

    pub fn failed_with(detail: impl Into<String>) -> Self {
        Self {
            succeeded: false,
            allow_auto_ack: false,
            detail: Some(detail.into()),
        }
    }
}

/// Pluggable execution backend for `relaypod watch`.
///
/// Current implementations:
/// - `ExecAdapter`: runs headless `codex exec` / `claude -p` or custom shell `exec:` commands.
/// - `HerdrAdapter`: launches and drives interactive agent panes via the `herdr` CLI.
///
/// Future backends (e.g. `tmux`, remote runners) implement this same trait.
pub trait WatcherAdapter: Send + Sync {
    fn name(&self) -> &'static str;

    fn dispatch<'a>(
        &'a self,
        ctx: DispatchContext<'a>,
    ) -> Pin<Box<dyn Future<Output = DispatchOutcome> + Send + 'a>>;
}
