use clap::Parser;
use clap::ValueEnum;
use codex_common::CliConfigOverrides;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version)]
pub struct Cli {
    /// Optional image(s) to attach to the initial prompt.
    #[arg(long = "image", short = 'i', value_name = "FILE", value_delimiter = ',', num_args = 1..)]
    pub images: Vec<PathBuf>,

    /// Model the agent should use.
    #[arg(long, short = 'm')]
    pub model: Option<String>,

    #[arg(long = "oss", default_value_t = false)]
    pub oss: bool,

    /// Select the sandbox policy to use when executing model-generated shell
    /// commands.
    #[arg(long = "sandbox", short = 's', value_enum)]
    pub sandbox_mode: Option<codex_common::SandboxModeCliArg>,

    /// Configuration profile from config.toml to specify default options.
    #[arg(long = "profile", short = 'p')]
    pub config_profile: Option<String>,

    /// Convenience alias for low-friction sandboxed automatic execution (-a on-failure, --sandbox workspace-write).
    #[arg(long = "full-auto", default_value_t = false)]
    pub full_auto: bool,

    /// Skip all confirmation prompts and execute commands without sandboxing.
    /// EXTREMELY DANGEROUS. Intended solely for running in environments that are externally sandboxed.
    #[arg(
        long = "dangerously-bypass-approvals-and-sandbox",
        alias = "yolo",
        default_value_t = false,
        conflicts_with = "full_auto"
    )]
    pub dangerously_bypass_approvals_and_sandbox: bool,

    /// Tell the agent to use the specified directory as its working root.
    #[clap(long = "cd", short = 'C', value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Allow running Codex outside a Git repository.
    #[arg(long = "skip-git-repo-check", default_value_t = false)]
    pub skip_git_repo_check: bool,

    #[clap(skip)]
    pub config_overrides: CliConfigOverrides,

    /// Specifies color settings for use in the output.
    #[arg(long = "color", value_enum, default_value_t = Color::Auto)]
    pub color: Color,

    /// Print events to stdout as JSONL.
    #[arg(long = "json", default_value_t = false)]
    pub json: bool,

    /// Specifies file where the last message from the agent should be written.
    #[arg(long = "output-last-message")]
    pub last_message_file: Option<PathBuf>,

    /// Webhook endpoint for plan updates
    #[arg(long = "plan-webhook", env = "RAMUS_WEBHOOK_URL", value_parser = validate_url)]
    pub plan_webhook: Option<String>,

    /// HMAC signing secret for webhooks
    #[arg(long = "webhook-secret", env = "RAMUS_WEBHOOK_SECRET")]
    pub webhook_secret: Option<String>,

    /// Path for event log file (JSONL format)
    #[arg(long = "plan-events")]
    pub plan_events: Option<PathBuf>,

    /// Path for current plan state (JSON)
    #[arg(long = "plan-state")]
    pub plan_state: Option<PathBuf>,

    /// Unique task identifier
    #[arg(long = "task-id")]
    pub task_id: Option<String>,

    /// Unique run identifier  
    #[arg(long = "run-id")]
    pub run_id: Option<String>,

    /// Enable plan events to stdout
    #[arg(long = "emit-plan-stdout")]
    pub emit_plan_stdout: bool,

    /// Include the experimental plan tool so the model can call `update_plan`.
    #[arg(long = "include-plan-tool", default_value_t = false)]
    pub include_plan_tool: bool,

    /// Initial instructions for the agent. If not provided as an argument (or
    /// if `-` is used), instructions are read from stdin.
    #[arg(value_name = "PROMPT")]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum Color {
    Always,
    Never,
    #[default]
    Auto,
}

fn validate_url(s: &str) -> Result<String, String> {
    let url = url::Url::parse(s).map_err(|e| e.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("URL must use http or https scheme".to_string());
    }
    Ok(s.to_string())
}
