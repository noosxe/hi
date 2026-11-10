//! The provider abstraction: a backend that can produce assistant turns.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

use crate::event::Event;
use crate::message::Message;

/// A backend that can produce one assistant turn.
///
/// `respond` receives the conversation so far and the sending end of a
/// channel; it streams `Event`s into the channel until the turn
/// completes. The trait is deliberately tiny — an HTTP LLM backend, a
/// local model, and the mock all reduce to exactly this shape.
/// `Send + Sync` are the thread-safety promises the turn thread needs:
/// `Send` — the provider may be *moved* to another thread (inside the
/// `Arc`), `Sync` — it may be *used* through `&self` from that thread
/// while the caller still holds its own handle. Any provider made of
/// plain data satisfies both; ones with interior mutability must use
/// thread-safe primitives (e.g. `Mutex`) to keep the promise.
pub trait Provider: Send + Sync {
    /// Stream one assistant turn into `tx`.
    ///
    /// Runs on its own thread (see `spawn_turn`). Dropping the
    /// `Receiver` is the cancellation signal: every subsequent send
    /// fails, and a well-behaved provider returns promptly.
    fn respond(&self, history: &[Message], tx: Sender<Event>);
}

/// Run one turn on a background thread; events come back through the
/// returned `Receiver`.
///
/// Why each piece looks the way it does:
///
/// - `dyn Provider` — a *trait object*: "some provider, decided at
///   runtime" (later: whichever backend the config picked), instead of
///   fixing one concrete type at compile time.
/// - `Arc` — *atomically reference-counted* shared ownership. The
///   caller keeps the provider for future turns while the spawned
///   thread holds a second handle to the same object; the count
///   releases when the last handle drops.
/// - `mpsc` — *multi-producer, single-consumer* channel. Here used
///   with one producer (the turn thread) and one consumer (the event
///   loop), which is the shape every agent harness converges on.
pub fn spawn_turn(provider: Arc<dyn Provider>, history: &[Message]) -> Receiver<Event> {
    let (tx, rx) = channel();
    // The thread must own everything it touches for as long as it
    // runs ('static): it takes its own Arc handle to the provider and
    // a copy of the history. `tx` moves in too; when the thread ends,
    // dropping it closes the channel.
    let history = history.to_vec();
    thread::spawn(move || {
        // The provider runs on a worker thread of its own and this
        // thread relays its events, noting whether the round already
        // closed itself (`Error`, `TurnEnd`). A panic inside `respond`
        // would otherwise take its message with the thread: the join
        // below recovers it, like a panicking tool's, and reports it on
        // the single finalization path without repeating what the
        // provider already said (#243). Dropping `rx` still cancels:
        // the failed relay drops the inner receiver, so the provider's
        // next send fails.
        let (inner_tx, inner_rx) = channel();
        let worker = thread::spawn(move || provider.respond(&history, inner_tx));
        let (mut sent_error, mut sent_end) = (false, false);
        for event in inner_rx {
            match &event {
                Event::Error(_) => sent_error = true,
                Event::TurnEnd => sent_end = true,
                _ => {}
            }
            if tx.send(event).is_err() {
                return;
            }
        }
        if let Err(payload) = worker.join()
            && !sent_end
        {
            if !sent_error {
                let _ = tx.send(Event::Error(format!(
                    "provider panicked: {}",
                    crate::agent::panic_message(payload.as_ref())
                )));
            }
            let _ = tx.send(Event::TurnEnd);
        }
    });
    rx
}
