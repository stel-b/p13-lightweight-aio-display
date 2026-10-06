//! aio-cli: control the aio daemon from the command line.

use std::path::{Path, PathBuf};

use aio_ipc::{Client, DeviceState, Request, Response, Source, Status};
use anyhow::{Context, Result, bail};

const USAGE: &str = "usage: aio-cli <command>

commands:
  set color <name | #rrggbb>   show a solid color
  set image <path>             show a still image
  set gif <path>               play an animated GIF
  brightness <0-100>           backlight level (0 turns the backlight off)
  rotate <0|90|180|270>        rotate the picture on the device
  pause                        stop sending frames (the display keeps the current one)
  resume                       continue playback
  status [--json]              show daemon and device state
  preview <out.jpg>            save the frame currently on the display";

fn absolute_file(path: &str) -> Result<PathBuf> {
    let path = std::path::absolute(Path::new(path))?;
    if !path.is_file() {
        bail!("{} is not a file", path.display());
    }
    Ok(path)
}

fn parse_request(args: &[String]) -> Result<Request> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(match args.as_slice() {
        ["set", "color", c] => {
            let rgb = aio_ipc::parse_color(c).with_context(|| format!("unknown color {c:?}"))?;
            Request::SetSource { source: Source::Color { rgb } }
        }
        ["set", "image", p] => Request::SetSource { source: Source::Image { path: absolute_file(p)? } },
        ["set", "gif", p] => Request::SetSource { source: Source::Gif { path: absolute_file(p)? } },
        ["set", "video", p] => Request::SetSource { source: Source::Video { path: absolute_file(p)? } },
        ["brightness", v] => {
            let value: u8 = v.parse().ok().filter(|v| *v <= 100).with_context(|| format!("brightness must be 0-100, got {v:?}"))?;
            Request::SetBrightness { value }
        }
        ["rotate", d] => {
            let degrees: u16 = d.parse().ok().filter(|d| matches!(d, 0 | 90 | 180 | 270)).with_context(|| format!("rotation must be 0, 90, 180 or 270, got {d:?}"))?;
            Request::SetRotation { degrees }
        }
        ["pause"] => Request::Pause,
        ["resume"] => Request::Resume,
        ["status"] | ["status", "--json"] => Request::GetStatus,
        ["preview", _] => Request::GetPreview,
        _ => bail!("{USAGE}"),
    })
}

fn describe(source: &Source) -> String {
    match source {
        Source::Color { rgb: [r, g, b] } => format!("color #{r:02x}{g:02x}{b:02x}"),
        Source::Image { path } => format!("image {}", path.display()),
        Source::Gif { path } => format!("gif {}", path.display()),
        Source::Video { path } => format!("video {}", path.display()),
    }
}

fn print_status(s: &Status) {
    let device = match s.device {
        DeviceState::Connected => "connected",
        DeviceState::Disconnected => "disconnected",
        DeviceState::Busy => "busy (in use by another program)",
    };
    println!("device:      {device}");
    println!("source:      {}", s.source.as_ref().map_or("none".into(), describe));
    println!("playback:    {}", if s.paused { "paused" } else { "playing" });
    println!("fps:         {:.1}", s.fps);
    println!("frames sent: {}", s.frames_sent);
    println!("brightness:  {}", s.brightness);
    println!("rotation:    {}", s.rotation.map_or("device default".into(), |d| format!("{d} degrees")));
    if let Some(f) = &s.firmware {
        println!("firmware:    {f}");
    }
    if let Some(e) = &s.last_error {
        println!("last error:  {e}");
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        println!("{USAGE}");
        return Ok(());
    }
    let request = parse_request(&args)?;
    let mut client = Client::connect().context("connecting to the daemon")?;
    let response = client.request(&request).context("talking to the daemon")?;

    match response {
        Response::Ok => {}
        Response::Error { message } => bail!(message),
        Response::Status(status) if args.get(1).is_some_and(|a| a == "--json") => {
            let line = aio_ipc::encode_line(&status);
            print!("{}", String::from_utf8_lossy(&line));
        }
        Response::Status(status) => print_status(&status),
        Response::Preview { jpeg } => {
            let out = &args[1];
            std::fs::write(out, &jpeg).with_context(|| format!("writing {out}"))?;
            println!("saved {} bytes to {out}", jpeg.len());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_commands() {
        assert_eq!(
            parse_request(&args("set color teal")).unwrap(),
            Request::SetSource { source: Source::Color { rgb: [0, 128, 128] } }
        );
        assert_eq!(parse_request(&args("pause")).unwrap(), Request::Pause);
        assert_eq!(parse_request(&args("brightness 0")).unwrap(), Request::SetBrightness { value: 0 });
        assert_eq!(parse_request(&args("rotate 90")).unwrap(), Request::SetRotation { degrees: 90 });
        assert!(parse_request(&args("brightness 101")).is_err());
        assert!(parse_request(&args("rotate 45")).is_err());
        assert_eq!(parse_request(&args("status --json")).unwrap(), Request::GetStatus);
        assert_eq!(parse_request(&args("preview out.jpg")).unwrap(), Request::GetPreview);
    }

    #[test]
    fn file_paths_become_absolute() {
        let Request::SetSource { source: Source::Gif { path } } =
            parse_request(&args("set gif Cargo.toml")).unwrap()
        else {
            panic!()
        };
        assert!(path.is_absolute());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_request(&args("set color nope")).is_err());
        assert!(parse_request(&args("set gif missing.gif")).is_err());
        assert!(parse_request(&args("reboot")).is_err());
    }
}
