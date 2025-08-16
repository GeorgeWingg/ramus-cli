mod cli;
mod event_processor;
mod event_processor_with_human_output;
mod event_processor_with_json_output;
mod plan_emitter;

use std::io::IsTerminal;
use std::io::Read;
use std::path::PathBuf;

pub use cli::Cli;
use codex_core::BUILT_IN_OSS_MODEL_PROVIDER_ID;
use codex_core::ConversationManager;
use codex_core::NewConversation;
use codex_core::config::Config;
use codex_core::config::ConfigOverrides;
use codex_core::config_types::SandboxMode;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::Event;
use codex_core::protocol::EventMsg;
use codex_core::protocol::InputItem;
use codex_core::protocol::Op;
use codex_core::protocol::TaskCompleteEvent;
use codex_core::util::is_inside_git_repo;
use codex_ollama::DEFAULT_OSS_MODEL;
use event_processor_with_human_output::EventProcessorWithHumanOutput;
use event_processor_with_json_output::EventProcessorWithJsonOutput;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::event_processor::CodexStatus;
use crate::event_processor::EventProcessor;
use crate::plan_emitter::PlanEmitter;

pub async fn run_main(cli: Cli, codex_linux_sandbox_exe: Option<PathBuf>) -> anyhow::Result<()> {
    let Cli {
        images,
        model: model_cli_arg,
        oss,
        config_profile,
        full_auto,
        dangerously_bypass_approvals_and_sandbox,
        cwd,
        skip_git_repo_check,
        color,
        last_message_file,
        json: json_mode,
        sandbox_mode: sandbox_mode_cli_arg,
        plan_webhook,
        webhook_secret,
        plan_events,
        plan_state,
        task_id,
        run_id,
        emit_plan_stdout,
        include_plan_tool,
        prompt,
        config_overrides,
    } = cli;

    // Determine the prompt based on CLI arg and/or stdin.
    let prompt = match prompt {
        Some(p) if p != "-" => p,
        // Either `-` was passed or no positional arg.
        maybe_dash => {
            // When no arg (None) **and** stdin is a TTY, bail out early – unless the
            // user explicitly forced reading via `-`.
            let force_stdin = matches!(maybe_dash.as_deref(), Some("-"));

            if std::io::stdin().is_terminal() && !force_stdin {
                eprintln!(
                    "No prompt provided. Either specify one as an argument or pipe the prompt into stdin."
                );
                std::process::exit(1);
            }

            // Ensure the user knows we are waiting on stdin, as they may
            // have gotten into this state by mistake. If so, and they are not
            // writing to stdin, Codex will hang indefinitely, so this should
            // help them debug in that case.
            if !force_stdin {
                eprintln!("Reading prompt from stdin...");
            }
            let mut buffer = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut buffer) {
                eprintln!("Failed to read prompt from stdin: {e}");
                std::process::exit(1);
            } else if buffer.trim().is_empty() {
                eprintln!("No prompt provided via stdin.");
                std::process::exit(1);
            }
            buffer
        }
    };

    let (stdout_with_ansi, stderr_with_ansi) = match color {
        cli::Color::Always => (true, true),
        cli::Color::Never => (false, false),
        cli::Color::Auto => (
            std::io::stdout().is_terminal(),
            std::io::stderr().is_terminal(),
        ),
    };

    // TODO(mbolin): Take a more thoughtful approach to logging.
    let default_level = "error";
    let _ = tracing_subscriber::fmt()
        // Fallback to the `default_level` log filter if the environment
        // variable is not set _or_ contains an invalid value
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .or_else(|_| EnvFilter::try_new(default_level))
                .unwrap_or_else(|_| EnvFilter::new(default_level)),
        )
        .with_ansi(stderr_with_ansi)
        .with_writer(std::io::stderr)
        .try_init();

    let sandbox_mode = if full_auto {
        Some(SandboxMode::WorkspaceWrite)
    } else if dangerously_bypass_approvals_and_sandbox {
        Some(SandboxMode::DangerFullAccess)
    } else {
        sandbox_mode_cli_arg.map(Into::<SandboxMode>::into)
    };

    // When using `--oss`, let the bootstrapper pick the model (defaulting to
    // gpt-oss:20b) and ensure it is present locally. Also, force the built‑in
    // `oss` model provider.
    let model = if let Some(model) = model_cli_arg {
        Some(model)
    } else if oss {
        Some(DEFAULT_OSS_MODEL.to_owned())
    } else {
        None // No model specified, will use the default.
    };

    let model_provider = if oss {
        Some(BUILT_IN_OSS_MODEL_PROVIDER_ID.to_string())
    } else {
        None // No specific model provider override.
    };

    // Load configuration and determine approval policy
    let overrides = ConfigOverrides {
        model,
        config_profile,
        // This CLI is intended to be headless and has no affordances for asking
        // the user for approval.
        approval_policy: Some(AskForApproval::Never),
        sandbox_mode,
        cwd: cwd.map(|p| p.canonicalize().unwrap_or(p)),
        model_provider,
        codex_linux_sandbox_exe,
        base_instructions: None,
        include_plan_tool: Some(include_plan_tool),
        disable_response_storage: oss.then_some(true),
        show_raw_agent_reasoning: oss.then_some(true),
    };
    // Parse `-c` overrides.
    let cli_kv_overrides = match config_overrides.parse_overrides() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error parsing -c overrides: {e}");
            std::process::exit(1);
        }
    };

    let config = Config::load_with_cli_overrides(cli_kv_overrides, overrides)?;
    let mut event_processor: Box<dyn EventProcessor> = if json_mode {
        Box::new(EventProcessorWithJsonOutput::new(last_message_file.clone()))
    } else {
        Box::new(EventProcessorWithHumanOutput::create_with_ansi(
            stdout_with_ansi,
            &config,
            last_message_file.clone(),
        ))
    };

    if oss {
        codex_ollama::ensure_oss_ready(&config)
            .await
            .map_err(|e| anyhow::anyhow!("OSS setup failed: {e}"))?;
    }

    // Create plan emitter if any plan-related options are provided
    let plan_emitter = if plan_webhook.is_some()
        || plan_events.is_some()
        || plan_state.is_some()
        || emit_plan_stdout
    {
        // Generate default IDs if not provided
        let task_id = task_id.unwrap_or_else(|| format!("task-{}", uuid::Uuid::new_v4()));
        let run_id = run_id.unwrap_or_else(|| format!("run-{}", uuid::Uuid::new_v4()));

        Some(PlanEmitter::new(
            plan_webhook,
            webhook_secret,
            plan_events.clone(),
            plan_state.clone(),
            task_id,
            run_id,
            config.model.clone(),
            emit_plan_stdout,
            json_mode,
        )?)
    } else {
        None
    };

    // Print the effective configuration and prompt so users can see what Codex
    // is using.
    event_processor.print_config_summary(&config, &prompt);

    if !skip_git_repo_check && !is_inside_git_repo(&config.cwd.to_path_buf()) {
        eprintln!("Not inside a trusted directory and --skip-git-repo-check was not specified.");
        std::process::exit(1);
    }

    let conversation_manager = ConversationManager::default();
    let NewConversation {
        conversation_id: _,
        conversation,
        session_configured,
    } = conversation_manager.new_conversation(config).await?;
    info!("Codex initialized with event: {session_configured:?}");

    // Load initial sequence number if plan emitter is configured
    let sequence_number = if let Some(ref emitter) = plan_emitter {
        emitter.load_sequence().await.unwrap_or(1)
    } else {
        1
    };

    // Create parent directories for plan files if needed
    if let Some(ref path) = plan_events {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    if let Some(ref path) = plan_state {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    {
        let conversation = conversation.clone();
        let plan_emitter_clone = plan_emitter.clone();
        tokio::spawn(async move {
            let mut seq = sequence_number;
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        tracing::debug!("Keyboard interrupt (SIGINT)");
                        // Emit shutdown event if plan emitter is configured
                        if let Some(ref emitter) = plan_emitter_clone {
                            if let Err(e) = emitter.emit_shutdown(seq).await {
                                tracing::error!("Failed to emit shutdown event: {}", e);
                            }
                        }
                        // Immediately notify Codex to abort any in‑flight task.
                        conversation.submit(Op::Interrupt).await.ok();

                        // Exit the inner loop and return to the main input prompt. The codex
                        // will emit a `TurnInterrupted` (Error) event which is drained later.
                        break;
                    }
                    _ = async {
                        #[cfg(unix)]
                        {
                            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                                Ok(mut sigterm) => sigterm.recv().await,
                                Err(e) => {
                                    tracing::error!("Failed to install SIGTERM handler: {}", e);
                                    // Create a future that never resolves to avoid spurious termination behavior.
                                    std::future::pending::<Option<()>>().await
                                }
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            // On non-Unix systems, create a future that never resolves
                            std::future::pending::<()>().await
                        }
                    } => {
                        tracing::debug!("Termination signal (SIGTERM)");
                        // Emit shutdown event if plan emitter is configured
                        if let Some(ref emitter) = plan_emitter_clone {
                            if let Err(e) = emitter.emit_shutdown(seq).await {
                                tracing::error!("Failed to emit shutdown event: {}", e);
                            }
                        }
                        // Immediately notify Codex to abort any in‑flight task.
                        conversation.submit(Op::Interrupt).await.ok();

                        // Exit the inner loop and return to the main input prompt.
                        break;
                    }
                    res = conversation.next_event() => match res {
                        Ok(event) => {
                            tracing::debug!("Received event: {event:?}");

                            // Handle plan update events
                            if let EventMsg::PlanUpdate(ref plan_args) = event.msg {
                                if let Some(ref emitter) = plan_emitter_clone {
                                    match emitter.emit_plan_update(plan_args, seq).await {
                                        Ok(()) => {
                                            // Only persist and increment sequence on successful emission
                                            if let Err(e) = emitter.persist_sequence(seq).await {
                                                tracing::error!("Failed to persist sequence: {}", e);
                                            }
                                            seq += 1;
                                        }
                                        Err(e) => {
                                            tracing::error!("Failed to emit plan update (sequence {} not incremented): {}", seq, e);
                                            // Note: sequence is NOT incremented on failure to maintain contiguous delivery
                                        }
                                    }
                                }
                            }

                            let is_shutdown_complete = matches!(event.msg, EventMsg::ShutdownComplete);
                            if let Err(e) = tx.send(event) {
                                tracing::error!("Error sending event: {e:?}");
                                break;
                            }
                            if is_shutdown_complete {
                                info!("Received shutdown event, exiting event loop.");
                                break;
                            }
                        },
                        Err(e) => {
                            tracing::error!("Error receiving event: {e:?}");
                            break;
                        }
                    }
                }
            }
        });
    }

    // Send images first, if any.
    if !images.is_empty() {
        let items: Vec<InputItem> = images
            .into_iter()
            .map(|path| InputItem::LocalImage { path })
            .collect();
        let initial_images_event_id = conversation.submit(Op::UserInput { items }).await?;
        info!("Sent images with event ID: {initial_images_event_id}");
        while let Ok(event) = conversation.next_event().await {
            if event.id == initial_images_event_id
                && matches!(
                    event.msg,
                    EventMsg::TaskComplete(TaskCompleteEvent {
                        last_agent_message: _,
                    })
                )
            {
                break;
            }
        }
    }

    // Send the prompt.
    let items: Vec<InputItem> = vec![InputItem::Text { text: prompt }];
    let initial_prompt_task_id = conversation.submit(Op::UserInput { items }).await?;
    info!("Sent prompt with event ID: {initial_prompt_task_id}");

    // Run the loop until the task is complete.
    while let Some(event) = rx.recv().await {
        let shutdown: CodexStatus = event_processor.process_event(event);
        match shutdown {
            CodexStatus::Running => continue,
            CodexStatus::InitiateShutdown => {
                conversation.submit(Op::Shutdown).await?;
            }
            CodexStatus::Shutdown => {
                break;
            }
        }
    }

    Ok(())
}
