# Working on NoSlacking

NoSlacking is a native Slack client in Rust, built on egui/eframe and the
[fastframe](https://github.com/crmne/fastframe) crates, in the style of
ZapFast and Spotifast.

## Shape

- The interface (`src/ui/`, `src/app.rs`) runs on the main egui thread.
- Slack's Web API and real-time run on a tokio runtime in `src/backend/`.
- The two speak only through `Command`s (UI → worker) and `Event`s (worker →
  UI). Every event wakes the window through the `Waker`.
- Views never call the network and never hold a Slack type. They read
  `src/model.rs` types and push `model::Action`s, which `app.rs` applies after
  the frame. The worker translates Slack's JSON (`src/slack/types.rs`) into the
  model.
- Failures cross as a `failure::Failure` (what went wrong) or a
  `failure::Problem` (and what it stopped), never as English text: the
  worker maps Slack's errors in `backend/api.rs`, the interface words them
  through `t` when it shows them.
- The UI is optimistic: a sent message or reaction shows at once and is
  reconciled on the server's echo or an error.

## Rules

- `unsafe` is forbidden. The one exception is the hardware video helper
  (`crates/noslacking-video`, a separate process), where it is denied
  except in the modules that call the platform's C libraries. Clippy runs with `-D warnings`, including
  `unwrap_used`; handle errors or use `expect` with a reason in tests only.
- Tests never touch the network, the keyring or the clock-dependent world.
  Parse fixtures, test pure functions. `cargo test` must pass offline.
- Secrets (tokens, cookies, the client secret) live only in the OS keyring
  (`src/credentials.rs`) and never in settings or logs. `src/redact.rs`
  keeps them out of the log and the panic log; types holding a secret
  print it as `<redacted>` in `Debug`.
- Every public item is documented. Comments say why, in plain words.
- The code compiles on Linux, macOS and Windows; platform code is behind
  `cfg` with a fallback.
- Work against a pretend Slack with `cargo run --features demo`; capture a
  screenshot with `--demo --demo-shot out.png [--demo-view thread|settings|
  sign-in|dm] [--demo-light]`.

## Checks

```
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --all-features --no-deps
```
