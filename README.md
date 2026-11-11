# qaf

A small terminal coding agent written in Rust for macOS and Linux.

## Install and run

Install with Cargo, with its bin directory on your `PATH`:

```sh
cargo install --git https://github.com/theodore-s-beers/qaf.git --locked
```

Sign in with your ChatGPT subscription, choose a model from your account's catalog, and start a session:

```sh
qaf login
qaf models
qaf --model YOUR_MODEL -C /path/to/project
```

To run a single task and exit, pass it as an argument:

```sh
qaf -C /path/to/project "Fix the failing test and explain the change"
```

## Interactive sessions

Each prompt continues the conversation. Enter submits; Shift+Enter adds a newline in supported terminals (Alt+Enter or Ctrl-J also work in the live input). Pasted newlines are preserved. Long prompts wrap and show up to six lines without truncating the submitted text. Up/Down navigate lines and history; Home/End move within a line.

The prompt is cyan when idle and orange while working. Messages submitted during a task are queued until the current response and tool batch finish. They do not interrupt running tools. Approval prompts temporarily take over input and restore your draft afterward.

When idle, these commands are available:

- `/model` shows the current model; `/model NAME` switches and remembers it.
- `/context` shows approximate context usage.
- `/clear` clears the conversation and queued messages without undoing file changes.
- `/exit` exits.

Ctrl-C stops a task or discards idle input. Ctrl-D with empty input exits when idle or stops a running task.

`--max-turns` limits model turns per task, including queued follow-ups. Undelivered messages survive a stopped task and accompany your next prompt. Shell commands time out after 120 seconds by default; change this with `--command-timeout`. See `qaf --help` for all options.

Responses use terminal Markdown formatting. `NO_COLOR` disables colors and Markdown styling. Redirected output or `TERM=dumb` uses raw Markdown and a line prompt that accepts input only between tasks.

Context usage appears after each task; long conversations are compacted automatically. Sessions and input history are not saved for resumption. Each run prints the path to a JSONL transcript in the temporary directory; `--transcript` chooses another path. Transcripts retain prompts, responses, and tool activity—including file contents—even after compaction.

## Configuration

Credentials and `preferences.json` live in `~/.config/qaf` (or `$XDG_CONFIG_HOME/qaf`); override this with `--auth-dir`. Use `qaf status` to view accounts, `qaf logout` to sign out, and `--account LABEL` to select an account.

Place personal instructions in `AGENTS.md` in the same configuration directory. qaf loads it at session startup and on `/clear`, and retains it through compaction. Symlinks are supported, so you can share a file with another harness. Missing or empty files are ignored; other read errors are reported.

Model selection order is `--model`, `OPENAI_MODEL`, then the saved preference. Explicit selections are remembered. For example, `preferences.json` can contain:

```json
{
  "model_primary": {
    "name": "YOUR_MODEL",
    "reasoning_effort": "medium"
  },
  "model_reviewer": {
    "name": "YOUR_REVIEW_MODEL",
    "fast": true
  },
  "approvals_reviewer": "auto_review"
}
```

- `approvals_reviewer` defaults to `"user"`; `"auto_review"` enables model review of unrestricted operations.
- Omit `model_reviewer.name` to follow the primary model, including `/model` changes.
- Each role has independent `reasoning_effort` and `fast` settings. Omitted effort uses the provider default. `fast` defaults to false; true requests priority service, subject to model and account availability.

Restart after editing preferences.

## Permissions

File and shell tools run in a sandbox: broad reads, writes limited to the project and private scratch space, no network, and no access to qaf's configuration. Existing project `.git` and `.jj` directories are read-only. OpenAI-hosted web search runs outside this local sandbox.

macOS uses `/usr/bin/sandbox-exec`. Linux (x86-64 or ARM64) requires `bubblewrap` and enabled user namespaces. Check support with:

```sh
qaf -C /path/to/project sandbox-check
```

Tools cannot run if the sandbox fails to start; failures never trigger automatic unrestricted retries.

Broader access requires approval for the exact operation: `allow` once, `deny` (default), or `cancel` the task. **Approved operations bypass the sandbox entirely**, including its protection of qaf's configuration. Requests needing human approval are denied without an interactive terminal.

Auto-review checks operations against the conversation and logs its decisions. Uncertainty, errors, and timeouts fall back to human approval. If user instructions exceed the review budget, human approval is required until `/clear`.

Direct writes to qaf's configuration require human approval. Reviewer policy also requires human review for indirect changes to approval settings or implementation. Model review is not an OS security boundary.

## Development

`prompts/SYSTEM_PROMPT.md` and `prompts/REVIEW_PROMPT.md` are embedded at compile time; rebuild after editing either.

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Tests use scripted model replies and real file/shell tools. They require `rustc`, a POSIX shell, a working OS sandbox, and loopback sockets, but no credentials or internet access. The key in `tests/fixtures` is generated test data.

The optional terminal smoke test requires Python 3 and checks input, approval drafts, resizing, cancellation, styling, and terminal cleanup:

```sh
cargo test --no-run
# Use the qaf unit-test executable path printed above:
python3 tests/fixtures/live-terminal.py target/debug/deps/qaf-<hash>
```
