//! Named-pipe IPC server. One JSON request per line, one response per line.

use std::ffi::c_void;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aio_ipc::{LibraryItem, MAX_REQUEST_LEN, PIPE_NAME, PlayMode, Request, Response, Source, Status};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::device::{Command, Shared, lock};
use crate::media;

pub struct Daemon {
    pub commands: Sender<Command>,
    pub shared: Arc<Mutex<Shared>>,
    /// Held for the whole of a request that changes settings, which also
    /// serializes imports.
    pub config: tokio::sync::Mutex<Config>,
    pub config_path: PathBuf,
    pub cache_dir: PathBuf,
}

impl Daemon {
    fn send(&self, cmd: Command) -> Result<(), String> {
        self.commands.send(cmd).map_err(|_| "device thread has stopped".to_owned())
    }

    fn save(&self, config: &Config) -> Result<(), String> {
        config.save(&self.config_path).map_err(|e| format!("saving config: {e:#}"))
    }

    fn check_path(source: &Source) -> Result<(), String> {
        match source.path() {
            Some(path) if !path.is_absolute() => Err(format!("path must be absolute: {}", path.display())),
            _ => Ok(()),
        }
    }

    /// Imports `source` (or reuses its cache) off the async thread.
    async fn import(&self, source: &Source) -> Result<media::CachedMedia, String> {
        let (src, cache) = (source.clone(), self.cache_dir.clone());
        tokio::task::spawn_blocking(move || media::import(&src, &cache))
            .await
            .map_err(|e| format!("import task failed: {e}"))?
            .map_err(|e| format!("{e:#}"))
    }

    /// Deletes cache entries that are neither current nor in the library.
    fn prune(&self, config: &Config) {
        let (cache, keep) = (self.cache_dir.clone(), config.keep_keys());
        tokio::task::spawn_blocking(move || media::prune(&cache, &keep));
    }

    /// Puts `media` on the display and makes `source` the selected source.
    fn show(&self, config: &mut Config, source: Source, media: media::CachedMedia) -> Result<(), String> {
        let key = media.key().to_owned();
        self.send(Command::SetMedia(media))?;
        info!(?source, "source set");
        config.source = Some(source);
        config.cache_key = Some(key);
        self.save(config)?;
        self.prune(config);
        Ok(())
    }

    async fn set_source(&self, source: Source) -> Result<(), String> {
        Self::check_path(&source)?;
        let mut config = self.config.lock().await;
        let media = self.import(&source).await?;
        self.show(&mut config, source, media)
    }

    async fn library_add(&self, source: Source) -> Result<(), String> {
        Self::check_path(&source)?;
        let mut config = self.config.lock().await;
        let media = self.import(&source).await?;
        let id = config.add_item(source.clone(), media.key().to_owned())?;
        info!(id, ?source, "added to library");
        self.save(&config)
    }

    async fn library_remove(&self, id: u64) -> Result<(), String> {
        let mut config = self.config.lock().await;
        let entry = config.remove_item(id)?;
        info!(id, source = ?entry.source, "removed from library");
        self.save(&config)?;
        self.prune(&config);
        Ok(())
    }

    async fn library_show(&self, id: u64) -> Result<(), String> {
        let mut config = self.config.lock().await;
        let entry = config.item(id).cloned().ok_or_else(|| format!("no library item #{id}"))?;
        // Normally cached since it was added; re-import if the cache is gone.
        let (cache, key) = (self.cache_dir.clone(), entry.cache_key.clone());
        let cached = tokio::task::spawn_blocking(move || media::open_cached(&cache, &key).ok())
            .await
            .map_err(|e| format!("cache task failed: {e}"))?;
        let media = match cached {
            Some(m) => m,
            None => {
                let m = self.import(&entry.source).await?;
                if let Some(e) = config.library.iter_mut().find(|e| e.id == id) {
                    e.cache_key = m.key().to_owned(); // the file changed since it was added
                }
                m
            }
        };
        self.show(&mut config, entry.source, media)
    }

    async fn set_play_mode(&self, mode: PlayMode) -> Result<(), String> {
        let mut config = self.config.lock().await;
        config.play_mode = mode;
        info!(?mode, "play mode set");
        self.save(&config)
    }

    async fn set_paused(&self, paused: bool) -> Result<(), String> {
        let mut config = self.config.lock().await;
        self.send(if paused { Command::Pause } else { Command::Resume })?;
        config.paused = paused;
        self.save(&config)
    }

    async fn set_brightness(&self, value: u8) -> Result<(), String> {
        if value > 100 {
            return Err(format!("brightness must be 0-100, got {value}"));
        }
        let mut config = self.config.lock().await;
        self.send(Command::SetBrightness(value))?;
        config.brightness = value;
        self.save(&config)
    }

