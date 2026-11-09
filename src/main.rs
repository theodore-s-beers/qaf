mod agent;
mod auth;
mod context;
mod model;
mod permissions;
mod preferences;
mod render;
mod review;
mod sandbox;
mod terminal;
mod tools;
mod transcript;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
#[command(
    version,
    about = "A small coding agent with sandboxed tools and human approval."
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Task to complete; omit to start an interactive session
    prompt: Option<String>,
    /// Saved `ChatGPT` registration label
    #[arg(long, global = true, default_value = "default")]
    account: String,
    /// Credential directory (defaults to ~/.config/qaf or $`XDG_CONFIG_HOME/qaf`)
    #[arg(long, global = true, env = "QAF_AUTH_DIR")]
    auth_dir: Option<PathBuf>,
    /// Model to use and remember (defaults to the last selected model)
    #[arg(long, env = "OPENAI_MODEL")]
    model: Option<String>,
    #[arg(short = 'C', long, default_value = ".")]
    directory: PathBuf,
    /// Maximum model turns per task
    #[arg(long, default_value = "20", value_parser = clap::value_parser!(u32).range(1..))]
    max_turns: u32,
    #[arg(long, default_value = "120", value_parser = clap::value_parser!(u64).range(1..))]
    command_timeout: u64,
    /// JSONL output path (defaults to a unique file in the system temp directory)
    #[arg(long)]
    transcript: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Continue with `ChatGPT` and authorize use of your subscription
    Login {
        /// Print the sign-in URL without opening a browser
        #[arg(long)]
        no_browser: bool,
    },
    /// Revoke the selected session and clear its local tokens
    Logout,
    /// Show saved accounts without revealing credentials
    Status,
    /// List models available through your `ChatGPT` account
    Models,
    /// Verify the OS sandbox can launch; no model request is made
    SandboxCheck,
}

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    if arguments.next().as_deref() == Some(std::ffi::OsStr::new("__tool-worker")) {
        let path = arguments.next().context("missing worker request")?;
        return tools::worker(std::path::Path::new(&path));
    }
    run()
}

#[tokio::main]
async fn run() -> Result<()> {
    let args = Args::parse();
    let store = auth::Store::new(args.auth_dir)?;
    if let Some(command) = args.command {
        return run_command(command, &store, &args.account, &args.directory).await;
    }
    let terminal = if args.prompt.is_none() {
        anyhow::ensure!(
            std::io::stdin().is_terminal(),
            "interactive mode requires a terminal; pass a task as an argument instead"
        );
        Some(terminal::LineEditor::new().context("open interactive line editor")?)
    } else {
        None
    };
    let preferences_path = store.directory().join("preferences.json");
    let model_name = preferences::select_model(&preferences_path, args.model.as_deref())?;
    if args
        .prompt
        .as_ref()
        .is_some_and(|prompt| prompt.trim().is_empty())
    {
        bail!("prompt must not be empty");
    }
    let tools = tools::Tools::new(
        &args.directory,
        Duration::from_secs(args.command_timeout),
        store.directory(),
    )
    .await?;
    let config_directory = store.directory().to_owned();
    let reviewer = preferences::reviewer(&preferences_path)?;
    let primary_settings = preferences::model_settings(&preferences_path, "model_primary")?;
    let reviewer_settings = preferences::model_settings(&preferences_path, "model_reviewer")?;
    let reviewer_name = reviewer_settings.name.clone();
    let model = model::Client::new(store, args.account.clone(), model_name.clone())?
        .with_settings(primary_settings.clone());
    let reviewer_model = model.reviewer(reviewer_settings.clone());
    let approver = review::Approval {
        mode: reviewer,
        model: &reviewer_model,
        human: &terminal::Human,
        config_directory: &config_directory,
    };
    let path = args.transcript.unwrap_or_else(|| {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("qaf-{}-{stamp}.jsonl", std::process::id()))
    });
    let mut transcript = transcript::Transcript::create(&path)?;
    transcript.record(
        "start",
        &json!({"model": model_name, "account": args.account, "directory": tools.directory(), "max_turns": args.max_turns, "interactive": terminal.is_some(), "approvals_reviewer": reviewer, "reviewer_model":reviewer_name, "model_primary":primary_settings, "model_reviewer":reviewer_settings}),
    )?;
    if args.model.is_some() {
        preferences::remember_model(&preferences_path, &model_name)?;
    }
    show_models(&model_name, &primary_settings, reviewer, &reviewer_settings);
    terminal::status(format_args!("Transcript: {}", path.display()));
    if let Some(prompt) = args.prompt {
        let mut agent = agent::Agent::new(&prompt, &model_name);
        task(
            &mut agent,
            &model,
            &tools,
            &mut transcript,
            args.max_turns,
            &approver,
        )
        .await?
        .map_err(anyhow::Error::msg)?;
    } else if let Some(mut terminal) = terminal {
        interactive(
            &mut terminal,
            &model,
            &tools,
            &mut transcript,
            args.max_turns,
            &model_name,
            &approver,
        )
        .await?;
    }
    transcript.record("finished", &json!({}))
}

