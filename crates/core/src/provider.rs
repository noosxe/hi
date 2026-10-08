//! The provider abstraction: a backend that can produce assistant turns.

use std::sync::Arc;
use std::sync::mpsc::{channel, Receiver, Sender};
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
    thread::spawn(move || provider.respond(&history, tx));
    rx
}
