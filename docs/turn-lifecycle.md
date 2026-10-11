# Turn lifecycle — end-to-end data flow

Files this doc describes: `crates/tui/src/app.rs` (`handle_key`, `pump`,
`start_turn`, `activity_label`, `Turn`), `crates/core/src/provider.rs`
(`spawn_turn`), `crates/core/src/openai.rs` (`respond`, `stream`),
`crates/tui/src/lib.rs` (`event_loop`). Type contracts live in
[core.md](core.md); this doc is about the flow.

## The happy path

```
user presses Enter (no completion menu open)
  └─ App::handle_key → App::run_command (not a slash command)
       └─ messages.push(Message::User(input)); start_turn()
            └─ spawn_turn(provider, &messages)          [core]
                 └─ OS thread: provider.respond(history, tx)
                      ├─ send(TurnStart)
                      ├─ POST {baseUrl}/chat/completions {stream:true}
                      ├─ per SSE frame: send(Token(chunk))
                      └─ send(TurnEnd)                  (or Error + TurnEnd)

per frame (event_loop polls keys with 50ms timeout):
  └─ App::pump()
       ├─ tick += 1                      (drives the spinner)
       └─ while try_recv():
            ├─ TurnStart  → no-op (tokens follow)
            ├─ Token(t)   → turn.text.push_str(t)     (rendered live)
            ├─ Error(m)   → turn.error = Some(m)
            └─ TurnEnd    → finalize (below)

finalize:
  ├─ turn taken out of App.turn (turn = None)
  ├─ if !turn.text.is_empty() → messages.push(Assistant(text))
  ├─ if turn.error            → messages.push(Error(msg))   (after the partial)
  └─ if queued                → push it as User + start_turn() immediately
```

Rendering: the live area shows `turn.text` while in flight; finalized
messages are printed **above** the viewport into native scrollback
(`ui::message_into`) and are never redrawn by ratatui.

## Cancellation (Esc)

- `handle_key(Esc)` while a turn is in flight drops the `Turn` (which owns
  the `Receiver`). The provider thread's next `send` fails → it returns.
  Partial output already pumped into `turn.text` is **kept**: finalization
  still pushes the partial `Assistant` message (plus `Error` if one was
  received).
- Esc also restores any queued mid-turn input back into the input box
  (tested: `esc_cancels_the_turn_and_restores_the_queue_to_the_input`).
- Hard limits: connect is bounded (15s) and DNS resolve (10s) so a worker can
  actually reach a checkpoint; the stream itself has no timeout by design.

## Queueing (typing while a turn runs)

- Enter mid-turn does **not** start a second turn: input is merged into
  `queued` (multiple submissions join with a blank line; the activity label
  shows `· N queued`, computed from `"\n\n"` matches).
- When the running turn finalizes, the queued text fires as one `User`
  message and the next turn starts immediately
  (`queued_message_fires_when_the_turn_finishes`).

## Model switching mid-turn

The `/model` picker rebuilds `App.provider` (+ system prompt, thinking
level) for **future** turns. A running turn finishes on its original
provider — `spawn_turn` holds its own `Arc` clone, so swapping `App.provider`
cannot affect it.

## Error path (wire level)

`OpenAiCompatProvider::respond` maps failures so consumers keep one finalize
path: partial `Token`s stand, then one `Event::Error` (message built by
`describe()`, e.g. `"chat request: server returned HTTP 401"`), then
`TurnEnd`. Malformed SSE payload → `Malformed` → same path. Missing
`[DONE]` (server closed early) is tolerated as a complete stream.

## Invariants recap (all tested somewhere)

1. `TurnStart` first, exactly one `TurnEnd` last, always.
2. `Token`s concatenate to the exact reply; partial output survives errors
   and cancellation.
3. `Message::Error` never reaches the wire (`wire_messages` filters it).
4. At most one turn in flight; `queued` holds at most one merged message.
5. Dropping `Turn` is the only cancellation mechanism; providers must honor
   failed sends as "stop".
