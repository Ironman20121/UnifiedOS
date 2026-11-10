//! `uos`: the UnifiedOS command line.

mod report;

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use serde_json::Value;
use uos_agent::{default_tools, Agent, AgentConfig, Approver, AuditLog, Policy, Risk};
use uos_hw::{HardwareSnapshot, ModelProfile, PlanOptions, Reserve, TierPlan};
use uos_llm::{Ollama, DEFAULT_BASE_URL};
use uos_vm::{LaunchRequest, Paths};

const DEFAULT_MODEL: &str = "qwen2.5-coder:32b-instruct";
const MAX_AGENT_STEPS: usize = 12;

#[derive(Parser)]
#[command(name = "uos", version, about = "UnifiedOS: every app in its own VM, run by a local AI agent")]
struct Cli {
    /// Address of the local model server.
    #[arg(long, global = true, env = "UOS_OLLAMA_URL", default_value = DEFAULT_BASE_URL)]
    ollama_url: String,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Args)]
struct ModelArgs {
    #[arg(long, env = "UOS_MODEL", default_value = DEFAULT_MODEL)]
    model: String,
    /// Context window to load the model with, in tokens.
    #[arg(long, default_value_t = 16384)]
    context: u32,
    /// Most GPU memory the model may use, in MiB. Layers that do not fit run from RAM.
    #[arg(long)]
    vram_limit: Option<u64>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show this machine's hardware and where the model would be placed.
    Hw {
        #[command(flatten)]
        model: ModelArgs,
    },
    /// Ask the agent to do something in plain language.
    Ask {
        #[arg(required = true)]
        request: Vec<String>,
        #[command(flatten)]
        model: ModelArgs,
        /// Do not ask before ordinary changes. Privileged actions still ask.
        #[arg(long)]
        yes: bool,
    },
    /// Run a Linux or Windows program in its own virtual machine.
    Run {
        app: PathBuf,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Guest memory in MiB.
        #[arg(long, default_value_t = 1024)]
        memory: u32,
        #[arg(long, default_value_t = 2)]
        cpus: u32,
        /// Let the program change files in its own folder (read-only by default).
        #[arg(long)]
        writable: bool,
        /// Stop the program after this many seconds.
        #[arg(long)]
        timeout: Option<u64>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("uos: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let ollama = Ollama::new(cli.ollama_url);
    match cli.command {
        Cmd::Hw { model } => {
            let (hardware, profile, plan) = placement(&ollama, &model)?;
            print!("{}", report::hardware(&hardware));
            print!("{}", report::placement(&profile, &plan));
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Ask {
            request,
            model,
            yes,
        } => {
            let (_, _, plan) = placement(&ollama, &model)?;
            let config = AgentConfig {
                model: model.model,
                gpu_layers: Some(plan.gpu_layers),
                context_tokens: Some(model.context),
                max_steps: MAX_AGENT_STEPS,
                policy: Policy {
                    auto_approve_modify: yes,
                },
            };
            let audit = AuditLog::new(AuditLog::default_path());
            let tools = default_tools(Paths::from_env()?);
            let agent = Agent::new(&ollama, &TerminalApprover, tools, audit, config);
            println!("{}", agent.run(&request.join(" "))?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Run {
            app,
            args,
            memory,
            cpus,
            writable,
            timeout,
        } => {
            let request = LaunchRequest {
                app: &app,
                args: &args,
                memory_mib: memory,
                cpus,
                interactive: true,
                timeout: timeout.map(Duration::from_secs),
                writable_share: writable,
            };
            let outcome = uos_vm::launch(&request, &Paths::from_env()?, &mut io::stdout())?;
            // Exit statuses are a single byte; anything else reads as failure.
            Ok(u8::try_from(outcome.exit_code).map_or(ExitCode::FAILURE, ExitCode::from))
        }
    }
}

/// Reads the machine's live state and decides where the model goes.
fn placement(
    ollama: &Ollama,
    args: &ModelArgs,
) -> anyhow::Result<(HardwareSnapshot, ModelProfile, TierPlan)> {
    let hardware = uos_hw::probe()?;
    let profile = ollama
        .model_profile(&args.model)
        .with_context(|| format!("could not inspect model '{}'", args.model))?;
    let options = PlanOptions {
        context_tokens: args.context,
        reclaimable_vram_mib: ollama.loaded_vram_mib()?,
        vram_limit_mib: args.vram_limit,
        reserve: Reserve::default(),
    };
    let plan = uos_hw::plan(&profile, &hardware, &options);
    Ok((hardware, profile, plan))
}

/// Asks on the terminal. Anything but an explicit yes is a refusal, including
/// a closed or non-interactive stdin.
struct TerminalApprover;

impl Approver for TerminalApprover {
    fn approve(&self, tool: &str, args: &Value, risk: Risk) -> bool {
        eprintln!("\nThe agent wants to run {tool} [{risk:?}]:\n  {args}");
        eprint!("Allow? [y/N] ");
        let _ = io::stderr().flush();
        let mut answer = String::new();
        match io::stdin().lock().read_line(&mut answer) {
            Ok(_) => matches!(answer.trim(), "y" | "Y" | "yes"),
            Err(_) => false,
        }
    }
}
