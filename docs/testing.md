# Test harnesses: what exists, how to run it, what it cannot prove

Issues #49, #50 and #51. Everything here runs offline. None of it talks to Microsoft, WhatsApp or Cloudflare.

## Relay end-to-end (`wrangler dev`)

`relay/e2e/e2e.ts` starts `wrangler dev` in local mode, waits for `/health`, runs the protocol checks over real HTTP and a real WebSocket, then kills the whole process tree.

```
cd relay
bun install
bun run test:e2e
```

- **Secrets:** `RELAY_TOKEN` and `CLIENT_STATE` are random per run, passed as `--var` to the local dev server only. Nothing is written to a file and nothing is committed.
- **State:** the Durable Object storage goes to a throwaway folder (`--persist-to`) that is deleted afterwards, so a run never sees an earlier run's `dropped` count.
- **Port:** a free port is picked each run. The wait for `/health` has a 120 s deadline, because `workerd` is slow to start on a cold CI runner. Measured on the author's machine: a full run takes about 5 s.
- **Cleanup:** on Windows `taskkill /T /F`, elsewhere a process-group kill. The script then checks nothing still answers on the port and fails if something does.
- The file is `e2e.ts`, not `*.test.ts`, so the fast `bun test` job does not start wrangler.

| Protocol rule in `docs/DESIGN.md` | Checked by |
|---|---|
| `GET /health` is 200 `ok` | "GET /health returns ok" |
| `validationToken` echoed as `text/plain` | "validationToken is echoed as text/plain" |
| Notifications with no socket counted; `hello.dropped` on the next connect | "notifications with no socket are counted in hello.dropped" (runs before any socket connects) |
| `/ws` 401 for a wrong or missing token; upgrade needed | "GET /ws: wrong or missing token is 401, no upgrade is 426" |
| `hello` on connect | "GET /ws with the right token gets hello ..." |
| `ping` gets `pong` | "ping gets pong" |
| Right `clientState` gives a `notify` frame with ids only | "a good notification arrives as a notify frame with ids only" (also checks the message body and the client state are not in the frame) |
| `lifecycle` frame | "a lifecycle notification arrives as a lifecycle frame" |
| Wrong `clientState` gives an `alert` frame and nothing else | "a wrong clientState gives one alert frame and nothing else" |
| A second socket closes the first with code 4000 | "a second socket closes the first with code 4000" |

**Not covered:** the one-alert-a-minute throttle (needs a 60 s wait or a clock seam in the Worker, which this issue forbids changing), the 64 KB body limit, the 10-second Graph endpoint-check deadline, and anything against the deployed Worker.

**CI:** the `relay` job in `.github/workflows/ci.yml` runs `bun run test:e2e` after the typecheck. It uses Bun and installs wrangler through `bun install --frozen-lockfile`; the 15 minute job timeout is far above the measured time. It has not run on a GitHub runner yet: if `workerd` fails to start there, read the log before changing the test.

## Teams backend against a mock Graph server (#49)

**Status: partial.** The in-process mock server and most scenarios already exist inside the crate; the part the issue asks for in `crates/teams/tests/` cannot be built without changing `src`.

Why: the base-URL override is `#[cfg(test)]` only (a test of its own, `a_release_build_has_no_way_to_redirect_graph` in `graph.rs`, proves a release build has none). A file in `crates/teams/tests/` is a separate crate that links the normal library, where the override does not exist. Moving the override behind a Cargo feature would let `tests/` use it, but then the feature must be proven off in release builds, and the token endpoint (`auth.rs`) has no override at all. That is a `src` change, so it is reported, not made here.

What exists now (all in `src`, with the mock in `src/testkit.rs`, which is not compiled into a release build):

| Scenario from the issue | Where it is tested |
|---|---|
| First load (chats, paging) | `backend/tests.rs`: older-history and catch-up tests drive `chats` and `messages` through the mock; `graph.rs` tests the page and `nextLink` host check |
| Send success and failure (NOT SENT) | `backend/tests.rs`: `text_goes_out_as_escaped_html`, `a_refused_reply_says_the_scope_is_missing`, the media send tests |
| 429 with `Retry-After` | `graph.rs`: `graph_errors_carry_status_code_and_message` (the message says `Try again in 17 s`). There is **no automatic retry**: the user sees the wait |
| Subscription `reauthorizationRequired` | `subscription.rs`: one PATCH and no second request, and a failed one is an error |
| 401 then refresh | **Not covered end to end.** Token refresh is unit-tested (`auth.rs`), but the token endpoint cannot be pointed at a mock |
| Per-user subscription limit (10) | **Not covered.** Orphan cleanup before create is unit-tested (`subscription.rs::orphans`), the limit error from Graph is not mocked |

To finish #49, a Teams change is needed: a `test-support` Cargo feature (or `pub(crate)` constructors) that sets both base URLs, plus a test that a release build cannot enable it. Then the two uncovered rows and a `tests/` copy of the table become possible.

## WhatsApp with a fake transport (#51)

`crates/whatsapp/tests/fake_transport.rs` plugs fake transports into the whatsapp-rust 0.7 `Bot` (the same builder `session_release.rs` uses). All fakes are in that one file, so a library trait change breaks one file.

```
cargo test -p chatcli-whatsapp --test fake_transport
```

| Fake | Test | Asserts |
|---|---|---|
| Refuses to connect | `a_refused_connect_retries_with_backoff_not_in_a_fast_loop` | at least 2 and at most 15 connect attempts in 60 s of virtual time (the library's Fibonacci backoff), then a stop with no further attempts |
| Connects, closes at once | `a_socket_that_drops_at_once_does_not_reconnect_in_a_fast_loop` | same bounds |
| Connects, never answers | `a_server_that_never_answers_is_one_attempt_until_the_library_gives_up` | exactly one attempt in the first 5 s, and shutdown finishes within the library's 20 s handshake deadline |
| Closes after 3 s | `a_socket_closed_after_a_few_seconds_also_backs_off_and_shutdown_stays_clean` | backoff bound and clean shutdown |

Each test also proves a stopped session never connects again (120 s of virtual time). The clock is paused (`start_paused`), the whole file runs in well under a second, the session file is a throwaway under the temp folder, and the client version is pinned so no request is made.

**What this does not prove, and why (#51 is partial):** the tests drive the library, not `chatcli_whatsapp::run`. `run` builds its own `TokioWebSocketTransportFactory` (`src/backend.rs`, `start_session`) and has no way to take another, so the TUI-visible strings (`CONNECTING`, `NOT LIVE` with a reason) and "late events from a stopped session are ignored" are still only tested with hand-built events in `src/backend_tests.rs`. To finish it, `start_session` needs a `#[cfg(test)]` seam that takes a factory and an HTTP client; then these four fakes can be reused to assert the events the TUI sees.

**One finding:** a stalled handshake keeps the library's `shutdown()` waiting up to its own 20 s handshake deadline. chatcli's `stop_session` wraps it in a 10 s `STOP_TIMEOUT`, so quitting is bounded at 10 s, then the session is dropped.

The fakes cannot finish the Noise handshake, so linking, QR and message flow stay with the hand-built events.

## `chatcli doctor`

Unit tests are in `crates/app/src/doctor.rs` (config, settings, data folder, WhatsApp file, single instance, text and JSON output, exit code). The Teams checks have their own tests in `crates/teams/src/doctor.rs`. A check that needs a live Microsoft account or the deployed Worker is not tested here.
