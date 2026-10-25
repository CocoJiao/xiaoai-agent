# XiaoAI Agent Notes

This file complements [README.md](README.md). The README is the user-facing
entry point; this document records engineering notes and invariants for future
coding agents working on this repository.

## Native Speech Frontend Invariant

The agent does not open an ALSA capture device and does not load `libvpm.so` or
`libaivs_sdk.so`. The firmware's device-specific `mipns-*` process remains the
sole owner of the real microphone path and continues to provide the matched
microphone-array processing, wake word, AEC, beamforming, and VAD. It sends wake
events and 16 kHz mono `S16_LE` PCM to the agent over the native
`speech.usock` datagram protocol.

The agent binds the standard path:

```text
/tmp/mico_aivs_lab/usock/speech.usock
```

The runtime launcher copies the firmware's `pns` init script to `/tmp`, removes
`-r opus32`, and bind-mounts that copy over `/etc/init.d/pns` for the current
boot. This makes the socket receive PCM rather than Opus without changing the
rootfs. Do not bypass `start-agent.sh` or restore `-r opus32` unless the agent
also gains a packet-preserving Opus decoder.

`mico_aivs_lab` must run during normal operation because its `common.usock`
services are used by the system TTS path. The launcher briefly stops it while
installing the takeover, lets the agent bind the standard speech path, and
self-bind-mounts that socket before restarting `mico_aivs_lab`. The mount point
makes native attempts to unlink the Agent socket fail with `EBUSY`; native
speech binding then fails while `common.usock` starts normally. Keep this
fail-closed protection in place whenever the real microphone frontend is
running.

The native `/data/mipns/dialog_continuous` mode is deliberately disabled. After
each answer, the agent finishes the current native dialog and requests the next
capture through `pnshelper`'s public `ubus` event. This segmented sequence is
required for reliable follow-up turns; restoring the native full-duplex marker
can make the LED turn off and prevent the next utterance from reaching the
agent.

Keep this ownership boundary intact:

```text
real microphone -> native mipns-* -> speech.usock -> xiaoai-agent -> external ASR
```

Supporting another model should normally reuse that model's own `mipns-*`
frontend instead of linking the agent directly to model-specific shared
libraries. Compatibility still depends on the firmware providing a compatible
`mipns` executable, socket protocol, PCM mode, and `pnshelper` control path.

## Fault Handling Boundary

Two owners, two scopes. Keep them apart so they never fight over the same
takeover:

| Owner | Scope it watches | Its only allowed action |
|---|---|---|
| the launcher's watchdog loop (`start-agent.sh`, `WATCHDOG_INTERVAL`, default 5 s) | process level: agent process gone, `speech.usock` inode replaced, socket protection mount disappeared | `start-agent.sh restart` — tear the takeover down and install it again |
| the agent process itself (`interrupt_turn` / `yield_to_native` in `main.rs`) | protocol level: one turn aborted, capture or ASR failed, `SpeechServiceEvent::Fatal` | `start-agent.sh stop` — finish the native dialog, drop the socket protection, bring native `mico_aivs_lab` / `pns` back, then exit |

Rules:

- The watchdog only observes. It never sends protocol messages: no
  `finish_dialog`, no `stop_capture`. Conversely the agent never re-installs the
  takeover: getting the microphone back requires a full `start-agent.sh start`
  (pns override, protected `mico` restart, `pnshelper` follow-up request), not a
  binary restart.
- A speech-service loop error is **not** instantly fatal. `supervise_service`
  restarts the receive loop on the *same* socket and with the *same*
  `ServiceState` (registered peer, current turn, counters) up to three times with
  a 200 ms / 1 s / 3 s backoff; only then does it emit
  `SpeechServiceEvent::Fatal`. Re-binding is not an option there: the launcher
  keeps `speech.usock` bind-mounted, so the agent cannot unlink it.
