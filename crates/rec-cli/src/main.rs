//! `rec`: control the rec background service from a terminal, script or
//! macro tool, and read saved clips for AI tools (`digest`, `analyze`,
//! `mcp`; see `docs/ai-clips.md`).

mod mcp;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rec_core::config;
use rec_core::ipc::{self, RecorderState, Request, Response};
use rec_library::image::Imaging;
use rec_library::journal::Journal;
use rec_library::{Library, analysis, digest, sys};

const USAGE: &str = "\
rec - control the rec background recorder

usage:
  rec status            show what is being recorded
  rec save [SECONDS]    save a clip (default length from config)
  rec mark              add a marker at the current moment
  rec pause | resume    pause or resume buffering
  rec reload            reload the config file
  rec quit              stop the background service
  rec config            print the config file path

saved clips, for AI tools:
  rec digest PATH [--out DIR] [--every SECONDS] [--width PX] [--no-analysis] [--no-frames]
                        write a bundle a model can read, for a clip or for every
                        clip in a folder (then with index.json and overlap
                        groups): the sidecar, summary.md, stills and contact
                        sheets. DIR defaults to rec-digest next to the clips
  rec analyze PATH [--force]
                        add a thumbnail, scene cuts, brightness and motion to
                        the sidecars of a clip or of every clip in a folder
  rec mcp [--dir DIR]...
                        serve the clip library to an AI agent as a local MCP
                        server on stdin/stdout (DIR defaults to the clips folder)";

#[derive(Debug, PartialEq)]
enum Command {
    Help,
    ConfigPath,
    Send(Request),
    Digest(DigestArgs),
    Analyze { target: PathBuf, force: bool },
    Mcp { dirs: Vec<PathBuf> },
}

#[derive(Debug, PartialEq)]
struct DigestArgs {
    target: PathBuf,
    out: Option<PathBuf>,
    every: f64,
    width: u32,
    analyze: bool,
    frames: bool,
}

fn parse(args: &[String]) -> Result<Command, String> {
    let mut args = args.iter().map(String::as_str);
    let command = match args.next() {
        None | Some("help" | "-h" | "--help") => Command::Help,
        Some("digest") => return parse_digest(args),
        Some("analyze") => {
            let (mut target, mut force) = (None, false);
            for arg in args {
                match arg {
                    "--force" => force = true,
                    flag if flag.starts_with("--") => {
                        return Err(format!("unknown option `{flag}`"));
                    }
                    path if target.is_none() => target = Some(PathBuf::from(path)),
                    extra => return Err(format!("unexpected argument `{extra}`")),
                }
            }
            let target = target.ok_or("`rec analyze` needs a clip or folder")?;
            return Ok(Command::Analyze { target, force });
        }
        Some("mcp") => {
            let mut dirs = Vec::new();
            while let Some(arg) = args.next() {
                match arg {
                    "--dir" => {
                        let dir = args.next().ok_or("`--dir` needs a folder")?;
                        dirs.push(PathBuf::from(dir));
                    }
                    other => return Err(format!("unexpected argument `{other}`")),
                }
            }
            return Ok(Command::Mcp { dirs });
        }
        Some("config") => Command::ConfigPath,
        Some("status") => Command::Send(Request::Status),
        Some("save") => {
            let seconds = args
                .next()
                .map(|s| match s.parse::<u32>() {
                    Ok(n) if n > 0 => Ok(n),
                    _ => Err(format!("`{s}` is not a number of seconds")),
                })
                .transpose()?;
            Command::Send(Request::Save { seconds })
        }
        Some("mark") => Command::Send(Request::Mark),
        Some("pause") => Command::Send(Request::Pause),
        Some("resume") => Command::Send(Request::Resume),
        Some("reload") => Command::Send(Request::ReloadConfig),
        Some("quit") => Command::Send(Request::Quit),
        Some(other) => return Err(format!("unknown command `{other}`")),
    };
    match args.next() {
        Some(extra) => Err(format!("unexpected argument `{extra}`")),
        None => Ok(command),
    }
}

