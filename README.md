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

In a session, each prompt continues the conversation:

- `/clear` starts fresh without undoing file changes.
- `/context` shows approximate context usage.
- `/model` shows the current model; `/model NAME` switches and remembers it, preserving the conversation.
- `/exit` or Ctrl-D exits.
- On an interactive terminal, the input area stays at the bottom while responses stream above it. Type and press Enter during a task to queue a follow-up.
- Follow-ups reach the model in order after the current response and tool batch finish, before its next request. They do not interrupt running tools. A queued follow-up also continues the conversation after a final answer.
- Ctrl-C stops the current task, or discards input at the prompt. Ctrl-D during a task also stops it when the input is empty.
- Approval prompts temporarily use the input area and restore your unfinished draft afterward. Session commands such as `/model` and `/clear` are available when the task is idle.
- Enter submits; Shift+Enter inserts a newline in terminals that report modified Enter. Alt+Enter or Ctrl-J also insert a newline in the live input. Pasted newlines are preserved.
- Prompts wrap at word boundaries where possible (splitting overlong words) and show up to six lines; longer drafts keep their full contents, with the visible area following the cursor. Up/Down move between lines and recall history at the edges; Home/End move within the current line.

Queued follow-ups share the current task's `--max-turns` budget. If a task stops with undelivered messages, they are retained for your next prompt; `/clear` discards them. With redirected output or `TERM=dumb`, sessions use the original line prompt and accept input only between tasks.

Responses stream with terminal Markdown formatting; redirected output and `NO_COLOR` use raw Markdown. Conversation and input history last only for the current session; sessions cannot be resumed.

A context summary appears after each interactive task. qaf requests automatic compaction at 230,000 tokens within a 258,000-token working budget, not a hard input limit. Usage is approximate, especially after compaction.

Commands time out after 120 seconds by default; use `--command-timeout` to change this. `--max-turns` limits model turns per task. See `qaf --help` for all options.

Each run prints the path to a JSONL transcript, stored in the temporary directory by default. Transcripts retain prompts, responses, and tool activity, including file contents, even after compaction.

## Configuration

Credentials and `preferences.json` live in `~/.config/qaf` (or `$XDG_CONFIG_HOME/qaf`), overridable with `--auth-dir`. Credentials have owner-only permissions; keep them out of source control. Use `qaf status` to view accounts, `qaf logout` to sign out, and `--account LABEL` to select an account.

Model selection order is `--model`, `OPENAI_MODEL`, then the saved preference. Explicit selections are remembered. For example, `preferences.json` can contain:

```json
{
  "model_primary": {
    "name": "YOUR_MODEL",
    "reasoning_effort": "medium",
    "fast": false
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
- Reasoning effort and Fast mode are independent for each role. Omitted effort uses the provider default; `fast` defaults to false. When true, it requests the priority service tier; availability depends on the model and account.

Restart after editing preferences. Legacy `model` and `reviewer_model` fields remain readable and migrate when saving a model selection.

## Permissions

Local tools read, write, and edit files and run shell commands. The sandbox permits broad reads but limits writes to the project and private scratch space. It blocks network access and access to qaf's configuration directory; existing project `.git` and `.jj` directories are read-only. The main agent also has OpenAI-hosted web search, which runs outside the local sandbox.

macOS uses `/usr/bin/sandbox-exec`. Linux (x86-64 or ARM64) requires `bubblewrap` and enabled user namespaces. Check support with:

```sh
qaf -C /path/to/project sandbox-check
```

qaf refuses to run tools if the sandbox cannot start. Failures never trigger automatic unrestricted retries.

Broader access requires approval for the exact operation. Human choices are `allow` once, `deny` (default), or `cancel` the task. **Approved operations bypass the sandbox entirely**, including its protection of qaf's configuration. Requests needing human approval are denied without an interactive terminal.

Auto-review evaluates the operation against conversation evidence without tools, and logs its decision and rationale. Uncertainty, errors, and timeouts fall back to human approval. If user instructions exceed the review budget, human approval is required until `/clear`. Denied operations do not execute.

Direct writes to qaf's configuration require human approval. The reviewer policy also requires human review for indirect changes to approval settings or implementation. Model review is not an OS security boundary.

## Development

`prompts/SYSTEM_PROMPT.md` and `prompts/REVIEW_PROMPT.md` are embedded at compile time; rebuild after editing either. The agent prompt's `{{REQUESTED_MODEL}}` placeholder is filled with the selected model identifier.

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Tests use scripted model replies and real file/shell tools. They require `rustc`, a POSIX shell, a working OS sandbox, and loopback sockets, but no credentials or internet access. The key in `tests/fixtures` is generated test data.

The optional terminal smoke test checks follow-up input, approval draft restoration, resizing, cancellation, and terminal cleanup using Python 3 and a pseudo-terminal:

```sh
cargo test --no-run
# Use the qaf unit-test executable path printed above:
python3 tests/fixtures/live-terminal.py target/debug/deps/qaf-<hash>
```
