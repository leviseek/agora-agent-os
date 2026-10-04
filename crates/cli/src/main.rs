//! agentos - the Agent OS command line interface.
//!
//! Every command works in-process against a bootstrapped kernel. Commands that have a remote
//! counterpart also accept --remote <grpc-endpoint> and then go through the gRPC client instead,
//! which is how a CLI on one machine drives a runtime on another.

use agentos_core::config::RuntimeConfig;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{EventFilter, TaskGraphRecord, TaskKind, TaskPayload, TaskRecord};
use agentos_core::{SessionId, TaskId};
use agentos_kernel::Kernel;
use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "agentos",
    version,
    about = "Agent Operating System runtime, CLI and diagnostics",
    long_about = "A distributed agent runtime: session actors, an agent loop, a capability mesh, \
                  a task scheduler, durable state and an event log."
)]
struct Cli {
    /// Configuration file (JSON). Defaults to AGENTOS_CONFIG or ./config/agora-agent-os.json.
    #[arg(long, global = true)]
    config: Option<String>,
    /// Drive a remote runtime over gRPC instead of bootstrapping one in-process.
    #[arg(long, global = true)]
    remote: Option<String>,
    /// Emit machine readable JSON where the command has a human default.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the runtime: HTTP/WS gateway plus the gRPC endpoint.
    Start(StartArgs),
    /// Session lifecycle and messaging.
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Task graph operations.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Capability discovery and invocation.
    Capability {
        #[command(subcommand)]
        cmd: CapabilityCmd,
    },
    /// Worker and placement operations.
    Worker {
        #[command(subcommand)]
        cmd: WorkerCmd,
    },
    /// Actor operations, including migration.
    Actor {
        #[command(subcommand)]
        cmd: ActorCmd,
    },
    /// Inspect agent runs.
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Run the full acceptance scenario and print a report.
    Demo(DemoArgs),
    /// Check configuration, storage, capabilities and providers.
    Doctor,
}

#[derive(Args, Debug)]
struct StartArgs {
    /// Override the HTTP address (host:port).
    #[arg(long)]
    http: Option<String>,
    /// Override the gRPC address (host:port).
    #[arg(long)]
    grpc: Option<String>,
}