- Who exits, and when:
  - `Fatal` / the event channel closing = the frontend is gone. Yield, then exit
    (code 0 if the yield succeeded, 1 if it did not) — staying alive serves nobody.
  - **Consecutive turn failures** = the frontend is still alive, it just cannot
    answer. Yield; if the yield did not succeed, **keep running** and reset the
    counter instead of exiting: exiting there leaves a mute speaker with the
    watchdog already stopped (`stop_watchdog` runs early in the restore sequence).
    Do not retry the yield immediately — a second `start-agent.sh stop` usually
    fails for the same reason (the launcher lock, or an unkillable agent) and turns
    into repeated stop/start of the native services.
- Both sides funnel through the launcher's `launcher.lock` (`mkdir` lock). When
  the lock is held, the loser logs and returns; it must not retry in a loop, or
  the two recoveries will race each other.
- The `stop` exit code is part of that contract, and it is the only thing that
  separates "the frontend was never touched" from "the frontend was half
  dismantled":
  `0` restored · `3` untouched (launcher lock busy) ·
  `4` pns/watchdog stopped but mico still up · `5` half-dismantled (protection
  still mounted, native files not restored, native services not back).
  Only `3` lets `yield_after_consecutive_failures` return `None` and keep running;
  `4`/`5`/timeout all exit. A missing or non-executable launcher never reaches `3`
  (it becomes `exit_code: None` or `126`, both of which exit) — the docs must not
  read more optimistically than the code.
  `5` is folded into `succeeded()` itself, not into one call site: its last two
  steps (`start_native_mico`, `pns start`) run after `rm` + `mv`, so the inode
  already points at the native socket while mico/pns are down — socket in place,
  microphone dead. When the script and the inode disagree, the script knows more.
  `succeeded()` has three call sites (`Fatal` ×2, consecutive failures ×1); a rule
  written at one of them leaves the more common `Fatal` path unguarded.
  Changing one side without the other silently turns "untouched" into "half
  dismantled" — that pairing is why the codes live in `start-agent.sh` with a
  pointer to `YieldOutcome` in `main.rs`.
- Yielding stops the watchdog on purpose. After a yield the speaker is native
  xiaoai again and stays that way until a human runs `start-agent.sh start`.
  That is the intent: a speaker that answers as native xiaoai is better than one
  that can be woken but never answers.
- "Did the yield actually hand the microphone back?" is decided by a **fact**, not
  by the launcher's exit code: `SpeechService::ownership()` remembers the inode of
  the socket it bound, and `SocketOwnership::taken_over()` checks whether the path
  still resolves to it. The restore sequence `mv`s the native socket back, so the
  inode changes — and the launcher may return non-zero ("could not kill the agent")
  *after* the native frontend is already serving. Neither signal alone is correct.
- The yield command runs through `ShellRunner::run_detached` / `Device::run_detached`
  (no `kill_on_drop`). On the normal `run` path a 180 s timeout would kill the
  `sh` that is halfway through `restore_native_frontend`, leaving the takeover
  half-dismantled. Never route TTS/playback through the detached path: those must
  die with the turn.
- Known gap, tracked in issue #13: a single `speech.usock` send/recv error still
  escalates to `SpeechServiceEvent::Fatal`. Grading those errors (warn and
  continue for recoverable `errno`s, fatal only for an unusable socket) needs
  its own bounded-retry design and is not implemented yet.

## Testing Rules

- **A fix for a hole needs a test that fails without the fix.** Prove it by
  reverting the fix, watching the new test go red with the *expected* wrong value,
  then restoring. "Green CI" cannot tell a test that guards something from a test
  that merely runs; only the reverse check can. The cross-file contracts in this
  repo (yield exit codes, `ServiceState` survival across restarts, config section
  rollback, `/health` socket ownership) were each caught or confirmed this way.
  **Assert that the reverting edit actually applied** (count the match before
  writing): a `replace` that silently matches nothing yields a false green and
  you will report "verified" for a check that never ran.
- Prefer real filesystem facts over flags that stand in for them: `rm` + `rename`
  for an inode change, a real `UnixDatagram` for a socket error, a real file for a
  path that must exist.

## Interrupting Playback

Interrupting speech needs **both** halves; either one alone produces the bug the
user actually notices ("I said stop and it read out one more sentence"):

- `PlaybackGate` (`tts.rs`) — a monotonic token. Today only the wake branch calls
  `begin()`; `/stop` and per-sentence streaming are the next step, and this list
  must not pretend otherwise. Every path that can start audio — native, remote
  synthesis, and the remote→native fallback — goes through `may_play(token)`
  first. This stops work that has **not** started yet.
