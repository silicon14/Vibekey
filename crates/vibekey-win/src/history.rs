//! In-memory text clipboard history and suppression of internal clipboard writes.
//!
//! The listener runs on the main thread, while paste/restore runs on the inject
//! worker.  `HistoryControl` closes the race between those threads: notifications
//! delivered during a write are ignored immediately, and the clipboard sequence
//! number also suppresses a notification delivered after the write has finished.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};

use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;

/// Keep memory use bounded and the picker quickly navigable.
pub const HISTORY_CAPACITY: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    id: u64,
    text: String,
}

impl HistoryEntry {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Newest-first text history. It deliberately has no file-backed representation:
/// copied passwords and other sensitive text must not be written to disk merely
/// because clipboard history is enabled.
#[derive(Debug)]
pub struct ClipboardHistory {
    entries: VecDeque<HistoryEntry>,
    next_id: u64,
}

impl ClipboardHistory {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            next_id: 1,
        }
    }

    /// Add text, or move an identical existing entry to the front. Returns true
    /// when the order/content changed. Empty clipboard values are not entries.
    pub fn record(&mut self, text: String) -> bool {
        if text.is_empty() {
            return false;
        }
        if let Some(position) = self.entries.iter().position(|entry| entry.text == text) {
            if position == 0 {
                return false;
            }
            if let Some(entry) = self.entries.remove(position) {
                self.entries.push_front(entry);
                return true;
            }
            return false;
        }

        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.entries.push_front(HistoryEntry { id, text });
        while self.entries.len() > HISTORY_CAPACITY {
            self.entries.pop_back();
        }
        true
    }

    pub fn entries(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.entries.iter()
    }

    pub fn text(&self, id: u64) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.id == id)
            .map(HistoryEntry::text)
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let Some(position) = self.entries.iter().position(|entry| entry.id == id) else {
            return false;
        };
        self.entries.remove(position).is_some()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for ClipboardHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared only for coordinating clipboard writes; actual history remains on the
/// main thread. The gate makes sequence-check + read + sequence-recheck atomic
/// with respect to internal writes. A sequence of zero is never trusted.
#[derive(Debug, Default)]
pub struct HistoryControl {
    gate: Mutex<()>,
    internal_sequence: AtomicU32,
}

impl HistoryControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run one complete clipboard mutation as an internal operation. The Win32
    /// writer returns a sequence obtained while it still owns the clipboard, so
    /// a later external write can never be misregistered as ours.
    pub fn internal_write(
        &self,
        operation: impl FnOnce() -> crate::clipboard::ClipboardMutation,
    ) -> bool {
        let _lock = self.lock();
        let outcome = operation();
        if let Some(sequence) = outcome.sequence {
            self.internal_sequence.store(sequence, Ordering::Release);
        }
        outcome.succeeded
    }

    /// Read an external clipboard value without a check/read TOCTOU race. A
    /// try_lock keeps the UI thread from waiting behind an injection retry. The
    /// second sequence check also rejects changes made by other processes while
    /// CF_UNICODETEXT was being read.
    pub fn capture_text(&self) -> Option<String> {
        let _lock = match self.gate.try_lock() {
            Ok(lock) => lock,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        // SAFETY: GetClipboardSequenceNumber has no preconditions.
        let before = unsafe { GetClipboardSequenceNumber() };
        if before == 0 || before == self.internal_sequence.load(Ordering::Acquire) {
            return None;
        }
        let text = crate::clipboard::read_text_for_history();
        // SAFETY: GetClipboardSequenceNumber has no preconditions.
        let after = unsafe { GetClipboardSequenceNumber() };
        if after == 0 || after != before || after == self.internal_sequence.load(Ordering::Acquire)
        {
            return None;
        }
        text
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        // No history data is protected by this gate, so a poisoned lock cannot
        // leave a partially updated model. It only serialises OS clipboard IO.
        self.gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_empty_and_keeps_newest_first() {
        let mut history = ClipboardHistory::new();
        assert!(!history.record(String::new()));
        assert!(history.record("first".to_string()));
        assert!(history.record("second".to_string()));
        let values: Vec<&str> = history.entries().map(HistoryEntry::text).collect();
        assert_eq!(values, ["second", "first"]);
    }

    #[test]
    fn duplicate_moves_to_front_without_adding_a_copy() {
        let mut history = ClipboardHistory::new();
        history.record("first".to_string());
        history.record("second".to_string());
        assert!(history.record("first".to_string()));
        assert!(!history.record("first".to_string()));
        let values: Vec<&str> = history.entries().map(HistoryEntry::text).collect();
        assert_eq!(values, ["first", "second"]);
    }

    #[test]
    fn capacity_drops_the_oldest_entry() {
        let mut history = ClipboardHistory::new();
        for index in 0..(HISTORY_CAPACITY + 5) {
            history.record(format!("entry {index}"));
        }
        assert_eq!(history.entries().count(), HISTORY_CAPACITY);
        assert_eq!(
            history.entries().next().map(HistoryEntry::text),
            Some("entry 54")
        );
        assert!(history.entries().all(|entry| entry.text() != "entry 0"));
    }

    #[test]
    fn stable_ids_find_and_delete_exact_entries() {
        let mut history = ClipboardHistory::new();
        history.record("first".to_string());
        history.record("second".to_string());
        let first_id = history.entries().nth(1).unwrap().id();
        assert_eq!(history.text(first_id), Some("first"));
        assert!(history.remove(first_id));
        assert_eq!(history.text(first_id), None);
        assert!(!history.remove(first_id));
        assert_eq!(history.entries().count(), 1);
    }

    #[test]
    fn internal_sequence_comes_from_the_owned_write_result() {
        let control = HistoryControl::new();
        assert!(
            control.internal_write(|| crate::clipboard::ClipboardMutation {
                succeeded: true,
                sequence: Some(42),
            })
        );
        assert_eq!(control.internal_sequence.load(Ordering::Acquire), 42);

        // A failure without an owned mutation must not overwrite the prior value
        // with whatever sequence an unrelated process happens to have now.
        assert!(
            !control.internal_write(|| crate::clipboard::ClipboardMutation {
                succeeded: false,
                sequence: None,
            })
        );
        assert_eq!(control.internal_sequence.load(Ordering::Acquire), 42);
    }

    #[test]
    fn clear_removes_all_text() {
        let mut history = ClipboardHistory::new();
        history.record("secret".to_string());
        history.clear();
        assert!(history.is_empty());
        assert_eq!(history.entries().count(), 0);
    }
}
