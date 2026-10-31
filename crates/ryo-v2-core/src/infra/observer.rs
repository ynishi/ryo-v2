//! [`Observer`] adapters.

use crate::app::ports::{Event, Observer};
use std::sync::Mutex;

/// Collects every event in memory.
#[derive(Default)]
pub struct RecordingObserver {
    events: Mutex<Vec<Event>>,
}

impl RecordingObserver {
    /// 溜めた event を取り出して空にする。
    pub fn take(&self) -> Vec<Event> {
        match self.events.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(p) => std::mem::take(&mut *p.into_inner()),
        }
    }
}

impl Observer for RecordingObserver {
    fn observe(&self, event: Event) {
        match self.events.lock() {
            Ok(mut g) => g.push(event),
            Err(p) => p.into_inner().push(event),
        }
    }
}
