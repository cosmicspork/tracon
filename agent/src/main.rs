use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Context as _};
use clap::Parser;
use serde_json::json;

use tokio_util::sync::CancellationToken;
use tracon_agent::agent::{Agent, Outcome, Totals, Unattended};
use tracon_agent::conversation::{Block, Conversation, Role};
use tracon_agent::events::Events;
use tracon_agent::prompt::ORIENTATION;
use tracon_agent::server::{self, Settings};
use tracon_agent::tools::{Context, Registry};
use tracon_agent::wire::openai_chat::{Client, Sampling};

/// Tracon's harness for models that are not Claude. By default it serves the
/// node over stdin and stdout (JSON-RPC, the shared ACP vocabulary), asking
/// the node before every tool call. `--standalone` runs one task with no node
/// and no permission gate.
#[derive(Parser)]
#[command(name = "tracon-agent", version)]
struct Cli {
    /// Run one task without a node. There is no permission gate, so the
    /// caller vouches for the directory.
    #[arg(long)]
    standalone: bool,
    /// e.g. http://127.0.0.1:8080/v1
    #[arg(long, env = "TRACON_AGENT_BASE_URL")]
    base_url: String,
    #[arg(long, env = "TRACON_AGENT_MODEL")]
    model: String,
    /// A file holding the bearer token, if the endpoint needs one.
    #[arg(long, env = "TRACON_AGENT_API_KEY_FILE")]
    api_key_file: Option<PathBuf>,
    /// The name of an environment variable holding the bearer token: the
    /// node's per-session key, kept off the command line.
    #[arg(long)]
    api_key_env: Option<String>,
    /// The directory the tools work in; nothing outside it is reachable.
    /// Standalone only: under the node, `session/new` names it.
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Text appended to the orientation: the node's operator notes.
    #[arg(long)]
    system_prompt_file: Option<PathBuf>,
    /// The model's context window in tokens, reported with usage.
    #[arg(long)]
    context_window: Option<u64>,
    #[arg(long, default_value_t = 20)]
    max_turns: u32,
    /// The task. Read from stdin when absent.
    #[arg(long)]
    prompt: Option<String>,
    #[arg(long)]
    temperature: Option<f64>,
    #[arg(long)]
    min_p: Option<f64>,
    /// Ask the chat template not to think (Qwen3's `enable_thinking=false`).
    #[arg(long)]
    no_think: bool,
    /// Allow several tool calls in one response.
    #[arg(long)]
    parallel_tools: bool,
    /// Write JSONL events here instead of stderr.
    #[arg(long)]
    events: Option<PathBuf>,
}

/// Standalone exit codes: 0 finished, 1 failed, 3 turn limit, 4 context overflow.
#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tracon-agent: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let api_key = match (&cli.api_key_file, &cli.api_key_env) {
        (Some(path), _) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?
                .trim()
                .to_string(),
        ),
        (None, Some(name)) => {
            Some(std::env::var(name).with_context(|| format!("--api-key-env {name} is not set"))?)
        }
        (None, None) => None,
    };
    let client = Client::new(
        &cli.base_url,
        api_key,
        &cli.model,
        Sampling {
            temperature: cli.temperature,
            min_p: cli.min_p,
            no_think: cli.no_think,
        },
        cli.parallel_tools,
    )?;
    if !cli.standalone {
        if cli.prompt.is_some() {
            bail!(
                "--prompt is for --standalone; under the node the task arrives as session/prompt"
            );
        }
        let system_extra = match &cli.system_prompt_file {
            Some(path) => Some(
                std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?,
            ),
            None => None,
        };
        // stdout is the protocol: nothing else may write to it.
        server::serve(
            Settings {
                client,
                max_turns: cli.max_turns,
                system_extra,
                context_window: cli.context_window,
                events: cli.events,
            },
            Box::new(tokio::io::stdin()),
            Box::new(tokio::io::stdout()),
        )
        .await?;
        return Ok(ExitCode::SUCCESS);
    }
    let prompt = match cli.prompt {
        Some(p) => p,
        None => {
            let mut p = String::new();
            std::io::stdin().read_to_string(&mut p)?;
            p
        }
    };
    if prompt.trim().is_empty() {
        bail!("no task given: pass --prompt or write it to stdin");
    }
    let cx = Context::new(&cli.cwd).with_context(|| format!("--cwd {}", cli.cwd.display()))?;
    let events = match &cli.events {
        Some(path) => {
            Events::to_file(path).with_context(|| format!("--events {}", path.display()))?
        }
        None => Events::to_stderr(),
    };
    let tools = Registry::default();
    events.emit(
        "start",
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "model": cli.model,
            "base_url": cli.base_url,
            "cwd": cx.root,
            "max_turns": cli.max_turns,
            "tools": tools.names(),
        }),
    );
    let mut conv = Conversation::new(ORIENTATION);
    conv.push(Role::User, vec![Block::Text(prompt)]);
    let mut totals = Totals::default();
    let cancel = CancellationToken::new();
    let agent = Agent {
        client: &client,
        tools: &tools,
        cx: &cx,
        events: &events,
        hooks: &Unattended,
        cancel: &cancel,
        max_turns: cli.max_turns,
    };
    let outcome = agent.run(&mut conv, &mut totals).await;
    let (code, end) = match &outcome {
        Ok(o @ Outcome::Finished { text }) => {
            println!("{text}");
            (0, json!(o))
        }
        Ok(o @ Outcome::MaxTurns) => (3, json!(o)),
        Ok(o @ Outcome::ContextOverflow { .. }) => (4, json!(o)),
        Ok(o @ Outcome::Cancelled) => (1, json!(o)),
        Err(e) => (1, json!({"outcome": "error", "message": e.to_string()})),
    };
    let mut end = end;
    end["totals"] = json!(totals);
    events.emit("end", end);
    if let Err(e) = outcome {
        eprintln!("tracon-agent: {e}");
    }
    Ok(ExitCode::from(code))
}
