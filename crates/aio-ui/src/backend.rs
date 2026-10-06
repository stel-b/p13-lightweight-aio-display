//! Talks to the daemon on two background threads, so the window never blocks:
//! a poller (status and preview) and a commander (requests that may take a
//! while, like importing a video). Each keeps its own pipe connection.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aio_ipc::{Client, Request, Response, Source, Status};
use egui::ColorImage;

/// How often status is polled.
const STATUS_INTERVAL: Duration = Duration::from_millis(700);
/// Preview refresh while something animates (~20 fps); otherwise it follows
/// the status interval.
const PREVIEW_INTERVAL: Duration = Duration::from_millis(50);
/// Retry interval while the daemon can't be reached.
const RETRY_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub enum Action {
    SetSource(Source),
    Pause,
    Resume,
    Brightness(u8),
    Rotation(u16),
}

impl Action {
    fn request(&self) -> Request {
        match self {
            Action::SetSource(source) => Request::SetSource { source: source.clone() },
            Action::Pause => Request::Pause,
            Action::Resume => Request::Resume,
            Action::Brightness(value) => Request::SetBrightness { value: *value },
            Action::Rotation(degrees) => Request::SetRotation { degrees: *degrees },
        }
    }

    fn busy_text(&self) -> &'static str {
        match self {
            Action::SetSource(Source::Video { .. }) => "Importing video\u{2026} (this can take a while)",
            Action::SetSource(_) => "Importing\u{2026}",
            _ => "Applying\u{2026}",
        }
    }
}

/// What the window shows. Updated by the threads.
#[derive(Default)]
pub struct State {
    /// `None` until the first poll; `Err` when the daemon can't be reached.
    pub status: Option<Result<Status, String>>,
    pub preview: Option<ColorImage>,
    /// Bumped whenever `preview` changes, so the window re-uploads it.
    pub preview_generation: u64,
    /// Set while a request runs.
    pub busy: Option<&'static str>,
    /// Outcome of the last request: `Ok(())` or the daemon's error message.
    pub last_result: Option<Result<(), String>>,
}

pub struct Backend {
    state: Arc<Mutex<State>>,
    actions: Sender<Action>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

impl Backend {
    pub fn start(ctx: egui::Context) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let (actions, rx) = mpsc::channel();
        let (poke_tx, poke_rx) = mpsc::channel();
        {
            let (state, ctx) = (state.clone(), ctx.clone());
            std::thread::Builder::new()
                .name("ipc-poll".into())
                .spawn(move || poll_loop(&state, &ctx, &poke_rx))
                .expect("spawn poller");
        }
        {
            let state = state.clone();
            std::thread::Builder::new()
                .name("ipc-command".into())
                .spawn(move || command_loop(&state, &ctx, &rx, &poke_tx))
                .expect("spawn commander");
        }
        Self { state, actions }
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    pub fn send(&self, action: Action) {
        let _ = self.actions.send(action);
    }
}

fn connect(client: &mut Option<Client>) -> Result<&mut Client, String> {
    if client.is_none() {
        *client = Some(Client::connect().map_err(|e| e.to_string())?);
    }
    Ok(client.as_mut().expect("just connected"))
}

/// One request, reconnecting once if the pipe broke (e.g. daemon restarted).
fn request(client: &mut Option<Client>, req: &Request) -> Result<Response, String> {
    for attempt in 0..2 {
        match connect(client)?.request(req) {
            Ok(r) => return Ok(r),
            Err(e) => {
                *client = None;
                if attempt == 1 {
                    return Err(e.to_string());
                }
            }
        }
    }
    unreachable!()
}

fn decode_preview(jpeg: &[u8]) -> Option<ColorImage> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()?.to_rgb8();
    let (w, h) = img.dimensions();
    Some(ColorImage::from_rgb([w as usize, h as usize], img.as_raw()))
}

fn poll_loop(state: &Mutex<State>, ctx: &egui::Context, poke: &Receiver<()>) {
    let mut client = None;
    let mut last_preview: Option<Vec<u8>> = None;
    let mut next_status = Instant::now();
    let (mut connected, mut animating) = (false, false);
    loop {
        let now = Instant::now();
        if now >= next_status {
            let status = request(&mut client, &Request::GetStatus).and_then(|r| match r {
                Response::Status(s) => Ok(s),
                Response::Error { message } => Err(message),
                other => Err(format!("unexpected answer {other:?}")),
            });
            connected = status.is_ok();
            animating = status.as_ref().is_ok_and(|s| s.fps > 0.5);
            lock(state).status = Some(status);
            next_status = now + if connected { STATUS_INTERVAL } else { RETRY_INTERVAL };
            ctx.request_repaint();
        }

        // The daemon returns the frame on screen; only decode when it changed.
        if connected
            && let Ok(Response::Preview { jpeg }) = request(&mut client, &Request::GetPreview)
            && last_preview.as_deref() != Some(&jpeg)
        {
            let image = decode_preview(&jpeg);
            let mut s = lock(state);
            s.preview = image;
            s.preview_generation += 1;
            drop(s);
            last_preview = Some(jpeg);
            ctx.request_repaint();
        }

        let next_preview = if connected && animating { now + PREVIEW_INTERVAL } else { next_status };
        let wake = next_status.min(next_preview);
        match poke.recv_timeout(wake.saturating_duration_since(Instant::now())) {
            // A command finished: refresh status right away.
            Ok(()) => next_status = Instant::now(),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn command_loop(state: &Mutex<State>, ctx: &egui::Context, actions: &Receiver<Action>, poke: &Sender<()>) {
    let mut client = None;
    while let Ok(action) = actions.recv() {
        lock(state).busy = Some(action.busy_text());
        ctx.request_repaint();
        let result = match request(&mut client, &action.request()) {
            Ok(Response::Ok) => Ok(()),
            Ok(Response::Error { message }) => Err(message),
            Ok(other) => Err(format!("unexpected answer {other:?}")),
            Err(e) => Err(e),
        };
        let mut s = lock(state);
        s.busy = None;
        s.last_result = Some(result);
        drop(s);
        let _ = poke.send(()); // refresh status right away
        ctx.request_repaint();
    }
}