fn show_models(
    model_name: &str,
    primary_settings: &preferences::ModelSettings,
    reviewer: preferences::Reviewer,
    reviewer_settings: &preferences::ModelSettings,
) {
    terminal::status(format_args!(
        "Model: {model_name} (reasoning: {}; fast requested: {})",
        primary_settings
            .reasoning_effort
            .as_deref()
            .unwrap_or("default"),
        primary_settings.fast
    ));
    terminal::status(format_args!(
        "Approvals: {}",
        match reviewer {
            preferences::Reviewer::User => "human",
            preferences::Reviewer::AutoReview => "auto-review",
        }
    ));
    if reviewer == preferences::Reviewer::AutoReview {
        terminal::status(format_args!(
            "Reviewer model: {} (reasoning: {}; fast requested: {})",
            reviewer_settings
                .name
                .as_deref()
                .unwrap_or("follows session model"),
            reviewer_settings
                .reasoning_effort
                .as_deref()
                .unwrap_or("default"),
            reviewer_settings.fast
        ));
    }
}

async fn run_command(
    command: Command,
    store: &auth::Store,
    account: &str,
    directory: &std::path::Path,
) -> Result<()> {
    let http = model::http_client()?;
    tokio::select! {
        result = async {
            match command {
                Command::Login { no_browser } => {
                    store.login(&http, account, no_browser).await?;
                    println!("Signed in as {account}. Run qaf models to see available models.");
                    Ok(())
                }
                Command::Logout => store.logout(&http, account).await,
                Command::Status => store.status(),
                Command::SandboxCheck => {
                    let sandbox = sandbox::Sandbox::new(directory, store.directory(), &std::env::current_exe()?)?;
                    sandbox.check().await?;
                    println!("Sandbox ready: project writes, private scratch, no network.");
                    Ok(())
                }
                Command::Models => {
                    for (slug, name) in model::Client::models(store, account).await? {
                        println!("{slug}\t{name}");
                    }
                    Ok(())
                }
            }
        } => result,
        signal = tokio::signal::ctrl_c() => {
            signal.context("listen for Ctrl-C")?;
            bail!("interrupted");
        }
    }
}