#[derive(Subcommand, Debug)]
enum SessionCmd {
    /// Create a session.
    Create {
        #[arg(long, default_value = "cli-user")]
        user: String,
        #[arg(long, default_value = "cli session")]
        title: String,
    },
    /// List sessions.
    List,
    /// Show a session and its runtime state.
    Show { session_id: String },
    /// Send a goal to a session and wait for the agent loop to finish.
    Message {
        session_id: String,
        text: String,
        /// Return as soon as the goal is queued instead of waiting.
        #[arg(long)]
        no_wait: bool,
    },
    /// Cancel the run in flight.
    Cancel { session_id: String },
    /// Close a session.
    Close { session_id: String },
    /// Print the event log of a session.
    Events {
        session_id: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Export an actor snapshot.
    Snapshot {
        session_id: String,
        /// Write the checkpoint to this file instead of stdout.
        #[arg(long)]
        out: Option<String>,
    },
    /// Restore an actor from a snapshot file.
    Restore { file: String },
    /// Migrate the session actor (checkpoint -> transfer -> restore -> replay).
    Migrate {
        session_id: String,
        #[arg(long)]
        target_worker: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum TaskCmd {
    /// Create and run a task graph.
    Create {
        session_id: String,
        /// Capability to invoke, repeatable: --capability echo (uses --input for each).
        #[arg(long, value_delimiter = ',')]
        capability: Vec<String>,
        /// JSON input for every capability node.
        #[arg(long, default_value = "{}")]
        input: String,
        /// Add a final join node depending on all capability nodes.
        #[arg(long, default_value_t = true)]
        join: bool,
    },
    /// List task graph nodes.
    List {
        #[arg(long)]
        session_id: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum CapabilityCmd {
    /// List registered capabilities.
    List {
        #[arg(long)]
        query: Option<String>,
    },
    /// Show one capability descriptor.
    Describe { name: String },
    /// Invoke a capability directly.
    Invoke {
        name: String,
        #[arg(long, default_value = "{}")]
        input: String,
        #[arg(long)]
        session_id: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum WorkerCmd {
    /// List workers and their load.
    List,
    /// Show placement policy and the current decision inputs.
    Placement,
}

#[derive(Subcommand, Debug)]
enum ActorCmd {
    /// List live actors and directory entries.
    List,
    /// Take a checkpoint of an actor and persist it.
    Checkpoint { actor_id: String },
}

#[derive(Subcommand, Debug)]
enum AgentCmd {
    /// Show the last agent run of a session: plan, steps, observations and final answer.
    Inspect { session_id: String },
}

#[derive(Args, Debug)]
struct DemoArgs {
    /// Goal to run through the agent loop.
    #[arg(long, default_value = "what is 21*2 and then read notes.txt")]
    goal: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Some(config) = &cli.config {
        // Safe: single threaded at this point, before any runtime work starts.
        std::env::set_var("AGENTOS_CONFIG", config);
    }
    if let Err(e) = run(cli).await {
        eprintln!("error [{}]: {}", e.code(), e.message);
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let config = RuntimeConfig::load()?;
    agentos_core::telemetry::init_tracing(&config.observability.log_level, matches!(config.observability.log_format, agentos_core::config::LogFormat::Json))?;
    for warning in &config.warnings {
        tracing::warn!("{warning}");
    }

    match cli.command {
        Command::Start(args) => start(config, args).await,
        Command::Doctor => doctor(config).await,
        Command::Demo(args) => demo(config, args).await,
        Command::Session { cmd } if cli.remote.is_some() => {
            remote_session_cmd(cli.remote.as_deref().unwrap(), cmd, cli.json).await
        }
        other => {
            let kernel: Arc<Kernel> = Kernel::bootstrap(config).await?;
            match other {
                Command::Session { cmd } => session_cmd(kernel, cmd, cli.json).await,
                Command::Task { cmd } => task_cmd(kernel, cmd, cli.json).await,
                Command::Capability { cmd } => capability_cmd(kernel, cmd, cli.json).await,
                Command::Worker { cmd } => worker_cmd(kernel, cmd, cli.json).await,
                Command::Actor { cmd } => actor_cmd(kernel, cmd, cli.json).await,
                Command::Agent { cmd } => agent_cmd(kernel, cmd, cli.json).await,
                _ => unreachable!("handled above"),
            }
        }
    }
}

async fn start(mut config: RuntimeConfig, args: StartArgs) -> Result<()> {
    if let Some(http) = args.http {
        config.api.http_addr = http;
    }
    if let Some(grpc) = args.grpc {
        config.api.grpc_addr = grpc;
    }
    let kernel = Kernel::bootstrap(config).await?;
    let shutdown = tokio_util::sync::CancellationToken::new();

    let http = {
        let kernel = kernel.clone();
        let token = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = agentos_api::serve(kernel, token).await {
                tracing::error!(error = %e, "gateway stopped");
            }
        })
    };
    let grpc_addr = kernel.config.api.grpc_addr.clone();
    let grpc = {
        let kernel = kernel.clone();
        let token = shutdown.clone();
        tokio::spawn(async move {
            match agentos_kernel::transports::serve_grpc(kernel, grpc_addr, token).await {
                Ok(addr) => tracing::info!(%addr, "gRPC endpoint listening"),
                Err(e) => tracing::error!(error = %e, "gRPC server stopped"),
            }
        })
    };

    let health = kernel.health().await?;
    println!("agentos runtime started");
    println!("  node          : {}", health.node);
    println!("  http/ws       : http://{}{}", kernel.config.api.http_addr, kernel.config.api.ws_path);
    println!("  grpc          : {}", kernel.config.api.grpc_addr);
    println!("  store         : {}", kernel.store.backend_name());
    println!("  capabilities  : {}", health.capabilities);
    println!("  workers       : {}", health.workers_online);
    println!("  workspace     : {}", kernel.config.policy.workspace_root.display());
    println!("press ctrl-c to stop");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("\nshutting down");
        }
        _ = async {
            let _ = http.await;
        } => {}
        _ = async {
            let _ = grpc.await;
        } => {}
    }
    shutdown.cancel();
    kernel.shutdown().await;
    Ok(())
}

fn print_value(value: &Value, as_json: bool) {
    if as_json {
        println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
    } else {
        println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
    }
}

/// Drive a runtime on another machine through the AgentService gRPC client. This is the same
/// code path a node-to-node caller would use; the runtime cannot tell the difference.
async fn remote_session_cmd(endpoint: &str, cmd: SessionCmd, json_out: bool) -> Result<()> {
    let client = agentos_network::grpc::GrpcAgentClient::connect(endpoint.to_string()).await?;
    match cmd {
        SessionCmd::Create { user, title } => {
            let (session, actor) = client.create_session(&user, &title).await?;
            print_value(&json!({ "session_id": session, "actor_id": actor, "endpoint": endpoint }), json_out);
        }
        SessionCmd::List => {
            let sessions = client.list_sessions().await?;
            if json_out {
                print_value(&sessions, true);
            } else {
                println!("{:<34} {:<10} {:<12} {}", "SESSION", "STATE", "MESSAGES", "TITLE");
                if let Some(list) = sessions.as_array() {
                    for s in list {
                        println!(
                            "{:<34} {:<10} {:<12} {}",
                            s["id"].as_str().unwrap_or("-"),
                            s["state"].as_str().unwrap_or("-"),
                            s["message_count"],
                            s["title"].as_str().unwrap_or("-")
                        );
                    }
                }
            }
        }
        SessionCmd::Show { session_id } => {
            let session = parse_session(&session_id)?;
            print_value(&client.get_session(&session).await?, json_out);
        }
        SessionCmd::Message { session_id, text, no_wait } => {
            let session = parse_session(&session_id)?;
            let result = client.post_goal(&session, &text, !no_wait).await?;
            print_value(&result, json_out);
        }
        SessionCmd::Cancel { session_id } => {
            let session = parse_session(&session_id)?;
            println!("cancelled: {}", client.cancel(&session).await?);
        }
        SessionCmd::Close { session_id } => {
            let session = parse_session(&session_id)?;
            client.close_session(&session).await?;
            println!("closed {session_id}");
        }
        SessionCmd::Events { session_id, limit } => {
            let session = parse_session(&session_id)?;
            let events = client.list_events(&session, limit as u32).await?;
            if json_out {
                print_value(&events, true);
            } else if let Some(list) = events.as_array() {
                println!("{:<6} {:<22} {:<8} {}", "SEQ", "KIND", "SEVERITY", "MESSAGE");
                for e in list {
                    println!(
                        "{:<6} {:<22} {:<8} {}",
                        e["seq"],
                        e["kind"].as_str().unwrap_or("-"),
                        e["severity"].as_str().unwrap_or("-"),
                        e["message"].as_str().unwrap_or("-")
                    );
                }
            }
        }
        SessionCmd::Snapshot { session_id, out } => {
            let session = parse_session(&session_id)?;
            let checkpoint = client.snapshot(&session).await?;
            let text = serde_json::to_string_pretty(&checkpoint)?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &text)?;
                    println!("checkpoint written to {path}");
                }
                None => println!("{text}"),
            }
        }
        SessionCmd::Migrate { session_id, target_worker } => {
            let session = parse_session(&session_id)?;
            let report = client.migrate(&session, target_worker).await?;
            print_value(&report, json_out);
        }
        SessionCmd::Restore { .. } => {
            return Err(RuntimeError::invalid_input(
                "--remote restore is not wired yet: use the in-process CLI for snapshot restore",
            ));
        }
    }
    Ok(())
}

async fn session_cmd(kernel: Arc<Kernel>, cmd: SessionCmd, json_out: bool) -> Result<()> {
    match cmd {
        SessionCmd::Create { user, title } => {
            let record = kernel.sessions.create_session(&user, &title).await?;
            print_value(&serde_json::to_value(&record)?, json_out);
        }
        SessionCmd::List => {
            let sessions = kernel.sessions.list().await?;
            if json_out {
                print_value(&serde_json::to_value(&sessions)?, true);
            } else {
                println!("{:<34} {:<10} {:<12} {}", "SESSION", "STATE", "MESSAGES", "TITLE");
                for s in sessions {
                    println!("{:<34} {:<10} {:<12} {}", s.id, s.state.as_str(), s.message_count, s.title);
                }
            }
        }
        SessionCmd::Show { session_id } => {
            let session = parse_session(&session_id)?;
            let record = kernel.sessions.get(&session).await?;
            let status = kernel.sessions.status(&session).await.unwrap_or(Value::Null);
            print_value(&json!({ "session": record, "runtime": status }), json_out);
        }
        SessionCmd::Message { session_id, text, no_wait } => {
            let session = parse_session(&session_id)?;
            if no_wait {
                kernel.sessions.post_goal_async(&session, &text, &[]).await?;
                println!("accepted");
            } else {
                let result = kernel.sessions.post_goal(&session, &text, &[]).await?;
                print_value(&result, json_out);
            }
        }
        SessionCmd::Cancel { session_id } => {
            let session = parse_session(&session_id)?;
            println!("cancelled: {}", kernel.sessions.cancel(&session).await?);
        }
        SessionCmd::Close { session_id } => {
            let session = parse_session(&session_id)?;
            kernel.sessions.close(&session).await?;
            println!("closed {session_id}");
        }
        SessionCmd::Events { session_id, limit } => {
            let session = parse_session(&session_id)?;
            let events = kernel.sessions.events(&session, limit).await?;
            if json_out {
                print_value(&serde_json::to_value(&events)?, true);
            } else {
                println!("{:<6} {:<22} {:<8} {}", "SEQ", "KIND", "SEVERITY", "MESSAGE");
                for e in events {
                    println!("{:<6} {:<22} {:<8} {}", e.seq, e.kind.as_str(), e.severity.as_str(), e.message);
                }
            }
        }
        SessionCmd::Snapshot { session_id, out } => {
            let session = parse_session(&session_id)?;
            let checkpoint = kernel.sessions.snapshot(&session).await?;
            let text = serde_json::to_string_pretty(&checkpoint)?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &text)?;
                    println!("checkpoint written to {path} ({} bytes)", text.len());
                }
                None => println!("{text}"),
            }
        }
        SessionCmd::Restore { file } => {
            let text = std::fs::read_to_string(&file)?;
            let checkpoint: agentos_core::model::Checkpoint = serde_json::from_str(&text)
                .map_err(|e| RuntimeError::invalid_input(format!("invalid checkpoint: {e}")))?;
            let actor = kernel.sessions.restore(checkpoint).await?;
            println!("restored actor {actor}");
        }
        SessionCmd::Migrate { session_id, target_worker } => {
            let session = parse_session(&session_id)?;
            let report = kernel.sessions.migrate(&session, target_worker).await?;
            print_value(&serde_json::to_value(&report)?, json_out);
        }
    }
    Ok(())
}

