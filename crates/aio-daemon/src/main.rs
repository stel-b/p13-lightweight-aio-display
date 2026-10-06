//! aio-daemon: keeps the P13 display showing the configured source and
//! serves IPC on `\\.\pipe\aio-ui`.
//!
//!   aio-daemon              run in the foreground (Ctrl+C to stop)
//!   aio-daemon --service    run under the service manager
//!
//! The service is installed by the installer (or `scripts/install.ps1`).

use aio_daemon::{app, paths, service};
use anyhow::Result;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: aio-daemon [--service]
  (no argument)  run in the foreground until Ctrl+C
  --service      run as a Windows service (used by the service manager;
                 installed by the AIO Display installer)";

fn filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())
}

/// Panics end up in the log instead of vanishing with the service's stderr.
fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default(info);
    }));
}

fn main() -> Result<()> {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some(service::SERVICE_ARG) => {
            std::fs::create_dir_all(paths::log_dir())?;
            let appender = tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("aio-daemon")
                .filename_suffix("log")
                .max_log_files(7)
                .build(paths::log_dir())?;
            tracing_subscriber::fmt().with_env_filter(filter()).with_ansi(false).with_writer(appender).init();
            log_panics();
            tracing::info!(version = env!("CARGO_PKG_VERSION"), "service starting");
            service::run_dispatcher()
        }
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => anyhow::bail!("unknown command {other:?}\n{USAGE}"),
        None => {
            tracing_subscriber::fmt().with_env_filter(filter()).init();
            log_panics();
            tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(app::run(async {
                let _ = tokio::signal::ctrl_c().await;
            }))
        }
    }
}