- `device.abort_current_output()` — kills `tts_play.sh` / `miplayer`, which are
  separate processes. `task.abort()` only drops the future; the audio plays on.
  This stops the sentence that is **already** sounding. When music was only
  paused/ducked, kill `tts_play.sh` alone instead: `killall miplayer` would take
  the paused music with it. Known gap in that case — a reply synthesized through
  `miplayer` finishes on its own (music is off by default).

A reply whose token expired mid-synthesis returns `Ok(())` and plays nothing: it
was interrupted, not failed, so it must not count towards the consecutive-failure
yield.

## Suppressing Capture During External Playback

`miplayer` output is **not** in pns's AEC reference. While an external TTS reply
is sounding the microphone hears the answer itself, the local VAD calls it
speech, and the capture runs to `max_utterance_s` (`end=max_duration`) before the
ASR transcribes the agent's own words back as a user question. Measured on OH2P
1.62.2: capture was requested 155 ms after synthesis finished, the follow-up
stream opened 143 ms later, the first "speech" landed ~3.2 s after that (player
startup + buffering), and the utterance ended at 14.7 s with `end=max_duration`.

The fix is a **capture-side quiet window**; the `speech.usock` protocol sequence
is unchanged.

- `PlaybackGate` carries the window (`mute_mic_for` / `mic_muted` /
  `settle_quiet_window`) next to the interrupt token. `begin()` — wake, and
  `/stop` later — clears it: the wake branch already kills the player, so the
  sound has stopped and keeping the mute on would swallow the user's next
  sentence.
- `capture.rs` drops PCM while the window is open (it never reaches the
  collector, so no `SpeechEvent` is produced) and **holds the idle timeout**.
  Holding it is load-bearing: `runtime.session_idle_timeout_s` defaults to 20 s,
  shorter than a long reply, so a session would otherwise be killed mid-sentence.
- The window closes on a **fact, not arithmetic**: the default playback command
  truncates `/tmp/xiaoai-tts-player.log` on every start, so `EndReached` in that
  file means the sentence finished. A watcher task polls it (200 ms; the device's
  busybox does support `sleep 0.2`) and shrinks the window to
  `tts.playback_tail_ms`. Until that signal arrives the window is a generous cap
  (`grace + 2 × duration + 10 s`): a missed signal must degrade to "muted a bit
  longer", never to "echo gets through".
- Only **non-pns** playback mutes. `native` (`tts_play.sh`) and the fallback to
  it go through pns, whose AEC knows its own output; muting there would discard
  barge-in for nothing.
- Duration comes from the WAV `data` chunk measured against the real file size,
  not from the declared length field: backends emit streaming headers
  (`0x7FFFFFFF`) and trusting them yields a multi-hour window.
- `DEFAULT_PLAYBACK_COMMAND` and `PLAYER_LOG_PATH` must name the same file; a
  test pins it because changing one silently degrades the window to the cap.
- The window's `generation` makes a late end-signal from the previous reply a
  no-op — otherwise it would close the *next* reply's window while that reply is
  still sounding.

What this deliberately gives up: **barge-in without the wake word**. That path is
the one the echo polluted (every `end=max_duration` sample came from it). Keeping
it needs real AEC — subtracting the known playback audio from the capture — not
the absence of suppression; tracked in issue #33. Wake-word interruption is
unaffected: it is decided by `mipns` in firmware and arrives as
`Turn(activation=Wakeup)`, independent of whether the VAD is being fed.

A config field added under `tts` stays hot-reloadable only because
`classify_config_change` lists the whole section as `applied` — moving these
fields into `device` or `capture` would make them restart-only.

### Verifying it on the device

`verify-quiet-window.sh start` records the current log offset (and prints a
config precheck) plus the three interactions to perform;
`verify-quiet-window.sh check` then asserts against **only** the bytes appended
since that offset. That indirection is the point: `agent.log` is append-only
across days, so a plain `grep` would count last week's `end=max_duration` and
turn "verified" into an illusion. `scp` does not preserve the exec bit (see
`docs/flash-troubleshooting.md`, section 5), so `chmod +x` after copying.