async fn task_cmd(kernel: Arc<Kernel>, cmd: TaskCmd, json_out: bool) -> Result<()> {
    match cmd {
        TaskCmd::Create { session_id, capability, input, join } => {
            let session = parse_session(&session_id)?;
            let input: Value = serde_json::from_str(&input)
                .map_err(|e| RuntimeError::invalid_input(format!("--input must be JSON: {e}")))?;
            let mut builder = agentos_task_scheduler::graph::TaskGraphBuilder::new(session.clone(), "cli graph");
            let graph_id = builder.graph_id();
            let mut ids: Vec<TaskId> = Vec::new();
            for name in &capability {
                let mut node = TaskRecord::new(
                    graph_id.clone(),
                    session.clone(),
                    format!("run {name}"),
                    TaskKind::Capability,
                    TaskPayload::Capability { capability: name.clone(), version: None, input: input.clone() },
                );
                node.max_attempts = 3;
                node.timeout_ms = kernel.config.policy.capability_timeout_ms;
                node.labels.insert("capability".into(), name.clone());
                ids.push(builder.add(node));
            }
            if join && !ids.is_empty() {
                let node = TaskRecord::new(
                    graph_id.clone(),
                    session.clone(),
                    "join",
                    TaskKind::Join,
                    TaskPayload::Join { template: "collected".into() },
                )
                .with_deps(ids.clone())
                .with_max_attempts(1);
                builder.add(node);
            }
            let graph = builder.build()?;

            let runner = Arc::new(CliTaskRunner { kernel: kernel.clone(), session: session.clone() });
            let scheduler = agentos_task_scheduler::scheduler::Scheduler::new(
                kernel.store.clone(),
                kernel.bus.clone(),
                runner,
                agentos_task_scheduler::scheduler::SchedulerConfig {
                    max_concurrency: kernel.config.policy.max_concurrent_tasks,
                    ..Default::default()
                },
            );
            let outcome = scheduler.run(graph).await?;
            print_value(
                &json!({
                    "graph_id": outcome.graph_id,
                    "state": outcome.state.as_str(),
                    "succeeded": outcome.succeeded,
                    "failed": outcome.failed,
                    "cancelled": outcome.cancelled,
                    "duration_ms": outcome.duration_ms,
                    "nodes": outcome.nodes.iter().map(|n| json!({
                        "id": n.id,
                        "title": n.title,
                        "state": n.state.as_str(),
                        "attempts": n.attempts,
                        "duration_ms": n.duration_ms(),
                        "result": n.result,
                        "error": n.error,
                    })).collect::<Vec<_>>(),
                }),
                json_out,
            );
        }
        TaskCmd::List { session_id } => {
            use agentos_storage::store::{collections, Collection};
            let collection: Collection<TaskRecord> = Collection::new(collections::TASKS);
            let mut tasks = collection.list(kernel.store.as_ref(), 1000).await?;
            if let Some(session) = &session_id {
                tasks.retain(|t| t.session_id.as_str() == session.as_str());
            }
            tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at));
            if json_out {
                print_value(&serde_json::to_value(&tasks)?, true);
            } else {
                println!("{:<34} {:<12} {:<10} {:<6} {}", "TASK", "STATE", "KIND", "TRY", "TITLE");
                for t in tasks {
                    println!("{:<34} {:<12} {:<10} {:<6} {}", t.id, t.state.as_str(), t.kind.as_str(), t.attempts, t.title);
                }
            }
        }
    }
    Ok(())
}

