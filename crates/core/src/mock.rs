//! A provider that streams a canned reply — no network, no API keys.

use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use crate::event::Event;
use crate::message::Message;
use crate::provider::Provider;

/// Streams a fixed reply word by word, pausing between tokens, so the
/// TUI's streaming path gets exercised at human speed.
#[derive(Debug, Clone)]
pub struct MockProvider {
    /// The full reply, split into word tokens on the fly.
    pub reply: String,
    /// Pause between tokens. `Duration::ZERO` in tests.
    pub delay: Duration,
}

impl MockProvider {
    pub fn new(reply: impl Into<String>, delay: Duration) -> Self {
        Self {
            reply: reply.into(),
            delay,
        }
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new(
            "Hello! I am the mock provider — streaming works, no API key required.",
            Duration::from_millis(40),
        )
    }
}

impl Provider for MockProvider {
    fn respond(&self, _history: &[Message], tx: Sender<Event>) {
        // If the very first send fails, the consumer is already gone.
        if tx.send(Event::TurnStart).is_err() {
            return;
        }
        // `split_inclusive` keeps the separator attached to each word,
        // so concatenating the tokens reproduces the reply exactly —
        // including the spaces between words.
        for word in self.reply.split_inclusive(' ') {
            // A failed send means the `Receiver` was dropped: that is
            // the cancellation signal, so stop streaming immediately.
            if tx.send(Event::Token(word.to_string())).is_err() {
                return;
            }
            thread::sleep(self.delay);
        }
        let _ = tx.send(Event::TurnEnd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::spawn_turn;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn streams_turnstart_tokens_turnend_in_order() {
        let rx = spawn_turn(
            Arc::new(MockProvider::new("one two three", Duration::ZERO)),
            &[],
        );
        // `rx.iter()` blocks until the channel closes (the thread
        // ended and dropped `tx`), collecting every event in order.
        let events: Vec<Event> = rx.iter().collect();

        assert_eq!(events.first(), Some(&Event::TurnStart));
        assert_eq!(events.last(), Some(&Event::TurnEnd));

        let text: String = events[1..events.len() - 1]
            .iter()
            .map(|event| match event {
                Event::Token(word) => word.as_str(),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(text, "one two three");
    }

    #[test]
    fn stops_when_receiver_is_dropped() {
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx); // simulate the UI quitting mid-turn

        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        thread::spawn(move || {
            MockProvider::new("never arrives", Duration::ZERO).respond(&[], tx);
            flag.store(true, Ordering::SeqCst);
        })
        .join()
        .unwrap();

        // The first token send failed, so `respond` returned at once —
        // the thread finished instead of blocking forever.
        assert!(done.load(Ordering::SeqCst));
    }
}