`check` exits 0 only when all five assertions pass — the window actually opened
(`CAPTURE_MUTED`), it was shrunk by `EndReached` rather than the cap, no
"no EndReached" warning leaked out of an interruption, no utterance reached
`end=max_duration`, and no `turn failed:` line. The last one used to be
`timed out waiting for user speech`, which **cannot** appear: `is_capture_timeout`
rewrites that error into `info!("session idle timeout")` at all three capture
call sites, so it only reaches the log via `error!("turn failed: {err:?}")` when
the turn genuinely fails. `turn failed:` is therefore the observable end of the
echo chain (answer transcribed as a question → LLM failure).

Both `end=max_duration` and `turn failed:` can also come from something other
than echo — another sound source in the room, a network or backend outage — so
the scripts point at the following `USER_ASR` line instead of asserting a cause.
`check` bails with exit 2 (no verdict, not a pass) when the log is smaller than
the recorded offset or the slice has fewer than 20 lines: negative assertions
over an empty slice are all vacuously true, and "all PASS" must never mean "we
looked at nothing". It additionally prints each mute→unmute interval, which is
read against the reply's length: ≈ audio duration + 3 s means the window
followed playback, close to `grace + 2 × duration + 10 s` means it ate the cap
— the one thing a green run must not hide.

Two preconditions, both checked by `start`: `tts.provider` must be an external
player (`native` goes through pns, never mutes, so there is nothing to measure),
and `tts.playback_command` must be left at the default. A custom command has
`wait_script: None`, so no watcher is spawned at all — assertion 2 fails by
design and assertion 3 is vacuous, and the script says so rather than blaming
the window. `RUST_LOG` at `info` is enough.

## Control Plane Auth

The local control plane (`server` section) is off by default and binds loopback
unless configured otherwise. Its whole security model rests on one property:
**cross-site requests cannot carry custom headers**, so a request without
`Authorization: Bearer` can only come from something that cannot set headers.

Two rules follow, and both are load-bearing:

- **Every route except the panel (`/`) requires auth**, and it is enforced by a
  router-level layer (`require_token`), not by each handler remembering to call
  `authorize`. Adding a route without the layer fails the route-level test.
- **State-changing endpoints must not be GET.** An unauthenticated `GET /stop`
  would be reachable from any web page (`<img src=…>`), because the victim's
  browser needs no header to send it. Such a page would be a CSRF pivot for the
  speaker: any site could make it talk or stop talking.

The panel itself is public because it is inert: one static HTML file, no device
data, token typed once and kept in `localStorage`. Never put the token in a query
string — it leaks into history, proxy logs and `Referer`.

## Configuration Hot Reload

`SIGHUP` re-reads `agent.yaml` in place. The reload is deliberately conservative —
it never halves a config:

| Section | On reload |
|---|---|
| `runtime.session_idle_timeout_s`, `capture`, `llm`, `agent`, `music`, `mcp` | applied on the next turn (read per use, or re-registered by rebuilding the tool server) |
| `device`, `asr`, `airplay` | **needs restart** — these are snapshotted at startup (`Device` keeps its template/timeout copy, `AsrClient` picks a provider, the AirPlay receiver owns its listener). The reload keeps the old values and warns |
| `runtime.speech_socket` | **rejected** — `SpeechService` is already bound to the path; switching it would leave mipns and the agent talking to different sockets |

Rules that must survive future refactors:

- `ConfigHandle` only ever holds the configuration that is **actually in effect**.
  Pending (`needs_restart`) and rejected sections are rolled back to their old
  values before the swap (as are rejected ones), so reading the handle never
  reports a change that the running process did not take.
- Only a **hot** section triggers the swap and the tool rebuild. A `SIGHUP` whose
  diff is entirely `needs_restart` / `rejected` is a warning-only no-op — otherwise
  "file differs from handle" would be permanent (the handle keeps the old values
  by design) and every later `SIGHUP` would rebuild the tool server and reconnect
  MCP for nothing.