struct CliTaskRunner {
    kernel: Arc<Kernel>,
    session: SessionId,
}

#[async_trait::async_trait]
impl agentos_task_scheduler::scheduler::TaskRunner for CliTaskRunner {
    async fn run(
        &self,
        node: &TaskRecord,
        ctx: agentos_task_scheduler::scheduler::TaskContext,
    ) -> Result<Value> {
        match &node.payload {
            TaskPayload::Capability { capability, input, .. } => {
                let caller = agentos_capability_runtime::capability::CallerContext {
                    session_id: self.session.clone(),
                    actor_id: None,
                    task_id: Some(node.id.clone()),
                    correlation: ctx.correlation.clone(),
                    cancellation: ctx.cancellation.clone(),
                };
                Ok(self.kernel.mesh.invoke(capability, None, input.clone(), caller).await?.output)
            }
            TaskPayload::Join { template } => Ok(json!({
                "template": template,
                "inputs": ctx.dependency_outputs,
            })),
            TaskPayload::Model { prompt, .. } => Ok(json!({ "prompt": prompt })),
            TaskPayload::Agent { goal, .. } => Ok(json!({ "goal": goal })),
        }
    }
}

async fn capability_cmd(kernel: Arc<Kernel>, cmd: CapabilityCmd, json_out: bool) -> Result<()> {
    match cmd {
        CapabilityCmd::List { query } => {
            let discovery = agentos_capability_runtime::registry::DiscoveryQuery {
                name_contains: query.unwrap_or_default(),
                ..Default::default()
            };
            let list = kernel.registry.discover(&discovery);
            if json_out {
                print_value(&serde_json::to_value(&list)?, true);
            } else {
                println!("{:<20} {:<10} {:<10} {:<20} {}", "NAME", "VERSION", "KIND", "PERMISSION", "DESCRIPTION");
                for c in list {
                    println!(
                        "{:<20} {:<10} {:<10} {:<20} {}",
                        c.name,
                        c.version,
                        format!("{:?}", c.kind).to_lowercase(),
                        c.permission.summary(),
                        truncate(&c.description, 60)
                    );
                }
            }
        }
        CapabilityCmd::Describe { name } => {
            let found = kernel
                .registry
                .describe(&name, &agentos_core::model::VersionReq::any())
                .map_err(|e| RuntimeError::not_found(format!("{name}: {e}")))?;
            print_value(&serde_json::to_value(&found)?, json_out);
        }
        CapabilityCmd::Invoke { name, input, session_id } => {
            let input: Value = serde_json::from_str(&input)
                .map_err(|e| RuntimeError::invalid_input(format!("--input must be JSON: {e}")))?;
            let session = session_id.map(SessionId::from_raw).unwrap_or_else(SessionId::new);
            let caller = agentos_capability_runtime::capability::CallerContext::new(session);
            let result = kernel.mesh.invoke(&name, None, input, caller).await?;
            print_value(
                &json!({
                    "capability": result.name,
                    "version": result.version,
                    "output": result.output,
                    "duration_ms": result.duration_ms,
                    "attempts": result.attempts,
                }),
                json_out,
            );
        }
    }
    Ok(())
}

