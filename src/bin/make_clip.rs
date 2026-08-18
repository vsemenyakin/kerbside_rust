//! Render the synthetic scene to files, for looking at or for feeding a port.
//!
//! ```text
//!     make_clip --frames 400 --mp4 clip.mp4
//!     make_clip --frames 8 --png-dir frames/
//!     make_clip --frames 400 --raw clip.bgr
//!     make_clip --frames 20 --hash
//! ```
//!
//! The application does not need this -- it generates frames itself, which is
//! the whole reason the input is reproducible. This exists for two other jobs:
//!
//! * **Looking at the scene.** `--mp4` or `--png-dir`.
//! * **Checking this port's generator against the Python's.** `--hash` prints
//!   the SHA-256 of each frame, and the two implementations must agree on every
//!   one of them before any downstream comparison means anything. This is the
//!   first thing to run after touching `source/road.rs` or `numpy_rng.rs`.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::process::ExitCode;

use kerbside::config;
use kerbside::output::frame_hash;
use kerbside::source::RoadScene;
use opencv::prelude::*;

struct Args {
    frames: i64,
    seed: Option<i64>,
    mp4: Option<String>,
    png_dir: Option<String>,
    raw: Option<String>,
    hash: bool,
    truth: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        frames: 400,
        seed: None,
        mp4: None,
        png_dir: None,
        raw: None,
        hash: false,
        truth: false,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = || {
            argv.next()
                .ok_or_else(|| format!("{arg} expects a value"))
        };
        match arg.as_str() {
            "--frames" => args.frames = value()?.parse().map_err(|e| format!("{}{e}", obfstr::obfstr!("--frames: ")))?,
            "--seed" => args.seed = Some(value()?.parse().map_err(|e| format!("{}{e}", obfstr::obfstr!("--seed: ")))?),
            "--mp4" => args.mp4 = Some(value()?),
            "--png-dir" => args.png_dir = Some(value()?),
            "--raw" => args.raw = Some(value()?),
            "--hash" => args.hash = true,
            "--truth" => args.truth = true,
            "-h" | "--help" => {
                println!("{}", include_str!("make_clip_usage.txt"));
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

fn run() -> Result<(), String> {
    let args = parse_args()?;

    // Typed overrides (no field-name strings): mirrors the main binary so this
    // tool builds under `dist`'s --no-default-features too. See config/mod.rs.
    let frames = args.frames;
    let mut overrides: Vec<config::Override> = vec![Box::new(move |s| s.video.SCENE_FRAMES = frames)];
    if let Some(seed) = args.seed {
        overrides.push(Box::new(move |s| s.video.SCENE_SEED = seed));
    }
    let settings = config::resolve(None, overrides, false)?;
    let scene = RoadScene::new(&settings)?;

    let mut writer = match &args.mp4 {
        Some(path) => {
            let fourcc = opencv::videoio::VideoWriter::fourcc('m', 'p', '4', 'v')
                .map_err(|e| format!("{}{e}", obfstr::obfstr!("fourcc: ")))?;
            let writer = opencv::videoio::VideoWriter::new(
                path,
                fourcc,
                settings.video.FPS as f64,
                opencv::core::Size::new(settings.video.FRAME_WIDTH, settings.video.FRAME_HEIGHT),
                true,
            )
            .map_err(|e| format!("cannot open video writer for {path:?}: {e}"))?;
            if !writer.is_opened()
                .map_err(|e| format!("{}{e}", obfstr::obfstr!("video writer: ")))?
            {
                return Err(format!("cannot open video writer for {path:?}"));
            }
            Some(writer)
        }
        None => None,
    };
    if let Some(dir) = &args.png_dir {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {dir:?}: {e}"))?;
    }
    let mut raw = match &args.raw {
        Some(path) => Some(BufWriter::new(
            File::create(path).map_err(|e| format!("cannot open {path:?}: {e}"))?,
        )),
        None => None,
    };

    // Insertion-ordered so the truth listing comes out sorted by id below, the
    // same way the Python's dict does.
    let mut seen: Vec<(i64, f64)> = Vec::new();
    for frame_id in 0..args.frames {
        let (image, truth) = scene.render(frame_id)?;
        if let Some(writer) = writer.as_mut() {
            writer.write(&image)
                .map_err(|e| format!("cannot write frame {frame_id}: {e}"))?;
        }
        if let Some(dir) = &args.png_dir {
            let path = format!("{dir}/{frame_id:06}.png");
            opencv::imgcodecs::imwrite_def(&path, &image)
                .map_err(|e| format!("cannot write {path:?}: {e}"))?;
        }
        if let Some(raw) = raw.as_mut() {
            let bytes = image.data_bytes()
                .map_err(|e| format!("frame {frame_id} is not contiguous: {e}"))?;
            raw.write_all(bytes)
                .map_err(|e| format!("cannot write raw frame {frame_id}: {e}"))?;
        }
        if args.hash {
            println!(
                "{frame_id:6}  {}  {} vehicles",
                frame_hash(&image)?,
                truth.visible().len()
            );
        }
        for vehicle in truth.visible() {
            if !seen.iter().any(|(id, _)| *id == vehicle.vehicle_id) {
                seen.push((vehicle.vehicle_id, vehicle.speed_kph));
            }
        }
    }
    if let Some(raw) = raw.as_mut() {
        raw.flush().map_err(|e| format!("{}{e}", obfstr::obfstr!("cannot flush the raw clip: ")))?;
    }

    seen.sort_by_key(|(id, _)| *id);
    if args.truth {
        println!("\nground truth -- vehicle id, true speed:");
        for (vehicle_id, speed) in &seen {
            println!("  {vehicle_id:4}  {speed:6.2} km/h");
        }
    }

    eprintln!(
        "\n{} frames, {}x{} 8-bit BGR, {} fps, seed {}, {} vehicles",
        args.frames,
        settings.video.FRAME_WIDTH,
        settings.video.FRAME_HEIGHT,
        settings.video.FPS,
        settings.video.SCENE_SEED,
        seen.len()
    );
    if args.mp4.is_some() {
        eprintln!(
            "note: the mp4 is lossy and is NOT a valid input for a port \
             comparison -- use the generator, or --raw."
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("make_clip: {message}");
            ExitCode::FAILURE
        }
    }
}