async fn interactive(
    terminal: &mut impl terminal::PromptReader,
    model: &impl model::Model,
    tools: &tools::Tools,
    transcript: &mut transcript::Transcript,
    max_turns: u32,
    model_name: &str,
    approver: &impl permissions::Approver,
) -> Result<()> {
    terminal::status(format_args!(
        "Enter a task. /model switches models; /context shows usage; /clear starts fresh; /exit or Ctrl-D exits. Ctrl-C stops the current task."
    ));
    let mut model_name = model_name.to_owned();
    let mut agent: Option<agent::Agent> = None;
    loop {
        eprintln!();
        let line = match terminal.read_prompt().await? {
            terminal::Input::Line(line) => line,
            terminal::Input::Interrupted => continue,
            terminal::Input::Eof => return Ok(()),
        };
        let prompt = line.trim();
        if prompt.split_whitespace().next() == Some("/model") {
            let words: Vec<_> = prompt.split_whitespace().collect();
            match words.as_slice() {
                [_] => terminal::status(format_args!(
                    "Model: {model_name}. Use /model NAME to switch; run qaf models for choices."
                )),
                [_, name] if *name == model_name => {
                    terminal::status(format_args!("Model: {model_name}"));
                }
                [_, name] => {
                    let result = tokio::select! {
                        result = model.select_model(name) => result,
                        signal = tokio::signal::ctrl_c() => {
                            signal.context("listen for Ctrl-C")?;
                            Err(anyhow::anyhow!("interrupted"))
                        }
                    };
                    match result {
                        Ok(()) => {
                            transcript
                                .record("model_changed", &json!({"from":model_name,"to":name}))?;
                            model_name = (*name).to_owned();
                            if let Some(agent) = agent.as_mut() {
                                agent.select_model(name);
                            }
                            terminal::status(format_args!("Model: {model_name}"));
                        }
                        Err(error) => {
                            eprintln!(
                                "Could not switch model; still using {model_name}: {error:#}"
                            );
                        }
                    }
                }
                _ => eprintln!("Usage: /model [NAME]"),
            }
            continue;
        }
        match prompt {
            "" => continue,
            "/exit" => return Ok(()),
            "/context" => {
                terminal::status(format_args!(
                    "Context: approximately {} / {} tokens; auto-compaction at {}.",
                    terminal::number(agent.as_ref().map_or(0, agent::Agent::context_tokens)),
                    terminal::number(context::WINDOW),
                    terminal::number(context::COMPACT_AT)
                ));
                continue;
            }
            "/clear" => {
                agent = None;
                transcript.record("clear", &json!({}))?;
                eprintln!("\nConversation cleared. Files and scratch space are unchanged.");
                continue;
            }
            _ if prompt.starts_with('/') && !prompt.contains(char::is_whitespace) => {
                eprintln!("Unknown command. Use /model, /context, /clear, or /exit.");
                continue;
            }
            _ => {}
        }
        if let Some(agent) = agent.as_mut() {
            agent.add_prompt(prompt);
        }
        let agent = agent.get_or_insert_with(|| agent::Agent::new(prompt, &model_name));
        if let Err(error) = task(agent, model, tools, transcript, max_turns, approver).await? {
            eprintln!(
                "Task stopped: {error}\nYou can continue with another prompt, or /clear to start fresh."
            );
        }
        let tokens = agent.context_tokens();
        let percent = (u128::from(tokens) * 100 + u128::from(context::WINDOW) / 2)
            / u128::from(context::WINDOW);
        terminal::status(format_args!(
            "\nContext: ~{} / {} tokens ({percent}%)",
            terminal::number(tokens),
            terminal::number(context::WINDOW)
        ));
    }
}

