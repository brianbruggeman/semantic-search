# Code review: idle model release

## Prior finding dispositions

- Daemon settings release assertion: revised. `ss/src/daemon.rs:679`–`705` routes both production factories through `configured_pool`; `ss/src/embed.rs:1051`–`1086` exercises that helper with a factory-owned channel sender and observes disconnection after requests on each lane.
- Readiness restoration assertion: revised. `ss/src/embed.rs:999`–`1029` observes readiness after warm-up, after expiry, and after successful query and index reloads, then observes both models' subsequent release.

## Worker state machine

`ss/src/embed.rs:423`–`468` stores each model in a worker-local `Option`. A loaded worker with a timeout uses `recv_timeout`; a cold worker uses blocking `recv`. Timeout clears readiness if that worker contributed to the counter, drops its model, and returns to the receive loop. Successful reload increments the counter only when the worker's local `ready` flag is false. On disconnect the same flag guards the decrement. The factory-error path returns the error to the request and loops with the empty slot, allowing a later request to retry. A job obtained before expiry runs synchronously on the owning worker; the next timed receive begins after its completion. No additional production state-machine defect was identified by this inspection.

`ss/src/embed.rs:933`–`987` adds positive-interval tests for both lanes, observing the model during active work, after work completes, on expiry, and after reload. These tests use wall-clock deadlines; a sufficiently delayed test executor can miss the interval in which the model remains loaded. Queued jobs across expiry and warm-up failure followed by recovery are not asserted by these new tests. Real runtime memory release is not established by these closure fixtures.

No tests were run as part of this review.