async fn worker_cmd(kernel: Arc<Kernel>, cmd: WorkerCmd, json_out: bool) -> Result<()> {
    match cmd {
        WorkerCmd::List => {
            let workers = kernel.workers.list();
            if json_out {
                print_value(&serde_json::to_value(&workers)?, true);
            } else {
                println!("{:<34} {:<10} {:<8} {:<8} {:<10} {}", "WORKER", "STATE", "ACTORS", "TASKS", "PRESSURE", "NAME");
                for w in workers {
                    println!(
                        "{:<34} {:<10} {:<8} {:<8} {:<10.2} {}",
                        w.id,
                        w.state.as_str(),
                        w.load.actors,
                        w.load.running_tasks,
                        w.load.pressure(&w.capacity),
                        w.name
                    );
                }
            }
        }
        WorkerCmd::Placement => {
            print_value(
                &json!({
                    "strategy": format!("{:?}", kernel.placement.policy().strategy),
                    "max_pressure": kernel.placement.policy().max_pressure,
                    "workers_online": kernel.workers.online(),
                    "live_actors": kernel.actors.list().len(),
                }),
                json_out,
            );
        }
    }
    Ok(())
}

async fn actor_cmd(kernel: Arc<Kernel>, cmd: ActorCmd, json_out: bool) -> Result<()> {
    match cmd {
        ActorCmd::List => {
            let records = kernel.actors.records();
            let directory = kernel.directory.list().await?;
            print_value(
                &json!({
                    "live": records,
                    "directory": directory,
                    "cache": {
                        "hits": kernel.directory.cache_stats().0,
                        "misses": kernel.directory.cache_stats().1,
                    }
                }),
                json_out,
            );
        }
        ActorCmd::Checkpoint { actor_id } => {
            let actor = agentos_core::ActorId::parse(&actor_id)
                .map_err(|e| RuntimeError::invalid_input(e.to_string()))?;
            let checkpoint = kernel.actors.checkpoint(&actor).await?;
            print_value(&serde_json::to_value(&checkpoint.meta)?, json_out);
        }
    }
    Ok(())
}

