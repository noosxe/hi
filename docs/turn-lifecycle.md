# Turn lifecycle — end-to-end data flow

Files this doc describes: `crates/tui/src/app.rs` (`handle_key`, `pump`,
Files this doc describes: `crates/tui/src/app.rs` (`handle_key`, `pump`,
`start_turn`, `activity_label`, `Turn`), `crates/core/src/provider.rs`
(`spawn_turn`), `crates/core/src/tool.rs`, `crates/core/src/agent.rs`
(`spawn_agent_turn`), `crates/core/src/openai.rs` (`respond`, `stream`),
`crates/tui/src/lib.rs` (`event_loop`, `guard_panic_hook`). Type contracts live in
[core.md](core.md); this doc is about the flow.

## The happy path

```
user presses Enter (no completion menu open)
  └─ App::handle_key → App::run_command (not a slash command)
       └─ messages.push(Message::User(input)); start_turn()
            └─ spawn_agent_turn(provider, tools, gate, &messages)     [core]
                 └─ OS thread: rounds until the model settles
                      round: spawn_turn(provider, working)
                        ├─ send(TurnStart)              (swallowed by the loop)
                        ├─ POST {baseUrl}/chat/completions {stream:true, tools?}
                        ├─ per SSE frame: send(Token(chunk)) | accumulate tool_call
                        ├─ stream end: send(ToolCall(…)) per accumulated call
                        └─ send(TurnEnd)                (swallowed unless final)
                      calls present: gate → execute → send(ToolResult(…)); next round
                      no calls (or failed round): loop emits the turn's TurnEnd

per frame (event_loop polls keys with 50ms timeout):
  └─ App::pump()
       ├─ tick += 1                      (drives the spinner)
       └─ loop on try_recv():
            ├─ Empty         → stop for this frame (turn stays in flight)
            ├─ Disconnected  → agent thread died, no TurnEnd (#172): set
            │                  turn.error, finalize (below)
            ├─ TurnStart     → no-op (tokens follow)
            ├─ Token(t)      → turn.text.push_str(t)     (rendered live)
            ├─ ToolCall(c)   → flush pending text as Assistant, push ToolCall
            ├─ ToolResult(r) → push ToolResult
            ├─ Error(m)      → turn.error = Some(m)
            ├─ Heartbeat     → ignored (liveness probe, #170)
            └─ TurnEnd       → finalize (below)

finalize:
  ├─ turn taken out of App.turn (turn = None)
  ├─ if !turn.text.is_empty() → messages.push(Assistant(text))
  ├─ if turn.error            → messages.push(Error(msg))   (after the partial)
  └─ if queued                → push it as User + start_turn() immediately
  └─ session.append(everything new this frame)   (failure → one System
     note + persistence off for the rest of the run)

Rendering: the live area is a window onto the transcript — the
unflushed tail with `turn.text` at its live edge. Finalized rows flush
into the scrollback above the viewport exactly once
(`scroll.rs` + `ui::segments_into`) and are never redrawn by ratatui.

## Tool rounds (the agent loop)

`spawn_agent_turn` (`crates/core/src/agent.rs`) wraps `spawn_turn`:

- One user turn = one `TurnStart` … one `TurnEnd` on the consumer side, no
  matter how many provider rounds happened. Per-round `TurnStart`/`TurnEnd`
  framing is swallowed; the loop emits the final `TurnEnd` itself (settled
  round, failed round, or round cap).
- A round that ends with tool calls is followed by interception
  (`ToolGate`: allow / deny(reason) / mutate), JSON-object validation,
  sequential execution in proposal order, and exactly one `ToolResult` per
  call. Refusals, unknown tools, and tool errors are error *results*
  (`is_error`), never `Event::Error` — the model sees why and can adapt.
  The loop's internal history grows with the assistant text (if any), the
  calls, and the results before the next round.
- `Event::Error` still means the turn itself failed (wire error, or round
  cap: `"tool loop did not settle within N rounds"`), always immediately
  before `TurnEnd` — the single finalization path is unchanged.
- Panics (#172). A tool (or the gate) that panics is caught around
  `decide_and_execute` (`catch_unwind`) and becomes an `is_error`
  `ToolResult` (`tool `x` panicked: <message>`; `&str` and `String`
  payloads both read) — the model adapts and the turn continues. A provider
  thread that panics closes its round's channel with no `TurnEnd`; the loop
  tracks whether the round saw one and, if not (and no `Error` came), sends
  `Error("provider thread ended without finishing the round …")` then
  `TurnEnd` — never a clean settle. Calls from such a round are not
  dispatched.
- Closed channel, no `TurnEnd` (defence for an agent thread that died anyway):
  every consumer treats it as a **failed turn**, never a success. `pump`
  sets the turn error and finalizes (partial text kept, `Error` entry,
  queued input fires); headless returns `Err`/`Failed` (non-zero exit); the
  subagent drain returns `Err`. A *dropped* `Receiver` (Esc) is the other
  direction and is never reported as a failure. Empty is not closed: an
  open, quiet channel leaves the turn in flight.
- The terminal side: ratatui's panic hook restores the terminal on any
  thread, so `hi-tui` wraps it after init (`guard_panic_hook`) and lets it
  run only for UI-thread panics. Worker-thread panics (tools, providers,
  task monitors) neither restore nor print while the TUI runs; they surface
  as results/errors. Headless keeps the default hook (stderr).
- Cancellation is unchanged: dropping the `Receiver` fails the loop's next
  send and everything unwinds.
- Subagents (#4) ride this contract one level down: a subagent is an
  in-flight tool call, so a cancelled turn abandons its thread exactly as
  it abandons a running `bash` (a tool already running may finish). But
  the subagent does **not** self-stop on its own: `SubagentRunner::run`
  owns and drains the subagent's `Receiver`, so its sends keep succeeding,
  and the parent loop is blocked inside the tool and sends nothing (#170).
  So the dropped `Receiver` stays the only cancellation signal, probed by
  sending: `Event::Heartbeat` (no content; every consumer ignores it) is
  sent on the parent's channel via `Tool::execute_with_probe`'s
  `alive: &dyn Fn() -> bool`. The runner drains with `recv_timeout`
  (250ms) and probes on every quiet interval and at least once per
  interval under a steady stream. A failed probe flushes the completed
  transcript, drops the subagent's `Receiver` and returns `Err`; the
  subagent's loop then stops at its next send — no further provider
  round, no further tool call.

## Cancellation (Esc)

- `handle_key(Esc)` while a turn is in flight drops the `Turn` (which owns
  the `Receiver`). The provider thread's next `send` fails → it returns.
  Partial output already pumped into `turn.blocks` is **kept**: `handle_key`
  itself pushes the partial `Assistant` message (only when there are blocks)
  and appends it to the session file at once via `persist_last` (#181) — key
  handling runs before `pump` captures its `added_from` marker, so no later
  frame append would ever cover it. Esc before any token persists nothing.
  `/new` and `/resume` are refused mid-turn (esc first), so they never see a
  partial; quitting mid-turn (Ctrl-C) drops the unfinished reply with the
  process, unrecorded.
- Esc can land after a `ToolCall` was pushed (and persisted) but before
  its `ToolResult`, leaving an orphan call. Nothing is written to history
  or the session file for it; instead `wire_messages` repairs every
  request: each `ToolCall` with no matching `ToolResult` gets a
  synthesized `role: "tool"` message (same `tool_call_id`) after the
  results that do exist for that assistant message and before the next
  non-result message, or at the end. Its text says the call was cancelled
  before a result was recorded and the tool may have run, partly run, or
  still be running. This covers Esc, quitting mid-turn, crashes and old
  session files alike (tests: `an_orphan_*`,
  `esc_during_a_tool_then_a_new_message_sends_a_well_formed_request`).
- Esc also clears the auto-trigger arm (`auto_armed`), so a cancelled
  turn's mandate cannot fire after the next unrelated human turn.
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

## Background-task completions (#5)

Tasks run outside the turn entirely (`crates/core/src/tasks.rs`), so they
enter the flow at the frame pump: `App::pump` drains the manager's
completion channel each frame, before the turn borrow. Each completion
becomes a `Message::System` note in the transcript; if a turn is in
flight the note lands inside the frame's persist range (the same
finalize append persists it), and an idle session persists it
immediately. Either way the fact rides the next `start_turn` payload —
the running turn's history snapshot is untouched. Nothing here starts a
turn — unless the human opted in with `settings.taskTurns` (#72,
AD-0022). Then the idle path may spend one unit of the per-session
budget and start a turn itself, carrying a marked `(auto-trigger)`
User mandate so the model knows the turn is machine-initiated and the
session file stays honest. Completions folding while a turn runs only
arm the trigger: when the turn ends, a queued human message fires and
consumes the arm (it already carries the notes; the human's
instruction dominates), otherwise the mandate turn fires. The budget
is a constant (10), not a preference — the spend that exhausts it
pushes one dim note explaining the new silence.

## Model switching mid-turn

The `/model` picker rebuilds `App.provider` (+ system prompt, thinking
level) for **future** turns. A running turn finishes on its original
provider — `spawn_turn` holds its own `Arc` clone, so swapping `App.provider`
cannot affect it.

## Error path (wire level)

`OpenAiCompatProvider::respond` maps failures so consumers keep one finalize
path: partial `Token`s stand, then one `Event::Error` (message built by
`describe()`, e.g. `"chat request: server returned HTTP 401"`), then
`TurnEnd`. Malformed SSE payload → `Malformed` → same path. An in-stream
error frame (`error` member or `finish_reason: "error"`) → `DataLine::Error`
→ same path, so headless runs exit non-zero (tool-call fragments of that
round are dropped, usage still reported). Missing
`[DONE]` (server closed early) is tolerated as a complete stream.

## Invariants recap (all tested somewhere)

1. `TurnStart` first, exactly one `TurnEnd` last, always (a dead agent
   thread is the one case the consumer must synthesize it: `pump`'s
   Disconnected arm).

2. `Token`s concatenate to the exact reply; partial output survives errors
   and cancellation.
3. `Message::Error` never reaches the wire (`wire_messages` filters it).
4. At most one turn in flight; `queued` holds at most one merged message.
5. Dropping `Turn` is the only cancellation mechanism; providers must honor
   failed sends as "stop".
6. Every `ToolCall` event is answered by exactly one `ToolResult` with the
   same `call_id`; a user turn may span rounds but keeps exactly one
   `TurnStart` and one `TurnEnd` (the loop swallows round framing). The
   exception is a cancelled/aborted turn (Esc, quit, crash), which can
   leave a recorded `ToolCall` with no result: history keeps it as is,
   and `wire_messages` answers it on the wire (see "Cancellation (Esc)").
7. Finalized messages reach the session file at turn end (append + flush);
   only the in-flight turn is ever lost, and a persistence failure never
   blocks the conversation.
