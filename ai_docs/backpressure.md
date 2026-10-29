# backpressure: queued, bounded, shed at the edge

Paths are under `slot-0/proxima/` (read-only). Owner premise checked: the HTTP server does not queue.

## what proxima does
- HTTP: no queue. `ConnAdmission::request_admit` (`proxima-listen/src/admission/request.rs:90`) is an atomic counter:
  at `max_in_flight` it returns `RequestAdmit::Shed{AtCapacity}` (`:35`, `state.rs:66`) and h1 renders an in-band 503
  (`proxima-http/src/http1/serve.rs:61,388-390`). Anything waiting waits in the kernel accept backlog, unbounded and invisible.
- Telemetry: a producer-side `BoundedQueue` with an overflow policy and a separate drainer. Config surface is
  `OverflowPolicy {Block, Drop}` (`proxima-telemetry/src/config.rs:169`, used by `RingSink` `recorder/mod.rs:598`).
  `Block` is producer-assist (`config.rs:150-168`); `Drop` sheds and counts.
- The shared engine under it: `FailMode {DropOldest, DropNewest, FailClosed}` (`proxima-core/src/ring/bounded.rs:98`),
  `BoundedQueue::enqueue` (`:181`) returning `EnqueueOutcome`, and `SinkFront::emit` (`proxima-primitives/src/pipe/sink_front.rs:188`)
  returning `Admission {Accepted, Dormant, Dropped(DropReason)}` (`:41-60`), with `dropped()` and `drained()` counters.
  Re-exported as `proxima::pipe::{SinkFront, FailMode, Admission, DropReason}` (`pipe/mod.rs:295`). Demo: `examples/backpressure/main.rs`.
- The ring is a lock-free MPMC FIFO; capacity rounds up to a power of two, minimum 2 (`ring/mpsc.rs:190-197`).

## CreditGate
`credit_gate.rs:57,151` use `notify_waiters`: every release wakes every waiter, they race to re-check
(`:111`), so wake order is not arrival order, and nothing bounds how many wait. It is a weighted concurrency
limiter, not a queue; it cannot answer "busy", and neither telemetry nor HTTP uses it. Not extended, not used.

## how ss composes it
`emit(job)` is the front, N worker tasks `drain_one()` are the back; the job carries a oneshot for its reply.
No ss-side admission type: `Router` (already `Request -> Response`) emits, awaits the reply, and renders at the edge.
- `Accepted`: await the reply; workers run the existing `SearchPipe` / `IndexPipe`.
- `Dropped(NewestDiscarded | Refused)`: the job (and its reply sender) is gone at once -> wire `busy`.
- `Dropped(OldestEvicted)`: the evicted job's reply sender is dropped; its waiter sees a closed channel -> wire `busy`.
  The `DropReason` is the whole error; `ErrorKind::Busy` is only written in `daemon.rs` at the edge.
- `queries_busy` / `index_busy` in status are the fronts' `dropped()`.

## strategy per lane (Settings: `*_cap`, `*_depth`, `*_overflow`)
| lane | cap | depth | overflow | why |
|---|---|---|---|---|
| search | 8 | 64 | `drop_newest` | FIFO fairness: those already waiting keep their place, the excess is told `busy` now and retries |
| index / embed | 2 | 8 | `drop_oldest` | a queued index run is superseded by a newer one for the same tree, and every dropped run is recoverable by rescan (events are hints, rescan is truth) |

Accepted values: `drop_newest`, `drop_oldest`, `refuse` (FailClosed: same outcome as drop_newest, different `DropReason`).
Not offered: `block`. Every request is its own spawned task, so blocking just relocates the unbounded queue into
parked tasks and defeats the depth bound; the telemetry `Block` works because one producer thread is throttled.

## differences from the old `Admit`
- depth counts jobs waiting for a worker, not jobs running: a burst faster than a worker wakes can see a full ring
  while workers are idle. Effective depth is `depth.next_power_of_two()` (min 2).
- status lost `queries_running` / `queries_waiting`: `SinkFront` exposes `dropped()` and `drained()` but no queue length.
  Asking proxima to expose `len()` on the engine would restore them; not edited here.
- `block` is absent (above).
