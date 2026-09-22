use anyhow::Result;
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use claude_code_proxy::{
    config, logging,
    monitor::MonitorHandle,
    paths,
    registry::{ANTHROPIC_STYLE_ALIASES, Registry},
    server::{self, ServerConfig},
    tui::{self, MonitorExit, MonitorUiConfig},
};
use std::io::IsTerminal;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Parser)]
#[command(name = "claude-code-proxy", version = VERSION, about = "Anthropic-compatible proxy for Claude Code provider backends", disable_version_flag = true)]
struct Cli {
    #[arg(long = "version", short = 'v', action = ArgAction::SetTrue)]
    version_flag: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Print version information
    Version,
    /// Start the proxy server and monitor
    Serve {
        #[arg(long)]
        port: Option<u16>,
        #[arg(long = "no-monitor", action = ArgAction::SetTrue)]
        no_monitor: bool,
    },
    /// Attach a read-only dashboard to a running proxy
    Monitor {
        #[arg(long)]
        url: Option<reqwest::Url>,
    },
    /// Open the monitor TUI with mock data and no proxy server
    #[command(hide = true)]
    Demo,
    /// List supported provider models
    Models {
        #[arg(long)]
        full: bool,
    },
    /// Manage Codex authentication
    Codex {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    /// Manage Kimi authentication
    Kimi {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    /// Manage Cursor authentication
    Cursor {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    /// Manage Grok authentication
    Grok {
        #[command(subcommand)]
        command: ProviderGroup,
    },
    /// Manage GitHub Copilot authentication, credential copy, and live models
    #[command(name = "github-copilot", alias = "copilot")]
    GithubCopilot {
        #[command(subcommand)]
        command: GithubCopilotGroup,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderGroup {
    Auth {
        #[command(subcommand)]
        command: claude_code_proxy::provider::AuthCommand,
    },
}

#[derive(Debug, Clone, ValueEnum)]
enum CopilotCopySource {
    Vscode,
    Opencode,
}
impl CopilotCopySource {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Vscode => "vscode",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Debug, Subcommand)]
enum GithubCopilotGroup {
    /// Authenticate directly with GitHub device flow
    Auth {
        #[command(subcommand)]
        command: claude_code_proxy::provider::AuthCommand,
    },
    /// Copy existing GitHub Copilot credentials from VS Code or OpenCode
    #[command(alias = "import")]
    Copy {
        #[arg(value_enum)]
        source: CopilotCopySource,
    },
    /// Query the live GitHub Copilot model catalog
    Models,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.version_flag {
        println!("claude-code-proxy {}", VERSION);
        return Ok(());
    }
    let command = cli.command.unwrap_or(Commands::Serve {
        port: None,
        no_monitor: false,
    });
    match command {
        Commands::Version => {
            println!("claude-code-proxy {}", VERSION);
            Ok(())
        }

        Commands::Serve { port, no_monitor } => {
            let bind_address = config::bind_address();
            let effective_port = port.unwrap_or_else(config::port);
            let registry = Registry::with_default_alias();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            match select_serve_mode(std::io::stdout().is_terminal(), no_monitor) {
                ServeMode::Plain => {
                    print_server_banner(&bind_address, effective_port, &registry);
                    runtime
                        .block_on(run_service(ServerConfig {
                            bind_address,
                            port: effective_port,
                            monitor: Some(MonitorHandle::default()),
                        }))
                        .map_err(|err| anyhow::anyhow!(err))
                }
                ServeMode::Monitor => {
                    let _stderr_guard = logging::suppress_stderr();
                    let monitor = MonitorHandle::default();
                    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
                    let (shutdown_complete_tx, shutdown_complete_rx) = std::sync::mpsc::channel();
                    let listener = runtime
                        .block_on(server::bind_proxy_listener(&bind_address, effective_port))?;
                    let local_addr = listener.local_addr()?;
                    let monitor_listen_url =
                        listen_url(&local_addr.ip().to_string(), local_addr.port());
                    let server_monitor = monitor.clone();
                    let server_task = runtime.spawn(async move {
                        let result =
                            server::serve_listener(listener, Some(server_monitor), async move {
                                let _ = shutdown_rx.await;
                            })
                            .await;
                        let _ = shutdown_complete_tx.send(());
                        result
                    });
                    let ui_result = tui::run_monitor(
                        monitor,
                        MonitorUiConfig {
                            listen_url: monitor_listen_url,
                            port: effective_port,
                            registry: &registry,
                            shutdown: Some(shutdown_tx),
                            shutdown_complete: Some(shutdown_complete_rx),
                        },
                    );
                    if matches!(&ui_result, Ok(MonitorExit::ForceQuit)) {
                        server_task.abort();
                        let _ = runtime.block_on(server_task);
                        std::process::exit(130);
                    }
                    let server_result = runtime.block_on(server_task)?;
                    ui_result?;
                    server_result.map_err(|err| anyhow::anyhow!(err))
                }
            }
        }

        Commands::Demo => {
            let registry = Registry::with_default_alias();
            tui::run_mock_monitor(config::port(), &registry)
        }
        Commands::Monitor { url } => {
            let url = url.unwrap_or_else(|| {
                format!("http://127.0.0.1:{}", config::port())
                    .parse()
                    .expect("local proxy URL")
            });
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(2))
                .build()?;
            let monitor = runtime.block_on(
                claude_code_proxy::monitor::remote::RemoteMonitor::connect(client, url.clone()),
            )?;
            tui::run_attached_monitor(|| monitor.snapshot(), url.to_string())?;
            Ok(())
        }
        Commands::Models { full } => {
            print_models(&Registry::with_default_alias(), full);
            Ok(())
        }
        Commands::Codex { command } => run_provider_cli("codex", command),
        Commands::Kimi { command } => run_provider_cli("kimi", command),
        Commands::Cursor { command } => run_provider_cli("cursor", command),
        Commands::Grok { command } => run_provider_cli("grok", command),
        Commands::GithubCopilot { command } => run_github_copilot_cli(command),
    }
}

async fn run_service(config: ServerConfig) -> Result<()> {
    let mut signals = ServiceShutdownSignals::new()?;
    let (shutdown, stopped) = tokio::sync::oneshot::channel();
    let server = server::serve_with_shutdown(config, async {
        let _ = stopped.await;
    });
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        signal = signals.recv() => {
            signal?;
            let _ = shutdown.send(());
            tokio::select! {
                result = &mut server => result,
                signal = signals.recv() => {
                    signal?;
                    std::process::exit(130);
                }
            }
        }
    }
}

#[cfg(unix)]
struct ServiceShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ServiceShutdownSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        })
    }

    async fn recv(&mut self) -> std::io::Result<()> {
        tokio::select! {
            _ = self.interrupt.recv() => Ok(()),
            _ = self.terminate.recv() => Ok(()),
        }
    }
}

