mod agent;
mod auth;
mod model;
mod permissions;
mod sandbox;
mod terminal;
mod tools;
mod transcript;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::{
    io::IsTerminal,
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
        let http = model::http_client()?;
        return tokio::select! {
            result = async {
                match command {
                    Command::Login { no_browser } => {
                        store.login(&http, &args.account, no_browser).await?;
                        println!("Signed in as {}. Run qaf models to see available models.", args.account);
                        Ok(())
                    }
                    Command::Logout => store.logout(&http, &args.account).await,
                    Command::Status => store.status(),
                    Command::SandboxCheck => {
                        let sandbox = sandbox::Sandbox::new(&args.directory, store.directory(), &std::env::current_exe()?)?;
                        sandbox.check().await?;
                        println!("Sandbox ready: project writes, private scratch, no network.");
                        Ok(())
                    }
                    Command::Models => {
                        for (slug, name) in model::Client::models(&store, &args.account).await? {
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
        };
    }
    let terminal = if args.prompt.is_none() {
        anyhow::ensure!(
            std::io::stdin().is_terminal(),
            "interactive mode requires a terminal; pass a task as an argument instead"
        );
        Some(terminal::Terminal::open().context("open interactive terminal")?)
    } else {
        None
    };
    let model_name = args
        .model
        .context("choose --model or set OPENAI_MODEL; run qaf models for choices")?;
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
    let model = model::Client::new(store, args.account.clone(), model_name.clone())?;
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
        &json!({"model": model_name, "account": args.account, "directory": tools.directory(), "max_turns": args.max_turns, "interactive": terminal.is_some()}),
    )?;
    eprintln!("Transcript: {}", path.display());
    if let Some(prompt) = args.prompt {
        let mut agent = agent::Agent::new(&prompt);
        task(&mut agent, &model, &tools, &mut transcript, args.max_turns)
            .await?
            .map_err(anyhow::Error::msg)?;
    } else if let Some(terminal) = terminal {
        interactive(&terminal, &model, &tools, &mut transcript, args.max_turns).await?;
    }
    transcript.record("finished", &json!({}))
}

async fn interactive(
    terminal: &terminal::Terminal,
    model: &impl model::Model,
    tools: &tools::Tools,
    transcript: &mut transcript::Transcript,
    max_turns: u32,
) -> Result<()> {
    eprintln!(
        "Enter a task. /clear starts fresh; /exit or Ctrl-D exits. Ctrl-C stops the current task."
    );
    let mut agent: Option<agent::Agent> = None;
    loop {
        terminal.write("\nqaf> ").await?;
        let line = tokio::select! {
            line = terminal.read_line(1024 * 1024) => line?,
            signal = tokio::signal::ctrl_c() => {
                signal.context("listen for Ctrl-C")?;
                continue;
            }
        };
        let Some(line) = line else { return Ok(()) };
        let prompt = line.trim();
        match prompt {
            "" => continue,
            "/exit" => return Ok(()),
            "/clear" => {
                agent = None;
                transcript.record("clear", &json!({}))?;
                eprintln!("Conversation cleared. Files and scratch space are unchanged.");
                continue;
            }
            _ if prompt.starts_with('/') && !prompt.contains(char::is_whitespace) => {
                eprintln!("Unknown command. Use /clear or /exit.");
                continue;
            }
            _ => {}
        }
        if let Some(agent) = agent.as_mut() {
            agent.add_prompt(prompt);
        }
        let agent = agent.get_or_insert_with(|| agent::Agent::new(prompt));
        if let Err(error) = task(agent, model, tools, transcript, max_turns).await? {
            eprintln!(
                "Task stopped: {error}\nYou can continue with another prompt, or /clear to start fresh."
            );
        }
    }
}

// The outer Result is for transcript failures: don't keep working without a log.
async fn task(
    agent: &mut agent::Agent,
    model: &impl model::Model,
    tools: &tools::Tools,
    transcript: &mut transcript::Transcript,
    max_turns: u32,
) -> Result<Result<(), String>> {
    let result = tokio::select! {
        result = agent.run(model, tools, transcript, max_turns, &terminal::Human, |event| {
            match event {
                agent::Event::Text(text) => println!("{text}"),
                agent::Event::Tool(name) => eprintln!("Running {name}…"),
            }
        }) => result,
        signal = tokio::signal::ctrl_c() => {
            signal.context("listen for Ctrl-C")?;
            Err(anyhow::anyhow!("interrupted"))
        }
    };
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
    use std::{io::Write, sync::Mutex};

    #[tokio::test]
    async fn session_continues_after_errors_and_clear_resets_context() {
        struct Model(Mutex<Vec<Vec<serde_json::Value>>>);
        impl model::Model for Model {
            fn respond(
                &self,
                history: &[serde_json::Value],
                _: &[serde_json::Value],
            ) -> impl std::future::Future<Output = Result<model::Reply>> + Send {
                let mut inputs = self.0.lock().unwrap();
                inputs.push(history.to_vec());
                let fail = inputs.len() == 2;
                drop(inputs);
                std::future::ready(if fail {
                    Err(anyhow::anyhow!("test model failure"))
                } else {
                    Ok(model::Reply {
                        output: vec![json!({"type":"message", "role":"assistant",
                        "content":[{"type":"output_text","text":"Done"}]})],
                    })
                })
            }
        }
        let root = tempfile::tempdir().unwrap();
        let tools = tools::Tools::for_test(root.path(), Duration::from_secs(1)).unwrap();
        let path = root.path().join("log");
        let mut transcript = transcript::Transcript::create(&path).unwrap();
        let model = Model(Mutex::new(Vec::new()));
        let (terminal, mut user) = terminal::Terminal::pair();
        user.write_all(b"first\nfollowup\n/clear\nthird\n/exit\n")
            .unwrap();
        interactive(&terminal, &model, &tools, &mut transcript, 1)
            .await
            .unwrap();
        let inputs = model.0.lock().unwrap();
        assert_eq!(inputs.len(), 3);
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
    }
}
