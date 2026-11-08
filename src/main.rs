mod agent;
mod auth;
mod model;
mod permissions;
mod sandbox;
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
    about = "A small coding agent with sandboxed tools and human approval."
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
    let prompt = args
        .prompt
        .context("provide a task, or run qaf login / qaf --help")?;
    let model_name = args
        .model
        .context("choose --model or set OPENAI_MODEL; run qaf models for choices")?;
    if prompt.trim().is_empty() {
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
        &json!({"model": model_name, "account": args.account, "directory": tools.directory(), "max_turns": args.max_turns}),
    )?;
    eprintln!("Transcript: {}", path.display());
    let mut agent = agent::Agent::new(&prompt);
    let result = tokio::select! {
        result = agent.run(&model, &tools, &mut transcript, args.max_turns, &Human, |event| {
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

struct Human;

impl permissions::Approver for Human {
    async fn approve(&self, request: &serde_json::Value) -> Result<permissions::Decision> {
        use std::{
            fs::OpenOptions,
            io::{Read, Write},
            os::unix::fs::OpenOptionsExt,
        };
        let Ok(mut terminal) = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open("/dev/tty")
        else {
            eprintln!("Broader access denied: no interactive terminal is available.");
            return Ok(permissions::Decision::Deny);
        };
        // JSON escaping prevents model-controlled terminal control sequences.
        let prompt = format!(
            "\nRun this operation WITHOUT the sandbox, with your user permissions?\n{}\nType allow (once), deny [default], or cancel: ",
            serde_json::to_string_pretty(request)?
        );
        terminal.write_all(prompt.as_bytes())?;
        let mut answer = Vec::new();
        loop {
            let mut byte = [0];
            match terminal.read(&mut byte) {
                Ok(0) => return Ok(permissions::Decision::Deny),
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => {
                    // Consume the entire line so leftovers cannot answer a later prompt.
                    if answer.len() <= 32 {
                        answer.push(byte[0]);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        if answer.len() > 32 {
            return Ok(permissions::Decision::Deny);
        }
        Ok(match std::str::from_utf8(&answer).unwrap_or("").trim() {
            "allow" => permissions::Decision::AllowOnce,
            "cancel" => permissions::Decision::Cancel,
            _ => permissions::Decision::Deny,
        })
    }
}