#[cfg(windows)]
struct ServiceShutdownSignals {
    ctrl_c: tokio::signal::windows::CtrlC,
}

#[cfg(windows)]
impl ServiceShutdownSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            ctrl_c: tokio::signal::windows::ctrl_c()?,
        })
    }

    async fn recv(&mut self) -> std::io::Result<()> {
        let _ = self.ctrl_c.recv().await;
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
struct ServiceShutdownSignals;

#[cfg(not(any(unix, windows)))]
impl ServiceShutdownSignals {
    fn new() -> std::io::Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) -> std::io::Result<()> {
        tokio::signal::ctrl_c().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServeMode {
    Monitor,
    Plain,
}
fn select_serve_mode(stdout_is_tty: bool, no_monitor: bool) -> ServeMode {
    if stdout_is_tty && !no_monitor {
        ServeMode::Monitor
    } else {
        ServeMode::Plain
    }
}

fn run_provider_cli(name: &str, command: ProviderGroup) -> Result<()> {
    let registry = Registry::with_default_alias();
    let provider = registry
        .provider(name)
        .ok_or_else(|| anyhow::anyhow!("unknown provider: {name}"))?;
    let ProviderGroup::Auth { command } = command;
    run_auth_command(provider.cli(), command)
}

fn run_github_copilot_cli(command: GithubCopilotGroup) -> Result<()> {
    match command {
        GithubCopilotGroup::Auth { command } => {
            let registry = Registry::with_default_alias();
            let provider = registry
                .provider("copilot")
                .or_else(|| registry.provider("github-copilot"))
                .ok_or_else(|| anyhow::anyhow!("Copilot provider is unavailable"))?;
            run_auth_command(provider.cli(), command)
        }
        GithubCopilotGroup::Copy { source } => {
            claude_code_proxy::providers::github_copilot::import_from(source.as_str())
        }
        GithubCopilotGroup::Models => {
            for model in claude_code_proxy::providers::github_copilot::discover_models()? {
                println!("{model}");
            }
            Ok(())
        }
    }
}

fn run_auth_command(
    handlers: &'static dyn claude_code_proxy::provider::CliHandlers,
    command: claude_code_proxy::provider::AuthCommand,
) -> Result<()> {
    use claude_code_proxy::provider::AuthCommand;
    match command {
        AuthCommand::Login => {
            if let Err(e) = handlers.login() {
                eprintln!("{e}");
                std::process::exit(2);
            }
            Ok(())
        }
        AuthCommand::Device => {
            if let Err(e) = handlers.device() {
                eprintln!("{e}");
                std::process::exit(2);
            }
            Ok(())
        }
        AuthCommand::Status => {
            if let Err(e) = handlers.status() {
                println!("{e}");
                if e.to_string() == "Not authenticated" {
                    std::process::exit(1);
                }
                std::process::exit(2);
            }
            Ok(())
        }
        AuthCommand::Logout => {
            handlers.logout()?;
            Ok(())
        }
    }
}

fn print_models(registry: &Registry, full: bool) {
    let grouped = registry.grouped_models();
    for provider in [
        "codex",
        "copilot",
        "github-copilot",
        "kimi",
        "grok",
        "opencode",
        "cursor",
    ] {
        let Some(models) = grouped.get(provider) else {
            continue;
        };
        if full || provider != "cursor" {
            println!("{provider}: {}", models.join(", "));
        } else {
            println!("{provider}: {}", compact_cursor_list(models));
        }
    }
}

fn compact_cursor_list(models: &[String]) -> String {
    let (legacy, dynamic): (Vec<_>, Vec<_>) =
        models.iter().cloned().partition(|m| !m.contains(':'));
    let mut out = String::new();
    if !legacy.is_empty() {
        out.push_str(&legacy.join(", "));
        out.push_str("; ");
    }
    out.push_str(&format!("{} cursor model aliases", dynamic.len()));
    if !dynamic.is_empty() {
        out.push_str(", example: cursor:gpt-5.5");
    }
    out.push_str(" run `claude-code-proxy models --full` for all aliases");
    out
}

fn listen_url(bind_address: &str, port: u16) -> String {
    match bind_address.parse::<std::net::IpAddr>() {
        Ok(ip) => format!("http://{}", std::net::SocketAddr::new(ip, port)),
        Err(_) => format!("http://{bind_address}:{port}"),
    }
}

fn print_server_banner(bind_address: &str, port: u16, registry: &Registry) {
    println!("Proxy listening on {}", listen_url(bind_address, port));
    println!("Logs: {}", paths::log_file().display());
    let cfg = paths::config_dir();
    if cfg.exists() {
        println!("Config: {}", cfg.display());
    }
    print_models(registry, false);
    println!();
    println!("Configure Claude Code (pick a model from above):");
    println!("  export ANTHROPIC_BASE_URL=\"http://localhost:{port}\"");
    println!("  export ANTHROPIC_AUTH_TOKEN=\"anything\"");
    println!("  export ANTHROPIC_MODEL=\"github-copilot:gpt-5.6-sol\"");
    println!("  export ANTHROPIC_SMALL_FAST_MODEL=\"github-copilot:gpt-5.6-luna\"");
    println!("  export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1");
}

#[allow(dead_code)]
fn alias_names() -> usize {
    ANTHROPIC_STYLE_ALIASES.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_serve_selects_monitor_on_tty() {
        assert_eq!(select_serve_mode(true, false), ServeMode::Monitor);
    }
    #[test]
    fn no_monitor_selects_plain_mode() {
        assert_eq!(select_serve_mode(true, true), ServeMode::Plain);
    }
    #[test]
    fn non_tty_stdout_selects_plain_mode() {
        assert_eq!(select_serve_mode(false, false), ServeMode::Plain);
    }
    #[test]
    fn demo_command_parses_without_server_options() {
        let cli = Cli::try_parse_from(["claude-code-proxy", "demo"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Demo)));
    }

    #[tokio::test]
    async fn shutdown_signal_setup_and_receive_preserve_io_results() {
        fn assert_constructor(_: fn() -> std::io::Result<ServiceShutdownSignals>) {}
        fn assert_io_future<F: std::future::Future<Output = std::io::Result<()>>>(_: &F) {}

        assert_constructor(ServiceShutdownSignals::new);
        let mut signals = ServiceShutdownSignals::new().unwrap();
        let receive = signals.recv();
        assert_io_future(&receive);
    }

    #[test]
    fn github_copilot_copy_cli_parses() {
        let cli = Cli::try_parse_from(["claude-code-proxy", "github-copilot", "copy", "opencode"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::GithubCopilot {
                command: GithubCopilotGroup::Copy {
                    source: CopilotCopySource::Opencode
                }
            })
        ));
        let cli_copilot =
            Cli::try_parse_from(["claude-code-proxy", "copilot", "copy", "opencode"]).unwrap();
        assert!(matches!(
            cli_copilot.command,
            Some(Commands::GithubCopilot {
                command: GithubCopilotGroup::Copy {
                    source: CopilotCopySource::Opencode
                }
            })
        ));
    }
    #[test]
    fn github_copilot_import_alias_parses() {
        let cli = Cli::try_parse_from(["claude-code-proxy", "github-copilot", "import", "vscode"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::GithubCopilot {
                command: GithubCopilotGroup::Copy {
                    source: CopilotCopySource::Vscode
                }
            })
        ));
    }
    #[test]
    fn github_copilot_models_cli_parses() {
        let cli = Cli::try_parse_from(["claude-code-proxy", "github-copilot", "models"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::GithubCopilot {
                command: GithubCopilotGroup::Models
            })
        ));
    }

    #[test]
    fn listen_url_brackets_ipv6_addresses() {
        assert_eq!(listen_url("::1", 18765), "http://[::1]:18765");
    }
}
