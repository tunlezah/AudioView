//! The shared publication point.
//!
//! One broadcaster, two consumers: the Unix socket and the web interface's
//! SSE stream both carry the same snapshot with the same `seq`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use lpframe_proto::{ServerMessage, State};
use tokio::sync::broadcast;

/// A note worth showing on the diagnostics page.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Note {
    pub ts: String,
    pub input: String,
    pub text: String,
}

struct Inner {
    seq: u64,
    state: State,
    /// Most recent first.
    notes: VecDeque<Note>,
    note_capacity: usize,
    counters: crate::machine::Counters,
}

#[derive(Clone)]
pub struct Hub {
    inner: Arc<Mutex<Inner>>,
    tx: broadcast::Sender<Arc<ServerMessage>>,
}

impl Hub {
    pub fn new(note_capacity: usize) -> Hub {
        // A slow client falls behind rather than blocking the core; broadcast
        // drops the oldest messages for it, and since every message is a full
        // snapshot, the next one it receives makes it correct again.
        let (tx, _) = broadcast::channel(64);
        Hub {
            inner: Arc::new(Mutex::new(Inner {
                seq: 0,
                state: State::default(),
                notes: VecDeque::new(),
                note_capacity: note_capacity.max(1),
                counters: Default::default(),
            })),
            tx,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ServerMessage>> {
        self.tx.subscribe()
    }

    pub fn snapshot(&self) -> (u64, State) {
        let g = self.inner.lock().expect("hub mutex poisoned");
        (g.seq, g.state.clone())
    }

    /// The current state as a message, for a client that has just connected.
    pub fn current_message(&self) -> ServerMessage {
        let (seq, state) = self.snapshot();
        ServerMessage::state(seq, crate::now_rfc3339(), state)
    }

    pub fn notes(&self) -> Vec<Note> {
        let g = self.inner.lock().expect("hub mutex poisoned");
        g.notes.iter().cloned().collect()
    }

    pub fn counters(&self) -> crate::machine::Counters {
        self.inner.lock().expect("hub mutex poisoned").counters
    }

    pub fn set_counters(&self, c: crate::machine::Counters) {
        self.inner.lock().expect("hub mutex poisoned").counters = c;
    }

    pub fn push_note(&self, input: &str, text: String) {
        let note = Note {
            ts: crate::now_rfc3339(),
            input: input.to_string(),
            text,
        };
        let mut g = self.inner.lock().expect("hub mutex poisoned");
        g.notes.push_front(note);
        let cap = g.note_capacity;
        g.notes.truncate(cap);
    }

    /// Publish a new snapshot. Returns the assigned sequence number.
    pub fn publish(&self, state: State) -> u64 {
        let msg = {
            let mut g = self.inner.lock().expect("hub mutex poisoned");
            g.seq += 1;
            g.state = state.clone();
            ServerMessage::state(g.seq, crate::now_rfc3339(), state)
        };
        let seq = match &msg {
            ServerMessage::State { seq, .. } => *seq,
            _ => unreachable!(),
        };
        // An error here only means nobody is listening yet.
        let _ = self.tx.send(Arc::new(msg));
        seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpframe_proto::Playback;

    #[test]
    fn sequence_numbers_increase_and_snapshots_are_current() {
        let hub = Hub::new(4);
        assert_eq!(hub.snapshot().0, 0);

        let mut s = State {
            playback: Playback::Playing,
            ..Default::default()
        };
        assert_eq!(hub.publish(s.clone()), 1);
        assert_eq!(hub.snapshot().0, 1);
        assert_eq!(hub.snapshot().1.playback, Playback::Playing);

        s.playback = Playback::Idle;
        assert_eq!(hub.publish(s), 2);
    }

    #[test]
    fn notes_are_capped_newest_first() {
        let hub = Hub::new(3);
        for i in 0..6 {
            hub.push_note("tick", format!("note {i}"));
        }
        let notes = hub.notes();
        assert_eq!(notes.len(), 3);
        assert_eq!(notes[0].text, "note 5");
        assert_eq!(notes[2].text, "note 3");
    }

    #[tokio::test]
    async fn subscribers_receive_published_snapshots() {
        let hub = Hub::new(4);
        let mut rx = hub.subscribe();
        hub.publish(State::default());
        let msg = rx.recv().await.unwrap();
        assert!(matches!(&*msg, ServerMessage::State { seq: 1, .. }));
    }
}