fn parse_digest<'a>(mut args: impl Iterator<Item = &'a str>) -> Result<Command, String> {
    let defaults = digest::Options::default();
    let mut parsed = DigestArgs {
        target: PathBuf::new(),
        out: None,
        every: defaults.every,
        width: defaults.width,
        analyze: true,
        frames: true,
    };
    let mut target = None;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("`{name}` needs a value"));
        match arg {
            "--out" => parsed.out = Some(PathBuf::from(value("--out")?)),
            "--every" => {
                let v = value("--every")?;
                parsed.every = match v.parse::<f64>() {
                    Ok(s) if s >= 0.5 => s,
                    _ => {
                        return Err(format!(
                            "`--every {v}`: give a number of seconds, 0.5 or more"
                        ));
                    }
                };
            }
            "--width" => {
                let v = value("--width")?;
                parsed.width = match v.parse::<u32>() {
                    Ok(w) if (64..=3840).contains(&w) => w,
                    _ => {
                        return Err(format!(
                            "`--width {v}`: give a width from 64 to 3840 pixels"
                        ));
                    }
                };
            }
            "--no-analysis" => parsed.analyze = false,
            "--no-frames" => parsed.frames = false,
            flag if flag.starts_with("--") => return Err(format!("unknown option `{flag}`")),
            path if target.is_none() => target = Some(PathBuf::from(path)),
            extra => return Err(format!("unexpected argument `{extra}`")),
        }
    }
    parsed.target = target.ok_or("`rec digest` needs a clip or folder")?;
    Ok(Command::Digest(parsed))
}

fn run_digest(args: DigestArgs) -> ExitCode {
    let opts = digest::Options {
        out: args.out,
        every: args.every,
        width: args.width,
        analyze: args.analyze,
        frames: args.frames,
        ..digest::Options::default()
    };
    if let Err(e) = sys::init() {
        eprintln!("rec: {e}");
        return ExitCode::FAILURE;
    }
    sys::background();
    let progress = |line: &str| eprintln!("{line}");
    let report = match digest::run(&args.target, &Journal::default_dir(), &opts, progress) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("rec: {e}");
            return ExitCode::FAILURE;
        }
    };
    for done in &report.done {
        eprintln!(
            "  {}: {} stills, {} contact sheets",
            done.file, done.stills, done.sheets
        );
    }
    match &report.index {
        Some(index) => println!("{}", index.display()),
        None => {
            for done in &report.done {
                println!("{}", done.bundle.display());
            }
        }
    }
    if report.failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        eprintln!("rec: {} clips failed", report.failed.len());
        ExitCode::FAILURE
    }
}

