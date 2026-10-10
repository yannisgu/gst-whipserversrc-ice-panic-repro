# whipserversrc: process aborts when ICE gathering completes twice

Reproduction for a crash in the WHIP server signaller of
`gst-plugin-webrtc` (`net/webrtc/src/whip_signaller/imp.rs`), present in
0.15.4 and `main`.

The signaller sends the SDP answer when webrtcbin's `ice-gathering-state`
becomes `complete`, and takes the answer channel for the session out of its
map with

```rust
let tx = settings
    .sdp_answer
    .remove(&session_id)
    .expect("SDP answer Sender needs to be valid");
```

webrtcbin can go back to `gathering` and reach `complete` a second time for
the same session. The notify handler then finds no channel and panics. The
panic happens inside a GObject notify callback and cannot unwind into C, so
the whole process aborts:

```
thread 'main' panicked at .../gst-plugin-webrtc-0.15.4/src/whip_signaller/imp.rs:685:38:
SDP answer Sender needs to be valid
thread 'main' panicked at .../library/core/src/panicking.rs:225:5:
panic in a function that cannot unwind
thread caused non-unwinding panic. aborting.
```

Fix: ignore the later completions, the answer was already sent
([`fix.patch`](fix.patch), branch
[`fix/whip-server-ice-gathering-complete`](https://gitlab.freedesktop.org/me5/gst-plugins-rs/-/tree/fix/whip-server-ice-gathering-complete)
on `main`, and
[`fix/whip-server-ice-gathering-complete-0.15`](https://gitlab.freedesktop.org/me5/gst-plugins-rs/-/tree/fix/whip-server-ice-gathering-complete-0.15)
on top of 0.15.4).

## Where it happened

In an application that serves WHIP itself (`whipserversrc` with
`signaller::host-addr=""`, offers passed to the signaller's `post` action),
with a real WHIP publisher over a VPN and a STUN server configured. While a
publisher reconnected repeatedly (each time: delete the old session, restart
the source, post the new offer), the process aborted on the second
reconnect. With the fix applied, 30 reconnects ran through; the "completed
again" case was hit once, right as the new session's ICE transports were
created. It depends on network timing, which is why the repro below
triggers it deterministically.

## Repro

`src/main.rs` creates a `whipserversrc` with the HTTP server disabled and
posts offers to the signaller's `post` action. No peer connects, only ICE
gathering runs. Between sessions it deletes the previous session and
restarts the source, like a reconnecting publisher.

```
whipserversrc-ice-panic-repro [bundled|unbundled|renotify] [sessions] [stun-uri]
```

- `renotify`: after the answer was sent, notifies `ice-gathering-state`
  again (with the state still `complete`), which is what webrtcbin does when
  gathering completes a second time. Aborts on the first session with 0.15.4.
- `bundled` / `unbundled`: plain offers, with and without BUNDLE (one or two
  ICE transports). These did not hit the race in local runs (30 sessions,
  with and without STUN), see `results/`.

## Build and run

Needs GStreamer 1.28 development files and Rust.

```
cargo run -- renotify 3                # 0.15.4 from crates.io: aborts
```

To test the fix, uncomment the `[patch.crates-io]` section in `Cargo.toml`
and run again: all sessions complete.

## Results

`results/` has the output of the runs on Linux (GStreamer 1.28.7):

- `0.15.4-renotify.txt`: abort on session 1.
- `fixed-renotify.txt`: fork branch, 10 sessions, no crash.
- `0.15.4-unbundled-stun.txt`: 30 natural sessions, no crash.

The `GStreamer-CRITICAL ... Trying to dispose element webrtcbin, but it is in
PAUSED instead of the NULL state` messages in the multi-session results come
from `webrtcsrc` disposing a session's bin on teardown; they are unrelated
to this crash.
