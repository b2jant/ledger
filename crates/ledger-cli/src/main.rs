//! `ledger` CLI binary entrypoint (`init`, `mcp`, `daemon`, `index`, `status`, `query`, `expand`, `bench`).
//!
//! Library crates enforce strict error types (`thiserror`); `anyhow` is used only here
//! at the top-level CLI boundary.

mod bench;
mod phase2_bench;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use ledger_core::{
    AnchorInput, ContextRequest, ExpandDelivery, ExpandRequest, FocusMode, PackFormatter, QueryMode,
};
use ledger_daemon::{DaemonClient, LedgerService, SocketClient, run_daemon_server};
use ledger_mcp::McpServer;

#[derive(Debug, Parser)]
#[command(
    name = "ledger",
    version,
    about = "Evidence-backed codebase context compiler & MCP server"
)]
struct Cli {
    /// Workspace root directory (defaults to current working directory).
    #[arg(long, global = true, default_value = ".")]
    workspace: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliMode {
    Auto,
    Enhance,
    Implement,
    Debug,
    BlastRadius,
    History,
}

impl From<CliMode> for QueryMode {
    fn from(value: CliMode) -> Self {
        match value {
            CliMode::Auto => Self::Auto,
            CliMode::Enhance => Self::Enhance,
            CliMode::Implement => Self::Implement,
            CliMode::Debug => Self::Debug,
            CliMode::BlastRadius => Self::BlastRadius,
            CliMode::History => Self::History,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialize `.ledger/` in the workspace, configure `.gitignore`, and optionally install `/enhance` harness skills.
    Init {
        /// Install thin `/ledger-enhance` skills into `.claude/commands/` and `.agents/skills/`.
        #[arg(long)]
        install_skills: bool,
    },
    /// Start the `stdio` MCP server for Claude Code, Codex, or Antigravity (connects to shared Owner Daemon).
    Mcp,
    /// Run the single per-workspace Owner Daemon (auto-spawned by `ledger mcp`).
    Daemon {
        /// Idle shutdown timeout in seconds when no clients are connected.
        #[arg(long, default_value_t = 900)]
        idle_seconds: u64,
    },
    /// Gracefully drain and stop the workspace owner daemon.
    Stop,
    /// Gracefully stop then start the workspace owner daemon.
    Restart,
    /// Force a synchronous workspace index generation commit.
    Index,
    /// Display workspace generation status, graph counts, and recent MCP usage telemetry.
    Status,
    /// Run a `ledger_context` query from the terminal.
    Query {
        /// Task description or natural-language query.
        task: String,
        /// Optional literal, symbol, route, or path anchor(s).
        #[arg(short, long)]
        anchor: Vec<String>,
        /// Query mode (`auto`, `enhance`, `implement`, `debug`, `blast-radius`, `history`).
        #[arg(short, long, value_enum, default_value_t = CliMode::Auto)]
        mode: CliMode,
        /// Maximum approximate output tokens.
        #[arg(long, default_value_t = 2800)]
        max_tokens: u32,
        /// Wait for a synchronized index pass before querying.
        #[arg(long)]
        fresh: bool,
    },
    /// Expand specific item IDs from a `snap:...` handle.
    Expand {
        /// Snapshot handle returned by `ledger query` / `ledger_context`.
        handle: String,
        /// Expansion item IDs (`body:sym_...`, `callers:sym_...`, `slice:path:start-end`).
        #[arg(required = true)]
        items: Vec<String>,
        /// Use delta omission if content digest has not changed.
        #[arg(long)]
        delta: bool,
    },
    /// Run the Phase 1 & Phase 2 (`B0` vs `L1` vs `L2` vs `L3`) reproducible ablation benchmarks.
    Bench,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_stderr_tracing();

    match cli.command {
        Commands::Init { install_skills } => cmd_init(&cli.workspace, install_skills)?,
        Commands::Mcp => cmd_mcp(&cli.workspace)?,
        Commands::Daemon { idle_seconds } => {
            run_daemon_server(&cli.workspace, Duration::from_secs(idle_seconds)).await?;
        }
        Commands::Stop => {
            SocketClient::stop_workspace(&cli.workspace)?;
            println!("Workspace daemon stopped.");
        }
        Commands::Restart => {
            SocketClient::stop_workspace(&cli.workspace)?;
            let _ = DaemonClient::connect(&cli.workspace)?;
            println!("Workspace daemon restarted.");
        }
        Commands::Index => {
            let client = DaemonClient::connect(&cli.workspace)?;
            let generation = client.index_now()?;
            let stats = client.status()?;
            println!(
                "Published generation g{generation}: {} files, {} symbols, {} resolved edges, {} VCS revisions (head: {}, workspace: {}).",
                stats.file_count,
                stats.symbol_count,
                stats.edge_count,
                stats.indexed_revisions,
                stats.vcs_head.as_deref().unwrap_or("none"),
                stats.workspace_id
            );
        }
        Commands::Status => cmd_status(&cli.workspace)?,
        Commands::Query {
            task,
            anchor,
            mode,
            max_tokens,
            fresh,
        } => {
            let client = DaemonClient::connect(&cli.workspace)?;
            if fresh {
                client.index_now()?;
            }
            let req = ContextRequest {
                task,
                anchors: anchor.into_iter().map(AnchorInput::Plain).collect(),
                mode: mode.into(),
                focus_paths: Vec::new(),
                focus_mode: FocusMode::Boost,
                max_approx_tokens: max_tokens,
            };
            let pack = client.query_context(&req)?;
            print!("{}", PackFormatter::render_context_pack(&pack));
        }
        Commands::Expand {
            handle,
            items,
            delta,
        } => {
            let client = DaemonClient::connect(&cli.workspace)?;
            let req = ExpandRequest {
                handle,
                items,
                delivery: if delta {
                    ExpandDelivery::Delta
                } else {
                    ExpandDelivery::Complete
                },
                max_approx_tokens: 2400,
            };
            let resp = client.expand(&req)?;
            print!("{}", PackFormatter::render_expand_response(&resp));
        }
        Commands::Bench => {
            let _ = bench::run_phase1_benchmark()?;
            let phase2_report = phase2_bench::run_phase2_ablation_suite()?;
            println!("\n{phase2_report}");
        }
    }

    Ok(())
}

fn init_stderr_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
}

fn cmd_init(workspace: &Path, install_skills: bool) -> anyhow::Result<()> {
    let canonical = fs::canonicalize(workspace)?;
    let root = canonical.as_path();
    let client = DaemonClient::connect(root)?;

    // Ensure `.gitignore` excludes disposable SQLite/socket files while keeping portable annotations optional.
    let gitignore_path = root.join(".gitignore");
    let existing = fs::read_to_string(&gitignore_path).unwrap_or_default();
    if !existing.contains(".ledger/ledger.db") {
        let mut updated = existing;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(".ledger/ledger.db*\n.ledger/runtime/\n");
        fs::write(&gitignore_path, updated)?;
    }

    if install_skills {
        install_harness_skills(root)?;
    }

    let generation = client.index_now()?;
    let stats = client.status()?;
    println!(
        "Initialized LEDGER in {} (workspace_id={}, generation={}, vcs_head={}, files={}, symbols={}, edges={}, vcs_revisions={})",
        root.display(),
        stats.workspace_id,
        generation,
        stats.vcs_head.as_deref().unwrap_or("none"),
        stats.file_count,
        stats.symbol_count,
        stats.edge_count,
        stats.indexed_revisions
    );
    if install_skills {
        println!(
            "Installed ✨ prompt-enhancement skills in `.claude/commands/ledger-enhance.md` and `.agents/skills/ledger-enhance/SKILL.md`."
        );
    }
    Ok(())
}

fn install_harness_skills(root: &Path) -> std::io::Result<()> {
    let claude_dir = root.join(".claude/commands");
    fs::create_dir_all(&claude_dir)?;
    fs::write(
        claude_dir.join("ledger-enhance.md"),
        "Call `ledger_context` with `mode: \"enhance\"` and `task: \"$ARGUMENTS\"`. Using ONLY the verified `FILES_TO_EDIT`, `FILES_TO_REFERENCE_AND_RESPECT`, `CO_CHANGE_BLAST_RADIUS`, and `VERIFICATION_COMMANDS` returned by LEDGER, present a concise 5-line grounded task brief, fetch any needed full bodies or diffs via `ledger_expand`, and execute cleanly without running broad exploratory `grep` scans.\n",
    )?;

    let agent_skill_dir = root.join(".agents/skills/ledger-enhance");
    fs::create_dir_all(&agent_skill_dir)?;
    fs::write(
        agent_skill_dir.join("SKILL.md"),
        "---\nname: ledger-enhance\ndescription: Ground a rough coding request into exact repository files to edit, contracts/callers to reference, historical co-change blast radius, and verification tests using the LEDGER MCP server.\n---\n\n1. Call `ledger_context(task, mode: \"enhance\")` first.\n2. Summarize the grounded `FILES_TO_EDIT`, `FILES_TO_REFERENCE_AND_RESPECT`, `CO_CHANGE_BLAST_RADIUS`, and `VERIFICATION_COMMANDS`.\n3. Use `ledger_expand(handle, items)` to retrieve any omitted function bodies or `diff:<rev>:<path>` historical diffs before editing.\n4. Fall back to native search only if `COVERAGE_NOTES` indicates missing coverage.\n",
    )?;
    Ok(())
}

fn cmd_mcp(workspace: &Path) -> anyhow::Result<()> {
    let client = DaemonClient::connect(workspace)?;
    let server = McpServer::new(client);
    let stdin = io::stdin();
    let stdout = io::stdout();
    server.serve_stdio(stdin.lock(), stdout.lock())?;
    Ok(())
}

fn cmd_status(workspace: &Path) -> anyhow::Result<()> {
    let client = DaemonClient::connect(workspace)?;
    let stats = client.status()?;
    println!("LEDGER Workspace Status");
    println!("  workspace_id:        {}", stats.workspace_id);
    println!("  db_incarnation:      {}", stats.db_incarnation);
    println!("  schema_version:      v{}", stats.schema_version);
    println!("  current_generation:  g{}", stats.current_generation);
    println!(
        "  vcs_head:            {}",
        stats.vcs_head.as_deref().unwrap_or("none")
    );
    println!("  indexed_files:       {}", stats.file_count);
    println!("  indexed_symbols:     {}", stats.symbol_count);
    println!("  resolved_edges:      {}", stats.edge_count);
    println!("  indexed_revisions:   {}", stats.indexed_revisions);
    println!("  mcp_queries_served:  {}", stats.total_queries_served);

    if !stats.recent_telemetry.is_empty() {
        println!("\nRecent MCP / Query Observability:");
        for t in &stats.recent_telemetry {
            println!(
                "  - [g{} | {} ({}) | {:.2} ms | ~{} tok] \"{}\" -> edit={:?}, ref={:?}",
                t.generation,
                t.tool_name,
                t.mode,
                (t.latency_us as f64) / 1000.0,
                t.approx_tokens,
                t.query_summary,
                t.edit_target_files,
                t.reference_files
            );
        }
    }
    Ok(())
}
