# qaf

A small terminal coding agent written in Rust for macOS and Linux.

## Run

Sign in with your ChatGPT subscription, then choose a model from your account's catalog:

```sh
cargo run -- login
cargo run -- models
cargo run -- --model YOUR_MODEL -C /path/to/project \
  "Fix the failing test and explain the change"
```

Login opens your browser to authorize qaf to use your ChatGPT plan. Use `status` to view saved accounts and `logout` to sign out. To use another account or workspace, pass `--account LABEL` when logging in and running tasks.

Credentials are stored with owner-only permissions in `~/.config/qaf/auth.json` (or `$XDG_CONFIG_HOME/qaf/auth.json`). Keep this directory out of source control.

The agent can read, write, and edit files and run shell commands with your user's permissions. The working directory is not a sandbox. Ctrl-C interrupts the run.

Each run prints the path to a JSONL transcript in the system temporary directory. Transcripts include prompts, model output, and tool activity, including file contents.

Run `cargo run -- --help` for options.

## Structure

- `src/main.rs`: CLI configuration and terminal output.
- `src/auth.rs`: ChatGPT login, credentials, and token refresh.
- `src/agent.rs`: conversation history and the model/tool loop.
- `src/model.rs`: OpenAI Responses API integration.
- `src/tools.rs`: file and shell tools.
- `src/transcript.rs`: JSONL recording.

## Verify

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Tests use scripted model replies and real file/shell tools. They require `rustc`, a POSIX shell, and permission to bind loopback sockets, but no credentials or internet access. The key in `tests/fixtures` is generated test data, not a real credential.