- The reload runs in a **spawned task**, never inline in the event loop: MCP
  reconnect is bounded by `2 × server.timeout_s` per server (20 s default), and
  during that window `speech.recv()` must keep being polled or wake events and
  `Fatal`-triggered yielding are delayed. Overlapping `SIGHUP`s are **coalesced**:
  a request arriving mid-reload is remembered and triggers one more round that
  re-reads the file (never silently dropped — otherwise the *first*, possibly
  wrong, edit would win), and the running flag is released by an **RAII guard** so
  a panicking reload cannot leave hot reload disabled forever.
- Inside `AgentRuntime`, the config snapshot and the tool server are swapped
  **together** through one `AgentView` (`RwLock<AgentView>`), and one view is read
  per turn and threaded down. Swapping them separately would leave a window where
  the preamble says "you have tools" while the server has none (or the reverse).
- A malformed `agent.yaml` must never take the speaker down: `AppConfig::load`
  failing means "keep the old config and warn", not "exit".
- `agent.tools_enabled` and any tool-facing section (`weather`, `web_search`,
  `music`, MCP endpoints) only take effect through a **rebuilt** `ToolServer`.
  The 17 built-in tools each capture an `Arc<AppConfig>` snapshot, and the MCP
  tools are registered into the same server, so `AgentRuntime::reload` rebuilds
  both and reconnects MCP. Conversation history and `AgentControl` are kept —
  a reload must not wipe the dialog the user is in the middle of.

## Cross Compilation

Do not assume this project cannot be cross-compiled locally.

The local workspace already has a working Rust cross-build path for the speaker
target, and `xiaoai-agent` has successfully produced an ARMv7 hard-float Linux
binary:

```bash
cargo +1.96.0 zigbuild --release --target armv7-unknown-linux-gnueabihf.2.25
```

This host uses `cargo-zigbuild` plus `zig` for the ARMv7 Linux native build and
link steps. A plain `cargo +1.96.0 build --release --target
armv7-unknown-linux-gnueabihf` may fail in shells where
`arm-linux-gnueabihf-gcc` is not present on `PATH`, because dependencies such
as `ring` and `aws-lc-sys` compile C/assembly during their `build.rs` scripts.
In that case, use the `zigbuild` command above rather than assuming cross
compilation is broken.

The speaker firmware uses glibc 2.25. Building with
`--target armv7-unknown-linux-gnueabihf` without the `.2.25` suffix can still
produce a valid ARMv7 hard-float ELF locally, but that binary may fail on the
speaker with missing symbols such as `GLIBC_2.28`, `GLIBC_2.29`,
`GLIBC_2.32`, `GLIBC_2.33`, or `GLIBC_2.34`. For binaries intended to run on
the speaker, keep the `armv7-unknown-linux-gnueabihf.2.25` target suffix.

Known-good output:

```text
xiaoai-agent/target/armv7-unknown-linux-gnueabihf/release/xiaoai-agent
```

The resulting binary is expected to be:

```text
ELF 32-bit LSB pie executable, ARM, EABI5, dynamically linked,
interpreter /lib/ld-linux-armhf.so.3
```

That ABI matches the speaker userland. The agent has no direct dependency on
model-specific Xiaomi shared libraries; its expected ELF dependencies are the
standard glibc runtime libraries `libc`, `libm`, `libpthread`, and `libdl`. If a
future shell cannot find a bare `arm-linux-gnueabihf-gcc`, inspect the existing
Rust target/build setup and the `cargo-zigbuild` glibc 2.25 path before rewriting
the toolchain assumptions.

## Runtime Safety

- Avoid NAND writes on the speaker whenever possible; prefer `/tmp` for logs,
  probes, PID files, and temporary audio.
- Do not commit real `agent.yaml` secrets. Use sanitized examples for committed
  config.
- Do not reconnect `mipns` to the native AIVS speech socket; that can bring back
  double answers and cloud-side control actions.
- Stop `mico_aivs_lab` only inside the launcher's coordinated transition. The
  launcher must protect the Agent socket before restarting it, and must restore
  native speech before leaving `mipns` running without the Agent.
- When testing `speech.usock` protocol changes, prefer `xiaoai_asr_probe` first,
  then wire the validated behavior into the main agent.
