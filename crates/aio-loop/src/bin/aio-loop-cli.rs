//! aio-loop-cli: the command-line side of the loop finder (`aio-loop` is the window).
//!
//!   aio-loop-cli info <video>
//!   aio-loop-cli find <video> <start> <end> [--radius S]
//!   aio-loop-cli cut  <video> <start-frame> <end-frame> [-o out.mp4] [--audio]
//!   aio-loop-cli auto <video> <start> <end> [--radius S] [-o out.mp4] [--audio]
//!
//! Times are seconds or m:ss(.mmm). `cut` keeps frames [start, end).

use std::path::{Path, PathBuf};

use aio_loop::ffmpeg::{self, VideoInfo};
use aio_loop::matcher::quality;
use aio_loop::{Stage, search, timecode};
use anyhow::{Context, Result, bail};

const USAGE: &str = "usage:
  aio-loop-cli info <video>
  aio-loop-cli find <video> <start> <end> [--radius S]
  aio-loop-cli cut  <video> <start-frame> <end-frame> [-o out.mp4] [--audio]
  aio-loop-cli auto <video> <start> <end> [--radius S] [-o out.mp4] [--audio]
times: seconds or m:ss(.mmm); cut keeps frames [start, end)";

struct Options {
    positional: Vec<String>,
    radius: f64,
    output: Option<PathBuf>,
    audio: bool,
}

fn parse_options(args: &[String]) -> Result<Options> {
    let mut o = Options { positional: Vec::new(), radius: 2.0, output: None, audio: false };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--radius" => o.radius = it.next().and_then(|v| v.parse().ok()).context("--radius needs seconds")?,
            "-o" | "--output" => o.output = Some(it.next().context("-o needs a path")?.into()),
            "--audio" => o.audio = true,
            _ => o.positional.push(arg.clone()),
        }
    }
    Ok(o)
}

fn time(s: &str) -> Result<f64> {
    timecode::parse(s).with_context(|| format!("not a time: {s:?} (use seconds or m:ss.mmm)"))
}

fn print_info(info: &VideoInfo) {
    println!(
        "{}x{}  {:.3} fps  {}  {} frames  {}{}",
        info.width,
        info.height,
        info.rate.fps(),
        timecode::format(info.duration),
        info.frames,
        info.codec,
        if info.has_audio { "  (has audio)" } else { "" }
    );
}

fn cut_with_progress(tools: &ffmpeg::Tools, info: &VideoInfo, start: u64, end: u64, out: &Path, audio: bool) -> Result<()> {
    println!("writing {} ({} frames, {})", out.display(), end - start, timecode::format(info.rate.time_of(end - start)));
    ffmpeg::cut(tools, info, start, end, out, audio, |_| {})?;
    println!("done");
    Ok(())
}

fn run_cli(args: &[String]) -> Result<()> {
    let o = parse_options(&args[1..])?;
    let video = |i: usize| -> Result<PathBuf> { Ok(PathBuf::from(o.positional.get(i).context(USAGE)?)) };
    let tools = ffmpeg::find_tools()?;
    match args[0].as_str() {
        "info" => print_info(&ffmpeg::probe(&tools, &video(0)?)?),
        "find" | "auto" => {
            let path = video(0)?;
            let (start, end) = (time(o.positional.get(1).context(USAGE)?)?, time(o.positional.get(2).context(USAGE)?)?);
            let info = ffmpeg::probe(&tools, &path)?;
            print_info(&info);
            let s = search(&tools, &info, start, end, o.radius, |stage| {
                eprintln!("{}", match stage {
                    Stage::DecodingStart => "decoding frames around the start...",
                    Stage::DecodingEnd => "decoding frames around the end...",
                    Stage::Comparing => "comparing...",
                });
            })?;
            println!("  #  start frame (time)        end frame (time)          length             diff");
            for (i, c) in s.candidates.iter().enumerate() {
                println!(
                    "{:>3}  {:>7} ({:>10})  {:>7} ({:>10})  {:>6} fr ({:>9})  {:.2}% {}",
                    i + 1,
                    c.start,
                    timecode::format(info.rate.time_of(c.start)),
                    c.end,
                    timecode::format(info.rate.time_of(c.end)),
                    c.frames(),
                    timecode::format(info.rate.time_of(c.frames())),
                    c.score * 100.0,
                    quality(c.score)
                );
            }
            if args[0] == "auto" {
                let best = s.candidates[0];
                let out = o.output.clone().unwrap_or_else(|| ffmpeg::loop_path(&path));
                cut_with_progress(&tools, &info, best.start, best.end, &out, o.audio)?;
            }
        }
        "cut" => {
            let path = video(0)?;
            let frame = |i: usize| -> Result<u64> {
                o.positional.get(i).and_then(|v| v.parse().ok()).with_context(|| format!("frame number expected\n{USAGE}"))
            };
            let (start, end) = (frame(1)?, frame(2)?);
            let info = ffmpeg::probe(&tools, &path)?;
            let out = o.output.clone().unwrap_or_else(|| ffmpeg::loop_path(&path));
            cut_with_progress(&tools, &info, start, end, &out, o.audio)?;
        }
        "-h" | "--help" | "help" => println!("{USAGE}"),
        other => bail!("unknown command {other:?}\n{USAGE}"),
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        println!("{USAGE}");
        return;
    }
    if let Err(e) = run_cli(&args) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
