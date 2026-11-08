mod agent;
mod auth;
mod model;
mod tools;
mod transcript;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Parser)]
#[command(
    version,
    about = "A small coding agent. Tools run with your user's permissions."
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Task to complete
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
}

#[tokio::main]
async fn main() -> Result<()> {
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
    let prompt = args
        .prompt
        .context("provide a task, or run qaf login / qaf --help")?;
    let model_name = args
        .model
        .context("choose --model or set OPENAI_MODEL; run qaf models for choices")?;
    if prompt.trim().is_empty() {
        bail!("prompt must not be empty");
    }
    let model = model::Client::new(store, args.account.clone(), model_name.clone())?;
    let tools = tools::Tools::new(&args.directory, Duration::from_secs(args.command_timeout))?;
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
        &json!({"model": model_name, "account": args.account, "directory": tools.directory(), "max_turns": args.max_turns}),
    )?;
    eprintln!("Transcript: {}", path.display());
    let mut agent = agent::Agent::new(&prompt);
    let result = tokio::select! {
        result = agent.run(&model, &tools, &mut transcript, args.max_turns, |event| {
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
        Ok(()) => transcript.record("finished", &json!({}))?,
        Err(error) => transcript.record("error", &json!({"message": format!("{error:#}")}))?,
    }
    result
}