// The outer Result is for transcript failures: don't keep working without a log.
async fn task(
    agent: &mut agent::Agent,
    model: &impl model::Model,
    tools: &tools::Tools,
    transcript: &mut transcript::Transcript,
    max_turns: u32,
    approver: &impl permissions::Approver,
) -> Result<Result<(), String>> {
    let mut showing_tools = false;
    let mut rendering = render::Stream::new();
    let mut output_error = None;
    let result = tokio::select! {
        result = agent.run(model, tools, transcript, max_turns, approver, |event| {
            match event {
                agent::Event::Text(text) => {
                    match rendering.finish(text) {
                        Ok(output) => print!("{output}"),
                        Err(error) => output_error = Some(error),
                    }
                    showing_tools = false;
                }
                agent::Event::Delta(text) => {
                    print!("{}", rendering.push(text));
                    if let Err(error) = std::io::stdout().flush() { output_error = Some(error.into()); }
                    showing_tools = false;
                }
                agent::Event::Search => {
                    if !showing_tools {
                        eprintln!();
                    }
                    terminal::status(format_args!("Searching the web…"));
                    showing_tools = true;
                }
                agent::Event::Approval => showing_tools = false,
                agent::Event::Compacted => {
                    terminal::status(format_args!("\nConversation compacted."));
                    showing_tools = false;
                }
                agent::Event::Tool(tool) => {
                    if !showing_tools {
                        eprintln!();
                    }
                    terminal::status(format_args!("{}", terminal::tool_preview(tool)));
                    showing_tools = true;
                }
            }
        }) => result,
        signal = tokio::signal::ctrl_c() => {
            signal.context("listen for Ctrl-C")?;
            Err(anyhow::anyhow!("interrupted"))
        }
    };
    print!("{}", rendering.finish_partial());
    let result = result.and_then(|()| output_error.map_or(Ok(()), Err));
    match &result {
        Ok(()) => transcript.record("task_finished", &json!({}))?,
        Err(error) => {
            agent.stop(transcript)?;
            transcript.record("error", &json!({"message": format!("{error:#}")}))?;
        }
    }
    Ok(result.map_err(|error| format!("{error:#}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, sync::Mutex};

    struct Model(Mutex<Vec<Vec<serde_json::Value>>>, Mutex<String>);
    impl model::Model for Model {
        fn select_model(&self, name: &str) -> impl std::future::Future<Output = Result<()>> + Send {
            std::future::ready(if name == "new-model" {
                name.clone_into(&mut self.1.lock().unwrap());
                Ok(())
            } else {
                Err(anyhow::anyhow!("unknown model"))
            })
        }

        fn respond(
            &self,
            history: &[serde_json::Value],
            _: &[serde_json::Value],
        ) -> impl std::future::Future<Output = Result<model::Reply>> + Send {
            assert!(
                history[0]["content"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("`{}`", self.1.lock().unwrap()))
            );
            let mut inputs = self.0.lock().unwrap();
            inputs.push(history.to_vec());
            let fail = inputs.len() == 2;
            drop(inputs);
            std::future::ready(if fail {
                Err(anyhow::anyhow!("test model failure"))
            } else {
                Ok(model::Reply {
                    usage: None,
                    output: vec![json!({"type":"message", "role":"assistant",
                    "content":[{"type":"output_text","text":"Done"}]})],
                })
            })
        }
    }
    struct Prompts(VecDeque<terminal::Input>);
    impl terminal::PromptReader for Prompts {
        fn read_prompt(
            &mut self,
        ) -> impl std::future::Future<Output = Result<terminal::Input>> + Send {
            std::future::ready(Ok(self.0.pop_front().unwrap_or(terminal::Input::Eof)))
        }
    }

    #[tokio::test]
    async fn session_continues_after_errors_and_clear_resets_context() {
        let root = tempfile::tempdir().unwrap();
        let tools = tools::Tools::for_test(root.path(), Duration::from_secs(1)).unwrap();
        let path = root.path().join("log");
        let mut transcript = transcript::Transcript::create(&path).unwrap();
        let model = Model(Mutex::new(Vec::new()), Mutex::new("session-model".into()));
        let mut terminal = Prompts(VecDeque::from([
            terminal::Input::Line("/context".into()),
            terminal::Input::Interrupted,
            terminal::Input::Line("first".into()),
            terminal::Input::Line("/model".into()),
            terminal::Input::Line("/model new-model".into()),
            terminal::Input::Line("/model missing".into()),
            terminal::Input::Line("/model too many args".into()),
            terminal::Input::Line("/context".into()),
            terminal::Input::Interrupted,
            terminal::Input::Line("followup".into()),
            terminal::Input::Line("/clear".into()),
            terminal::Input::Line("third".into()),
            terminal::Input::Line("/exit".into()),
        ]));
        interactive(
            &mut terminal,
            &model,
            &tools,
            &mut transcript,
            1,
            "session-model",
            &terminal::Human,
        )
        .await
        .unwrap();
        let inputs = model.0.lock().unwrap();
        assert_eq!(inputs.len(), 3);
        for (index, input) in inputs.iter().enumerate() {
            let instructions = input[0]["content"].as_str().unwrap();
            assert!(instructions.contains("qaf"));
            assert!(instructions.contains(if index == 0 {
                "`session-model`"
            } else {
                "`new-model`"
            }));
            assert!(!instructions.contains("{{REQUESTED_MODEL}}"));
        }
        assert_eq!(inputs[1][1]["content"], "first");
        assert_eq!(inputs[1][2]["role"], "assistant");
        assert_eq!(inputs[1][3]["content"], "followup");
        assert_eq!(inputs[2].len(), 2);
        assert_eq!(inputs[2][1]["content"], "third");
        drop(inputs);
        let records: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            records
                .iter()
                .filter(|r| r["kind"] == "task_finished")
                .count(),
            2
        );
        assert_eq!(records.iter().filter(|r| r["kind"] == "error").count(), 1);
        assert_eq!(records.iter().filter(|r| r["kind"] == "clear").count(), 1);
        let changes: Vec<_> = records
            .iter()
            .filter(|r| r["kind"] == "model_changed")
            .collect();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0]["data"],
            json!({"from":"session-model","to":"new-model"})
        );
    }
}