    async fn set_rotation(&self, degrees: u16) -> Result<(), String> {
        let rotation = aio_proto::hid::Rotation::from_degrees(degrees)
            .ok_or_else(|| format!("rotation must be 0, 90, 180 or 270, got {degrees}"))?;
        let mut config = self.config.lock().await;
        self.send(Command::SetRotation(rotation))?;
        config.rotation = Some(degrees);
        self.save(&config)
    }

    async fn status(&self) -> Status {
        let (source, brightness, rotation, library, play_mode, current_item) = {
            let c = self.config.lock().await;
            let library = c
                .library
                .iter()
                .map(|e| LibraryItem {
                    id: e.id,
                    source: e.source.clone(),
                    available: media::is_cached(&self.cache_dir, &e.cache_key) || e.source.path().is_some_and(|p| p.is_file()),
                })
                .collect();
            (c.source.clone(), c.brightness, c.rotation, library, c.play_mode, c.current_item())
        };
        let s = lock(&self.shared);
        Status {
            device: s.device,
            source,
            paused: s.paused,
            fps: s.fps(Instant::now()),
            frames_sent: s.frames_sent,
            last_error: s.last_error.clone(),
            brightness,
            rotation,
            firmware: s.firmware.clone(),
            library,
            play_mode,
            current_item,
        }
    }

    async fn handle(&self, req: Request) -> Response {
        let result = match req {
            Request::SetSource { source } => self.set_source(source).await,
            Request::Pause => self.set_paused(true).await,
            Request::Resume => self.set_paused(false).await,
            Request::SetBrightness { value } => self.set_brightness(value).await,
            Request::SetRotation { degrees } => self.set_rotation(degrees).await,
            Request::LibraryAdd { source } => self.library_add(source).await,
            Request::LibraryRemove { id } => self.library_remove(id).await,
            Request::LibraryShow { id } => self.library_show(id).await,
            Request::SetPlayMode { mode } => self.set_play_mode(mode).await,
            Request::GetStatus => return Response::Status(self.status().await),
            Request::GetPreview => {
                let jpeg = lock(&self.shared).preview.clone();
                if jpeg.is_empty() {
                    Err("nothing has been shown yet".to_owned())
                } else {
                    return Response::Preview { jpeg };
                }
            }
        };
        match result {
            Ok(()) => Response::Ok,
            Err(message) => Response::Error { message },
        }
    }
}

/// Access for SYSTEM, administrators and interactively logged-on users only
/// (`P` blocks inherited entries).
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

/// A security descriptor parsed once and kept for the daemon's lifetime.
struct PipeSecurity(*mut c_void);

// SAFETY: the descriptor is immutable after creation and never freed.
unsafe impl Send for PipeSecurity {}
unsafe impl Sync for PipeSecurity {}

impl PipeSecurity {
    fn new() -> io::Result<Self> {
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
        };
        let sddl: Vec<u16> = PIPE_SDDL.encode_utf16().chain([0]).collect();
        let mut sd = std::ptr::null_mut();
        // SAFETY: valid NUL-terminated string and out pointer.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(sd))
    }

    fn create(&self, first: bool) -> io::Result<NamedPipeServer> {
        use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        };
        let mut options = ServerOptions::new();
        // `first`: fail if another daemon (or anything else) already owns the name.
        options.first_pipe_instance(first).reject_remote_clients(true);
        // SAFETY: attrs points at a valid SECURITY_ATTRIBUTES for this call.
        unsafe {
            options.create_with_security_attributes_raw(PIPE_NAME, &mut attrs as *mut _ as *mut c_void)
        }
    }
}

/// Accepts clients forever.
pub async fn serve(daemon: Arc<Daemon>) -> io::Result<()> {
    let security = PipeSecurity::new()?;
    let mut server = security.create(true).map_err(|e| {
        io::Error::new(e.kind(), format!("cannot create {PIPE_NAME} (is a daemon already running?): {e}"))
    })?;
    info!("listening on {PIPE_NAME}");
    loop {
        server.connect().await?;
        let client = std::mem::replace(&mut server, security.create(false)?);
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(client, &daemon).await {
                debug!("client error: {e}");
            }
        });
    }
}

async fn handle_client(pipe: NamedPipeServer, daemon: &Daemon) -> io::Result<()> {
    let (read, mut write) = tokio::io::split(pipe);
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    loop {
        line.clear();
        let n = (&mut reader).take(MAX_REQUEST_LEN as u64 + 1).read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        if n > MAX_REQUEST_LEN {
            let resp = Response::Error { message: "request too long".into() };
            write.write_all(&aio_ipc::encode_line(&resp)).await?;
            return Ok(());
        }
        let resp = match aio_ipc::decode_line::<Request>(&line) {
            Ok(req) => {
                debug!(?req, "request");
                daemon.handle(req).await
            }
            Err(e) => {
                warn!("invalid request: {e}");
                Response::Error { message: format!("invalid request: {e}") }
            }
        };
        write.write_all(&aio_ipc::encode_line(&resp)).await?;
    }
}
