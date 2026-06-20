//! A tiny shared shutdown flag for cooperative daemon shutdown (ADR-0007).
//!
//! The standalone `taski-daemon` binary's Ctrl-C handler and the future unified
//! launcher both need to ask the daemon engine / watch loop to stop. This module
//! provides a linked signal/handle pair backed by an `Arc<AtomicBool>` — std-only,
//! no dependencies — so the engine polls a [`ShutdownHandle`] while whoever drives
//! shutdown holds the matching [`ShutdownSignal`] and calls [`ShutdownSignal::set`]
//! when it's time to stop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The writable half: held by whoever initiates shutdown (the standalone entry's
/// Ctrl-C handler; later, the launcher on TUI quit).
#[derive(Clone)]
pub struct ShutdownSignal(Arc<AtomicBool>);

/// The readable half: held by the daemon engine / watch loop.
#[derive(Clone)]
pub struct ShutdownHandle(Arc<AtomicBool>);

impl ShutdownSignal {
    /// Create a linked signal/handle pair (both start unset).
    pub fn new() -> (Self, ShutdownHandle) {
        let flag = Arc::new(AtomicBool::new(false));
        (Self(flag.clone()), ShutdownHandle(flag))
    }

    /// Request shutdown.
    pub fn set(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl ShutdownHandle {
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl Default for ShutdownSignal {
    fn default() -> Self {
        Self::new().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set()` on the signal is observable through the linked handle, and a fresh
    /// pair starts unset. Both halves are independently cloneable (e.g. the launcher
    /// moves one clone into the Ctrl-C closure while the engine holds its own).
    #[test]
    fn set_is_observed_by_handle() {
        let (signal, handle) = ShutdownSignal::new();
        assert!(!handle.is_set(), "fresh pair must start unset");
        assert!(!signal.is_set());

        signal.set();
        assert!(signal.is_set());
        assert!(handle.is_set(), "handle must observe signal.set()");

        // A cloned handle observes the same flag (the launcher model).
        let handle2 = handle.clone();
        assert!(handle2.is_set());
    }

    /// Each `ShutdownSignal::new()` produces an *independent* pair — setting one
    /// never trips another.
    #[test]
    fn pairs_are_independent() {
        let (s1, h1) = ShutdownSignal::new();
        let (_, h2) = ShutdownSignal::new();
        s1.set();
        assert!(h1.is_set());
        assert!(
            !h2.is_set(),
            "a second pair must be unaffected by the first"
        );
    }
}