fn run_analyze(target: &Path, force: bool) -> ExitCode {
    if let Err(e) = sys::init() {
        eprintln!("rec: {e}");
        return ExitCode::FAILURE;
    }
    sys::background();
    let library = Library::scan(&[target.to_owned()], &Journal::default_dir());
    for problem in &library.problems {
        eprintln!("skipped: {problem}");
    }
    if library.clips.is_empty() {
        eprintln!("rec: no rec clips in {}", target.display());
        return ExitCode::FAILURE;
    }
    let imaging = match Imaging::new() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("rec: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut failed = 0;
    for clip in &library.clips {
        if clip.sidecar.is_none() {
            eprintln!(
                "{}: skipped, it has no sidecar to keep the analysis in",
                clip.file
            );
            continue;
        }
        if clip.analysis.is_some() && !force {
            eprintln!("{}: already analysed (--force runs it again)", clip.file);
            continue;
        }
        match analysis::analyze(clip, &imaging).and_then(|a| analysis::save(&a).map(|()| a)) {
            Ok(a) => {
                let lines = (a.analysis.as_ref())
                    .map(analysis::describe)
                    .unwrap_or_default();
                println!("{}: {}", clip.file, lines.join("; "));
            }
            Err(e) => {
                failed += 1;
                eprintln!("{}: {e}", clip.file);
            }
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn report(response: Response) -> ExitCode {
    match response {
        Response::Ok => println!("ok"),
        Response::Saved { path } => println!("{}", path.display()),
        Response::Status(status) => {
            let state = match status.state {
                RecorderState::Idle => "idle",
                RecorderState::Recording => "recording",
                RecorderState::Paused => "paused",
                RecorderState::Error => "error",
            };
            println!("state:    {state}");
            if let Some(reason) = status.reason {
                println!("reason:   {reason}");
            }
            if let Some(warning) = status.warning {
                println!("problem:  {warning}");
            }
            if let Some(target) = status.target {
                println!("target:   {target}");
            }
            if let Some(encoder) = status.encoder {
                println!("encoder:  {encoder}");
            }
            println!(
                "buffer:   {:.1} s, {:.1} MB",
                status.buffered_seconds,
                status.buffer_bytes as f64 / (1024.0 * 1024.0)
            );
            println!(
                "version:  {} (protocol {})",
                status.version, status.protocol
            );
            if status.state == RecorderState::Error {
                return ExitCode::FAILURE;
            }
        }
        Response::Error { message } => {
            eprintln!("rec: {message}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args) {
        Ok(Command::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Command::ConfigPath) => {
            println!("{}", config::default_config_path().display());
            ExitCode::SUCCESS
        }
        Ok(Command::Digest(args)) => run_digest(args),
        Ok(Command::Analyze { target, force }) => run_analyze(&target, force),
        Ok(Command::Mcp { dirs }) => {
            let dirs = if dirs.is_empty() {
                vec![sys::clips_dir()]
            } else {
                dirs
            };
            match mcp::serve(dirs, Journal::default_dir()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("rec mcp: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Ok(Command::Send(request)) => match ipc::send(&request) {
            Ok(response) => report(response),
            Err(message) => {
                eprintln!("rec: {message}");
                ExitCode::FAILURE
            }
        },
        Err(message) => {
            eprintln!("rec: {message}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(line: &str) -> Result<Command, String> {
        let args: Vec<String> = line.split_whitespace().map(String::from).collect();
        parse(&args)
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse_str(""), Ok(Command::Help));
        assert_eq!(parse_str("--help"), Ok(Command::Help));
        assert_eq!(parse_str("config"), Ok(Command::ConfigPath));
        assert_eq!(parse_str("status"), Ok(Command::Send(Request::Status)));
        assert_eq!(
            parse_str("save"),
            Ok(Command::Send(Request::Save { seconds: None }))
        );
        assert_eq!(
            parse_str("save 30"),
            Ok(Command::Send(Request::Save { seconds: Some(30) }))
        );
        assert_eq!(
            parse_str("reload"),
            Ok(Command::Send(Request::ReloadConfig))
        );
    }

    #[test]
    fn rejects_bad_arguments() {
        assert!(parse_str("save 0").is_err());
        assert!(parse_str("save soon").is_err());
        assert!(parse_str("save 30 40").is_err());
        assert!(parse_str("status now").is_err());
        assert!(parse_str("record").is_err());
    }

    #[test]
    fn parses_clip_commands() {
        let Ok(Command::Digest(d)) =
            parse_str(r"digest C:\clips --every 2.5 --width 800 --no-analysis --out D:\out")
        else {
            panic!("digest");
        };
        assert_eq!(d.target, PathBuf::from(r"C:\clips"));
        assert_eq!(d.out, Some(PathBuf::from(r"D:\out")));
        assert_eq!(
            (d.every, d.width, d.analyze, d.frames),
            (2.5, 800, false, true)
        );
        let Ok(Command::Digest(d)) = parse_str("digest --no-frames a.mp4") else {
            panic!("digest");
        };
        assert_eq!((d.every, d.width, d.frames), (5.0, 1024, false));
        for bad in [
            "digest",
            "digest a b",
            "digest a --every 0",
            "digest a --width x",
            "digest a --out",
            "digest a --bogus",
        ] {
            assert!(parse_str(bad).is_err(), "{bad}");
        }
        assert_eq!(
            parse_str("analyze a.mp4 --force"),
            Ok(Command::Analyze {
                target: "a.mp4".into(),
                force: true
            })
        );
        assert!(parse_str("analyze").is_err());
        assert_eq!(parse_str("mcp"), Ok(Command::Mcp { dirs: vec![] }));
        assert_eq!(
            parse_str(r"mcp --dir C:\a --dir D:\b"),
            Ok(Command::Mcp {
                dirs: vec![r"C:\a".into(), r"D:\b".into()]
            })
        );
        assert!(parse_str("mcp --dir").is_err());
    }
}
