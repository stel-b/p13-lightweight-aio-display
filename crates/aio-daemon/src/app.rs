//! Daemon startup and shutdown, shared by the foreground and service modes.

use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::config::Config;
use crate::device::{Command, DeviceSettings, Shared, Timing, UsbConnector, Worker};
use crate::media::{self, CachedMedia};
use crate::server::{self, Daemon};
use crate::{notify, paths};

/// The cached frames of the configured source: by cache key when possible
/// (fast, works even if the file was moved), else by importing it again.
fn restore(config: &Config, cache_dir: &Path) -> Option<CachedMedia> {
    let source = config.source.as_ref()?;
    if let Some(media) = config.cache_key.as_deref().and_then(|k| media::open_cached(cache_dir, k).ok()) {
        return Some(media);
    }
    match media::import(source, cache_dir) {
        Ok(media) => Some(media),
        Err(e) => {
            warn!(?source, "cannot restore source: {e:#}");
            None
        }
    }
}

/// Runs until `shutdown` completes, the IPC server fails, or the device
/// thread dies (an error, so a service manager restarts us).
pub async fn run(shutdown: impl Future<Output = ()>) -> Result<()> {
    let config_path = paths::config_file();
    let cache_dir = paths::cache_dir();
    let mut config = Config::load(&config_path).unwrap_or_else(|e| {
        warn!("{e:#}; starting with defaults");
        Config::default()
    });

    let media = restore(&config, &cache_dir);
    if let Some(m) = &media {
        info!(source = ?config.source, frames = m.frames().len(), paused = config.paused, "restored");
        media::prune(&cache_dir, m.key());
        config.cache_key = Some(m.key().to_owned());
    }

    let (tx, rx) = mpsc::channel();
    let notify = match notify::spawn(tx.clone()) {
        Ok(target) => Some(target),
        Err(e) => {
            warn!("device notifications unavailable ({e:#}); relying on retries");
            None
        }
    };

    let shared = Arc::new(Mutex::new(Shared::new(config.paused)));
    let connector = UsbConnector::new(paths::device_key_file(), paths::handshake_file(), notify);
    let settings = DeviceSettings {
        brightness: config.brightness.min(100),
        rotation: config.rotation.and_then(aio_proto::hid::Rotation::from_degrees),
    };
    let worker = Worker::new(connector, rx, shared.clone(), media, config.paused, settings, Timing::default());
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    let device_thread = std::thread::Builder::new().name("device".into()).spawn(move || {
        worker.run();
        let _ = done_tx.send(()); // dropped unsent on panic: also wakes the receiver
    })?;

    let daemon = Arc::new(Daemon {
        commands: tx.clone(),
        shared,
        config: tokio::sync::Mutex::new(config),
        config_path,
        cache_dir,
    });

    let result = tokio::select! {
        r = server::serve(daemon) => r.context("IPC server"),
        _ = done_rx => Err(anyhow::anyhow!("device thread stopped unexpectedly")),
        () = shutdown => {
            info!("shutting down");
            Ok(())
        }
    };

    let _ = tx.send(Command::Shutdown);
    let _ = device_thread.join();
    result
}
