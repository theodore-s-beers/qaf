# qaf

A small terminal coding agent written in Rust for macOS and Linux.

## Run

Sign in with your ChatGPT subscription, choose a model from your account's catalog, and start a session:

```sh
cargo run -- login
cargo run -- models
cargo run -- --model YOUR_MODEL -C /path/to/project
```

Model selection order is `--model`, `OPENAI_MODEL`, then the saved preference. Explicit selections are remembered in `preferences.json`. To run a single task and exit, pass it as an argument:

```sh
cargo run -- -C /path/to/project "Fix the failing test and explain the change"
```

In a session, each prompt continues the conversation:

- `/clear` starts fresh without undoing file changes.
- `/context` shows approximate context usage.
- `/exit` or Ctrl-D exits.
- Ctrl-C stops the current task, or discards input at the prompt.
- Up/Down recall previous inputs.

Conversation and input history last only for the current session. `--max-turns` applies separately to each task. Responses render Markdown in the terminal; redirected output and `NO_COLOR` preserve the original Markdown.

qaf requests server-side compaction at 230,000 tokens, leaving headroom within a 258,000-token working budget. This is not a hard input limit. Compaction replaces older conversation with encrypted state; the full transcript remains intact. Usage is approximate between responses and immediately after compaction.

Each run prints the path to a JSONL transcript in the temporary directory. Transcripts contain prompts, responses, and tool activity, including file contents.

Credentials and preferences live in `~/.config/qaf` (or `$XDG_CONFIG_HOME/qaf`), overridable with `--auth-dir`. Credentials have owner-only permissions; keep them out of source control. Use `status` to view accounts, `logout` to sign out, and `--account LABEL` to select an account when logging in or running tasks.

See `cargo run -- --help` for options.

## Permissions

Tools read, write, and edit files and run shell commands. The OS sandbox permits broad reads but limits writes to the project and temporary scratch space. It blocks network access and access to qaf's configuration directory; existing project `.git` and `.jj` directories are read-only.

macOS uses `/usr/bin/sandbox-exec`. Linux (x86-64 or ARM64) requires `bubblewrap` and enabled user namespaces. Check support with:

```sh
cargo run -- -C /path/to/project sandbox-check
```

qaf refuses to run tools if the sandbox cannot start.

Broader access requires approval for the exact operation: `allow` once, `deny` (default), or `cancel` the task. **Approved operations bypass the sandbox entirely**, including its protection of qaf's configuration. Requests needing human approval are denied without an interactive terminal. Sandbox failures do not automatically trigger unrestricted retries.

To enable auto-review, add `approvals_reviewer` to `preferences.json` and restart:

```json
{
  "model": "YOUR_MODEL",
  "approvals_reviewer": "auto_review"
}
```

The default is `"user"`. Auto-review uses the selected model in a separate request with `REVIEW_PROMPT.md`, conversation evidence, and the full operation, but no tools. Decisions and rationales are logged. Uncertainty, errors, and timeouts fall back to human approval; denied operations do not execute.

Auto-review keeps readable evidence separately from the main model’s compacted context: verbatim user messages and recent assistant/tool activity. Omitted evidence is flagged. If user instructions exceed the review budget, approval falls back to you until `/clear`.

Direct writes to qaf's configuration require human approval. The reviewer policy also requires human review for indirect changes to approval settings or implementation. Model review is not an OS security boundary.

## Development

`SYSTEM_PROMPT.md` and `REVIEW_PROMPT.md` are embedded at compile time; rebuild after editing either. The agent prompt's `{{REQUESTED_MODEL}}` placeholder is filled with the selected model identifier.

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

Tests use scripted model replies and real file/shell tools. They require `rustc`, a POSIX shell, a working OS sandbox, and loopback sockets, but no credentials or internet access. The key in `tests/fixtures` is generated test data.
