//! Test tool: handshake with the P13 and show a color, an image or a GIF.
//! Static sources are sent once; GIFs loop until Ctrl+C.
//!
//! Usage: aio-show [--key FILE] [--handshake FILE] [--replay] [--responses FILE]
//!                 [--cache DIR] [--dry-run] <color | file>
//!        aio-show --info | --brightness N     (HID control only)

use std::io::Read;
use std::path::{Path, PathBuf};

use aio_daemon::media::{self, Source};
use aio_daemon::player::Player;
use aio_daemon::hid::HidDevice;
use aio_daemon::paths;
use aio_daemon::winusb::{OpenError, WinUsbTransport};
use aio_proto::{Auth, Display};
use anyhow::{Context, Result, bail};
use tracing::{info, warn};

const USAGE: &str = r"usage: aio-show [options] <color | file>
       aio-show --info            print the device info (HID only, no display handshake)
       aio-show --brightness N    set the backlight 0-100 (HID only)
  color:            a name (red, teal, ...) or #rrggbb
  file:             an image, GIF or video (animations loop until Ctrl+C)
  --key FILE        device public key overriding the built-in one (default
                    %ProgramData%\aio-ui\device_key.pem, used if present)
  --handshake FILE  replay data (default %ProgramData%\aio-ui\handshake.bin)
  --replay          replay a captured handshake.bin instead (developer use)
  --responses FILE  captured device answers to compare (replay only)
  --cache DIR       frame cache directory
  --dry-run         import into the cache only, don't open the device";

struct Args {
    key: PathBuf,
    handshake: PathBuf,
    replay: bool,
    responses: PathBuf,
    cache: PathBuf,
    dry_run: bool,
    info: bool,
    brightness: Option<u8>,
    source: String,
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        key: paths::device_key_file(),
        handshake: paths::handshake_file(),
        replay: false,
        responses: paths::expected_responses_file(),
        cache: paths::cache_dir(),
        dry_run: false,
        info: false,
        brightness: None,
        source: String::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().with_context(|| format!("{name} needs a path"));
        match arg.as_str() {
            "--key" => args.key = value("--key")?.into(),
            "--handshake" => args.handshake = value("--handshake")?.into(),
            "--replay" => args.replay = true,
            "--responses" => args.responses = value("--responses")?.into(),
            "--cache" => args.cache = value("--cache")?.into(),
            "--dry-run" => args.dry_run = true,
            "--info" => args.info = true,
            "--brightness" => {
                let v = value("--brightness")?;
                args.brightness = Some(v.parse().ok().filter(|v| *v <= 100).with_context(|| format!("brightness must be 0-100, got {v:?}"))?);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ if args.source.is_empty() => args.source = arg,
            _ => bail!("unexpected argument {arg:?}\n{USAGE}"),
        }
    }
    if args.source.is_empty() && !args.info && args.brightness.is_none() {
        bail!("{USAGE}");
    }
    Ok(args)
}

fn is_gif(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && &magic == b"GIF8"
}

fn is_video(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    matches!(ext.as_str(), "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" | "wmv")
}

fn parse_source(s: &str) -> Result<Source> {
    let path = PathBuf::from(s);
    if path.is_file() {
        return Ok(if is_gif(&path) {
            Source::Gif { path }
        } else if is_video(&path) {
            Source::Video { path: std::path::absolute(path)? }
        } else {
            Source::Image { path }
        });
    }
    match aio_ipc::parse_color(s) {
        Some(rgb) => Ok(Source::Color { rgb }),
        None => bail!("{s:?} is neither an existing file nor a color"),
    }
}

/// Talks to the HID control interface only (works while the daemon runs).
fn hid_only(args: &Args) -> Result<()> {
    use aio_proto::hid::{Command, HidClient};
    let mut hid = HidClient::new(HidDevice::open().context("opening the HID control interface")?);
    let info = hid.request(Command::Conn).context("conn")?;
    // Hide the serial number in the printout.
    let mut shown: serde_json::Value = serde_json::from_str(&info.body).unwrap_or_default();
    if let Some(sn) = shown.get_mut("sn") {
        *sn = "…".into();
    }
    println!("device info: {shown}");
    if let Some(value) = args.brightness {
        hid.request(Command::Brightness(value)).context("brightness")?;
        println!("brightness set to {value}");
    }
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = parse_args()?;
    if args.info || args.brightness.is_some() {
        return hid_only(&args);
    }
    let auth = paths::load_auth(&args.key, &args.handshake, args.replay)?;
    let expected = paths::load_expected_responses(&args.responses)?;

    // Import before touching the device so a bad source never leaves it mid-session.
    let source = parse_source(&args.source)?;
    let media = media::import(&source, &args.cache)?;
    info!(
        frames = media.frames().len(),
        loop_duration = ?media.loop_duration(),
        "source ready"
    );
    if args.dry_run {
        return Ok(());
    }

    let transport = WinUsbTransport::open(None).map_err(|e| match e {
        OpenError::NotFound => anyhow::anyhow!(
            "{e}. Is the cooler connected and WinUSB installed on \"P13 (Interface 0)\"?"
        ),
        OpenError::Busy(_) => {
            anyhow::anyhow!("{e}. Close Steam / MSI Center / other peripheral software and retry.")
        }
        e => e.into(),
    })?;
    info!("device opened");

    let mut display =
        Display::connect(transport, &auth).with_context(|| format!("{} handshake failed", auth.kind()))?;
    let di = display.device_info();
    info!(
        width = di.width,
        height = di.height,
        refresh_hz = di.refresh_hz,
        monitor = di.monitor_name().unwrap_or_default(),
        handshake = auth.kind(),
        "handshake ok"
    );
    match (&auth, &expected) {
        (Auth::Replay(_), Some(exp)) if exp == display.auth_responses() => {
            info!("device responses match capture")
        }
        (Auth::Replay(_), Some(_)) => warn!("device responses differ from the capture (handshake still completed)"),
        _ => {}
    }

    let mut player = Player::new(media);
    if !player.media().is_static() {
        info!("playing; Ctrl+C to stop");
    }
    player.run(&mut display).context("playback")?;
    info!("frame sent");
    Ok(())
}