async fn agent_cmd(kernel: Arc<Kernel>, cmd: AgentCmd, json_out: bool) -> Result<()> {
    match cmd {
        AgentCmd::Inspect { session_id } => {
            let session = parse_session(&session_id)?;
            let run = kernel.sessions.last_run(&session).await?;
            let record = kernel.sessions.get(&session).await?;
            if json_out {
                print_value(&json!({ "session": record, "run": run }), true);
            } else {
                println!("session : {} [{}]", session_id, record.as_ref().map(|r| r.state.as_str()).unwrap_or("unknown"));
                println!("goal    : {}", run["goal"].as_str().unwrap_or("-"));
                println!("state   : {}", run["state"].as_str().unwrap_or("-"));
                println!("model   : {}", run["model"].as_str().unwrap_or("-"));
                println!("answer  : {}", run["final_answer"].as_str().unwrap_or("-"));
                if let Some(error) = run["error"].as_str() {
                    println!("error   : {error}");
                }
                println!("steps   :");
                if let Some(steps) = run["steps"].as_array() {
                    for step in steps {
                        println!(
                            "  [{}] {:<9} {} {}",
                            step["index"],
                            step["kind"].as_str().unwrap_or("-"),
                            truncate(step["thought"].as_str().unwrap_or(""), 70),
                            step["observation"]["capability"].as_str().map(|c| format!("-> {c}")).unwrap_or_default()
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

async fn doctor(config: RuntimeConfig) -> Result<()> {
    println!("agentos doctor");
    println!("  config file     : {}", std::env::var("AGENTOS_CONFIG").unwrap_or_else(|_| "(defaults + environment)".into()));
    println!("  node            : {}", config.node.name);
    println!("  store backend   : {:?} at {}", config.storage.backend, config.storage.data_dir.display());
    println!("  workspace root  : {}", config.policy.workspace_root.display());
    println!("  http / grpc     : {} / {}", config.api.http_addr, config.api.grpc_addr);
    println!("  auth            : {}", if config.api.auth_required() { "bearer token required" } else { "open (no token configured)" });
    println!("  providers       :");
    for line in config.model_summary() {
        println!(
            "    {:<10} {:<10} configured={:<5} model={}",
            line["name"].as_str().unwrap_or("-"),
            line["kind"].as_str().unwrap_or("-"),
            line["configured"].as_bool().unwrap_or(false),
            line["model"].as_str().unwrap_or("-")
        );
    }
    config.ensure_dirs()?;
    let kernel = Kernel::bootstrap(config).await?;
    let health = kernel.health().await?;
    println!("  storage health  : ok ({} entries, backend {})", health.store.entries, health.store.backend);
    println!("  capabilities    : {}", health.capabilities);
    println!("  workers online  : {}", health.workers_online);
    println!("  wasm engine     : ready ({} instances live)", health.wasm_instances);
    println!("  checks passed");
    kernel.shutdown().await;
    Ok(())
}

async fn demo(config: RuntimeConfig, args: DemoArgs) -> Result<()> {
    let kernel = Kernel::bootstrap(config).await?;
    println!("== agentos demo ==");

    // 1. capabilities
    let names: Vec<String> = kernel.registry.list().into_iter().map(|c| format!("{}@{}", c.name, c.version)).collect();
    println!("capabilities : {}", names.join(", "));

    // 2. two sessions in parallel
    let a = kernel.sessions.create_session("demo", "session A").await?;
    let b = kernel.sessions.create_session("demo", "session B").await?;
    println!("sessions     : {} and {}", a.id, b.id);
    let (ra, rb) = tokio::join!(
        kernel.sessions.post_goal(&a.id, &args.goal, &[]),
        kernel.sessions.post_goal(&b.id, "what is 7*6?", &[])
    );
    println!("session A    : {}", summarize(&ra));
    println!("session B    : {}", summarize(&rb));

    // 3. snapshot and restore
    let checkpoint = kernel.sessions.snapshot(&a.id).await.unwrap();
    let handle = kernel.actors.lookup_session(&a.id).unwrap();
    kernel.actors.stop(&handle.id).await?;
    kernel.sessions.restore(checkpoint.clone()).await?;
    let status = kernel.sessions.status(&a.id).await?;
    println!(
        "snapshot     : {} ({} bytes, hash {}) restored with {} runs",
        checkpoint.meta.id,
        checkpoint.meta.bytes,
        &checkpoint.meta.state_hash[..12],
        status["runs"].as_array().map(|r| r.len()).unwrap_or(0)
    );

    // 4. migration pipeline
    let report = kernel.sessions.migrate(&a.id, None).await?;
    println!(
        "migration    : {} -> generation {}, {} events replayed in {} ms",
        report.state.as_str(),
        report.actor_id,
        report.replayed_events,
        report.duration_ms
    );

    // 5. events
    let events = kernel.bus.replay(EventFilter { limit: 200, ..Default::default() }).await?;
    println!("events       : {} recorded", events.len());
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for e in &events {
        *counts.entry(e.kind.as_str()).or_default() += 1;
    }
    for (kind, count) in counts {
        println!("               {kind:<24} {count}");
    }
    let graphs: agentos_storage::store::Collection<TaskGraphRecord> =
        agentos_storage::store::Collection::new(agentos_storage::store::collections::GRAPHS);
    let graphs = graphs.list(kernel.store.as_ref(), 10).await?;
    println!("task graphs  : {}", graphs.len());

    let health = kernel.health().await?;
    println!("health       : sessions={} actors={} workers={} capabilities={}", health.sessions, health.actors_live, health.workers_online, health.capabilities);
    kernel.shutdown().await;
    Ok(())
}

fn summarize(value: &Result<Value>) -> String {
    match value {
        Ok(v) => format!(
            "state={} steps={} answer={}",
            v["state"].as_str().unwrap_or("-"),
            v["steps"],
            truncate(v["answer"].as_str().unwrap_or("-"), 90)
        ),
        Err(e) => format!("error [{}] {}", e.code(), e.message),
    }
}

fn truncate(text: &str, max: usize) -> String {
    let cleaned = text.replace('\n', " ");
    if cleaned.chars().count() <= max {
        cleaned
    } else {
        cleaned.chars().take(max).collect::<String>() + "..."
    }
}

fn parse_session(id: &str) -> Result<SessionId> {
    SessionId::parse(id).map_err(|e| RuntimeError::invalid_input(e.to_string()))
}
