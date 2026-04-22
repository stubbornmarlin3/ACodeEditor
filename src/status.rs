use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StatusLevel {
    Info,
    Ok,
    Warn,
    Err,
}

pub struct StatusMsg {
    pub text: String,
    pub level: StatusLevel,
}

/// How long each status line stays on-screen before giving way to the
/// next queued message (or disappearing if the queue is empty).
pub const MSG_TTL: Duration = Duration::from_secs(3);

pub struct StatusBar {
    pub queue: VecDeque<StatusMsg>,
    pub expires_at: Option<Instant>,
}

impl StatusBar {
    pub fn new() -> Self {
        Self { queue: VecDeque::new(), expires_at: None }
    }

    pub fn push(&mut self, text: String, level: StatusLevel) {
        let msg = StatusMsg { text, level };
        let was_empty = self.queue.is_empty();
        self.queue.push_back(msg);
        if was_empty {
            self.expires_at = Some(Instant::now() + MSG_TTL);
        }
    }

    /// Classify by content then push. The 200+ existing call sites don't
    /// thread a level through explicitly — the content itself carries
    /// enough signal ("failed" → err, "usage:" → warn, etc.).
    ///
    /// Takes `String` rather than `impl Into<String>` so the many existing
    /// `"literal".into()` / `var.into()` call sites compile without extra
    /// type annotations (the target type is unambiguously `String`).
    pub fn push_auto(&mut self, text: String) {
        let level = classify(&text);
        self.push(text, level);
    }

    /// Live-update push: if the current message shares the given
    /// prefix, replace it in place (resetting the TTL) instead of
    /// queueing. Keeps the displayed line current — used by `:ex`
    /// output polling so a burst of new tail lines doesn't pile up
    /// behind an already-expired first message.
    pub fn push_live(&mut self, prefix: &str, text: String) {
        let level = classify(&text);
        let replace = self.queue.front().map_or(false, |m| m.text.starts_with(prefix));
        if replace {
            // Drop every queued `prefix`-tagged message so we don't
            // tail a long line with stale earlier ones.
            self.queue.retain(|m| !m.text.starts_with(prefix));
            self.queue.push_front(StatusMsg { text, level });
            self.expires_at = Some(Instant::now() + MSG_TTL);
        } else {
            self.push(text, level);
        }
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.expires_at = None;
    }

    pub fn current(&self) -> Option<&StatusMsg> {
        self.queue.front()
    }

    /// Advance past any messages whose display time has elapsed. Returns
    /// true if something changed (so the UI thread knows to redraw).
    pub fn tick(&mut self, now: Instant) -> bool {
        let Some(exp) = self.expires_at else { return false; };
        if now < exp { return false; }
        self.queue.pop_front();
        if self.queue.is_empty() {
            self.expires_at = None;
        } else {
            self.expires_at = Some(now + MSG_TTL);
        }
        true
    }

    /// Duration until the current message expires. None while idle — the
    /// main loop uses this to size its `recv_timeout`.
    pub fn next_tick_in(&self, now: Instant) -> Option<Duration> {
        self.expires_at.map(|t| t.saturating_duration_since(now))
    }

    /// Did the most recently pushed message contain this substring? Used
    /// by post-call branching (e.g., skip close-all if write failed).
    pub fn last_has(&self, needle: &str) -> bool {
        self.queue.back().map_or(false, |m| m.text.contains(needle))
    }

    pub fn last_starts_with(&self, prefix: &str) -> bool {
        self.queue.back().map_or(false, |m| m.text.starts_with(prefix))
    }
}

/// Heuristic classifier. The existing callsites use a consistent vocabulary
/// ("failed", "usage:", "opened ", etc.); we lean on that rather than
/// threading a level through every `format!` site.
pub fn classify(text: &str) -> StatusLevel {
    let t = text.to_lowercase();

    if t.contains("failed") || t.contains(" error") || t.starts_with("error") {
        return StatusLevel::Err;
    }

    if t.starts_with("usage:")
        || t.starts_with("unknown:")
        || t.contains(" needs ")
        || t.starts_with("unsaved:")
        || t.starts_with("max ")
        || t.starts_with("no cell")
        || t.starts_with("no project")
        || t.starts_with("no branches")
        || t.starts_with("no changes")
        || t.starts_with("no commits")
        || t.starts_with("no match")
        || t.starts_with("no conflict")
        || t.starts_with("no such")
        || t.starts_with("no op")
        || t.starts_with("no previous")
        || t.starts_with("no remotes")
        || t.starts_with("no stashes")
        || t.starts_with("nothing to")
        || t.starts_with("not a ")
        || t.contains("conflicted:")
        || t.contains("resolve first")
        || t == "cancelled"
        || t.starts_with("can't ")
        || t.starts_with("commit:")
        || t.starts_with("discard:")
        || t.starts_with("case mismatch:")
        || t.starts_with("case mismatch")
        || t.starts_with(":") // `:w needs editor focus`-style hints that start with `:`
    {
        return StatusLevel::Warn;
    }

    if t.starts_with("opened ")
        || t.starts_with("wrote ")
        || t.starts_with("added ")
        || t.starts_with("closed ")
        || t.starts_with("reloaded ")
        || t.starts_with("deleted ")
        || t.starts_with("force-deleted ")
        || t.starts_with("renamed ")
        || t.starts_with("pasted")
        || t.starts_with("switched to ")
        || t.starts_with("yanked ")
        || t.starts_with("resolved ")
        || t.starts_with("on ")
        || t.starts_with("new cell ")
        || t.starts_with("new file:")
        || t.starts_with("remote add")
        || t.starts_with("remote rm")
        || t.starts_with("branch ")
        || t.starts_with("stash ")
        || t.ends_with(" ok")
    {
        return StatusLevel::Ok;
    }

    StatusLevel::Info
}
