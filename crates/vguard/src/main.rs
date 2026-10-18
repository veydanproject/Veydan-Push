//! `vguard`: the read policy in front of a relay (see the library).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use vguard::config::{self, Config};
use vguard::proxy::{self, Shared};

#[derive(Parser)]
#[command(name = "vguard", about = "Veydan relay guard: the read policy in front of strfry", version = vguard::VERSION)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct ConfigArg {
    /// Config file.
    #[arg(long, env = "VGUARD_CONFIG", default_value = config::DEFAULT_PATH)]
    config: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy.
    Serve(ConfigArg),
    /// Read the config and report every problem in it.
    CheckConfig(ConfigArg),
    /// Ask the running proxy one public question through to strfry: the
    /// health check of the kit.
    Probe {
        #[command(flatten)]
        config: ConfigArg,
        /// Seconds to wait for the answer.
        #[arg(long, default_value_t = 5)]
        wait: u64,
    },
}

fn load(arg: &ConfigArg) -> Result<Config, ExitCode> {
    Config::load(&arg.config).map_err(|e| {
        eprintln!("vguard: {e}");
        ExitCode::from(2)
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime");
    match cli.command {
        Command::CheckConfig(arg) => match load(&arg) {
            Ok(_) => {
                println!("{}: ok", arg.config.display());
                ExitCode::SUCCESS
            }
            Err(code) => code,
        },
        Command::Serve(arg) => {
            let config = match load(&arg) {
                Ok(c) => c,
                Err(code) => return code,
            };
            let level: tracing::Level = config.log.level.parse().unwrap_or(tracing::Level::INFO);
            tracing_subscriber::fmt()
                .with_max_level(level)
                .with_ansi(false)
                .with_target(false)
                .init();
            runtime.block_on(serve(config))
        }
        Command::Probe { config, wait } => {
            let config = match load(&config) {
                Ok(c) => c,
                Err(code) => return code,
            };
            runtime.block_on(probe(config, Duration::from_secs(wait)))
        }
    }
}

async fn serve(config: Config) -> ExitCode {
    let listener = match tokio::net::TcpListener::bind(config.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vguard: listen on {}: {e}", config.listen);
            return ExitCode::from(1);
        }
    };
    tracing::info!(
        "vguard {} on {}, strfry at {}, relay {}",
        vguard::VERSION,
        config.listen,
        config.upstream,
        config.relay_hosts.join(" ")
    );
    let every = config.log.metrics_secs;
    let shared = Shared::new(config);
    if every > 0 {
        tokio::spawn(proxy::report(shared.clone(), Duration::from_secs(every)));
    }
    tokio::spawn(proxy::watch_denied(shared.clone()));
    tokio::spawn(proxy::watch_denied_groups(shared.clone()));
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("vguard: signals: {e}");
            return ExitCode::from(1);
        }
    };
    tokio::select! {
        _ = proxy::run(listener, shared.clone()) => {}
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    tracing::info!("vguard stops: {}", shared.metrics.line());
    ExitCode::SUCCESS
}

/// `["REQ","vguard-probe",{"kinds":[0],"limit":0}]` until its EOSE.
async fn probe(config: Config, wait: Duration) -> ExitCode {
    let url = format!("ws://{}/", config.listen);
    let asked = async {
        let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .map_err(|e| e.to_string())?;
        ws.send(Message::text(
            r#"["REQ","vguard-probe",{"kinds":[0],"limit":0}]"#,
        ))
        .await
        .map_err(|e| e.to_string())?;
        while let Some(m) = ws.next().await {
            if let Message::Text(t) = m.map_err(|e| e.to_string())? {
                let v: serde_json::Value = serde_json::from_str(t.as_str()).unwrap_or_default();
                if v[1] == "vguard-probe" {
                    let _ = ws.close(None).await;
                    return match v[0].as_str() {
                        Some("EOSE") => Ok(()),
                        _ => Err(format!("the relay answered {t}")),
                    };
                }
            }
        }
        Err("the connection closed without an answer".to_string())
    };
    match tokio::time::timeout(wait, asked).await {
        Ok(Ok(())) => {
            println!("vguard on {}: strfry answers", config.listen);
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => {
            eprintln!("vguard on {}: {e}", config.listen);
            ExitCode::from(1)
        }
        Err(_) => {
            eprintln!(
                "vguard on {}: no answer in {} s",
                config.listen,
                wait.as_secs()
            );
            ExitCode::from(1)
        }
    }
}
