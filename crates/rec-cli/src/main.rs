//! `rec`: control the rec background service from a terminal, script or
//! macro tool, and read saved clips for AI tools (`digest`, `analyze`,
//! `mcp`; see `docs/ai-clips.md`).

mod mcp;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use rec_core::config::{self, Config};
use rec_core::ipc::{self, RecorderState, Request, Response};
use rec_core::profile::{self, Profile, Profiles};
use rec_core::store::{self, BYTES_PER_GB, format_bytes};
use rec_library::image::Imaging;
use rec_library::journal::Journal;
use rec_library::{Library, analysis, clip, digest, sys};

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

per-game profiles (made when rec first records a game):
  rec profile [list]    list the profiles and the settings each one sets
  rec profile show NAME show a profile: its game, player names and settings.
                        NAME is its file name or one of its executables
  rec profile path [NAME]
                        print the profiles folder, or a profile's file

the clips folder:
  rec star [CLIP]       keep a clip: quota pruning never deletes it. CLIP is a
                        path, file name or clip id; default the newest clip
  rec unstar CLIP       let quota pruning delete the clip again
  rec prune [--dry-run] [--quota GB]
                        delete the oldest unstarred clips until rec's clips fit
                        in output.quota_gb (or GB). recd does this after every
                        save; --dry-run only lists what would go

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
    Analyze {
        target: PathBuf,
        force: bool,
    },
    Mcp {
        dirs: Vec<PathBuf>,
    },
    Star {
        clip: Option<String>,
        starred: bool,
    },
    Prune {
        dry_run: bool,
        quota_gb: Option<u32>,
    },
    Profile(ProfileCommand),
}

#[derive(Debug, PartialEq)]
enum ProfileCommand {
    List,
    Show(String),
    /// The folder, or a profile's file.
    Path(Option<String>),
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
        Some("star") => Command::Star {
            clip: args.next().map(str::to_owned),
            starred: true,
        },
        Some("unstar") => Command::Star {
            clip: Some(args.next().ok_or("`rec unstar` needs a clip")?.to_owned()),
            starred: false,
        },
        Some("prune") => {
            let (mut dry_run, mut quota_gb) = (false, None);
            while let Some(arg) = args.next() {
                match arg {
                    "--dry-run" => dry_run = true,
                    "--quota" => {
                        let v = args.next().ok_or("`--quota` needs a number of GB")?;
                        quota_gb = match v.parse::<u32>() {
                            Ok(n) if n > 0 => Some(n),
                            _ => return Err(format!("`--quota {v}`: give a whole number of GB")),
                        };
                    }
                    other => return Err(format!("unexpected argument `{other}`")),
                }
            }
            return Ok(Command::Prune { dry_run, quota_gb });
        }
        Some("config") => Command::ConfigPath,
        Some("profile") => Command::Profile(match args.next() {
            None | Some("list") => ProfileCommand::List,
            Some("show") => ProfileCommand::Show(
                args.next()
                    .ok_or("`rec profile show` needs a profile name")?
                    .to_owned(),
            ),
            Some("path") => ProfileCommand::Path(args.next().map(str::to_owned)),
            Some(other) => return Err(format!("unknown profile command `{other}`")),
        }),
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

fn run_star(key: Option<&str>, starred: bool) -> ExitCode {
    // A path names the clip directly; anything else is looked up in the
    // clips folder.
    let root = match key {
        Some(k) if Path::new(k).is_file() => PathBuf::from(k),
        _ => sys::clips_dir(),
    };
    let mut library = Library::scan(std::slice::from_ref(&root), &Journal::default_dir());
    let found = match key {
        Some(k) => library.position(k),
        None => newest(&library),
    };
    let Some(i) = found else {
        match key {
            Some(k) => eprintln!("rec: no clip `{k}` in {}", root.display()),
            None => eprintln!("rec: no rec clips in {}", root.display()),
        }
        return ExitCode::FAILURE;
    };
    let target = &mut library.clips[i];
    if let Err(e) = clip::set_starred(target, starred) {
        eprintln!("rec: {e}");
        return ExitCode::FAILURE;
    }
    let name = target.video.as_deref().unwrap_or(Path::new(&target.file));
    let verb = if starred { "starred" } else { "unstarred" };
    println!("{verb}: {}", name.display());
    ExitCode::SUCCESS
}

/// The most recently saved clip, by the video's modified time.
fn newest(library: &Library) -> Option<usize> {
    (library.clips.iter().enumerate())
        .filter_map(|(i, c)| {
            let modified = std::fs::metadata(c.video.as_deref()?)
                .ok()?
                .modified()
                .ok()?;
            Some((modified, i))
        })
        .max()
        .map(|(_, i)| i)
}

fn run_prune(dry_run: bool, quota_gb: Option<u32>) -> ExitCode {
    let configured = || {
        Config::load(&config::default_config_path())
            .ok()
            .and_then(|c| c.output.quota_gb)
    };
    let Some(gb) = quota_gb.or_else(configured) else {
        eprintln!("rec: no quota: set output.quota_gb in the config file, or pass --quota GB");
        return ExitCode::FAILURE;
    };
    let quota = u64::from(gb) * BYTES_PER_GB;
    let root = sys::clips_dir();
    let pruned = store::prune(&root, &Journal::default_dir(), quota, dry_run);
    for problem in &pruned.problems {
        eprintln!("kept, sidecar unreadable: {problem}");
    }
    let plan = &pruned.plan;
    let gone = if dry_run {
        &plan.delete
    } else {
        &pruned.deleted
    };
    let verb = if dry_run { "would delete" } else { "deleted" };
    for clip in gone {
        println!(
            "{verb} {} ({})",
            clip.video.display(),
            format_bytes(clip.bytes)
        );
    }
    for (video, e) in &pruned.failed {
        eprintln!("couldn't delete {}: {e}", video.display());
    }
    if pruned.quick {
        println!(
            "{}: every video together fits in {}, nothing to delete",
            root.display(),
            format_bytes(quota)
        );
        return ExitCode::SUCCESS;
    }
    let freed: u64 = gone.iter().map(|c| c.bytes).sum();
    let left = plan.used - freed;
    println!(
        "{}: rec's clips use {} of {}",
        root.display(),
        format_bytes(plan.used),
        format_bytes(quota)
    );
    if !gone.is_empty() {
        let then = if dry_run { "would use" } else { "now use" };
        println!("they {then} {}", format_bytes(left));
    }
    if plan.kept_clips > 0 {
        let clips = |n: usize| if n == 1 { "clip" } else { "clips" };
        let why = if plan.starred == plan.kept_clips {
            "starred".to_string()
        } else {
            format!(
                "{} starred, the rest read-only or with an unreadable sidecar",
                plan.starred
            )
        };
        println!(
            "kept, not counted: {} {}, {} ({why})",
            plan.kept_clips,
            clips(plan.kept_clips),
            format_bytes(plan.kept)
        );
    }
    if left > quota {
        println!(
            "the rest are too new to delete: the newest clip and clips saved in the last few minutes"
        );
    }
    if pruned.failed.is_empty() {
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

fn run_profile(command: &ProfileCommand) -> ExitCode {
    let dir = profile::default_dir();
    if *command == ProfileCommand::Path(None) {
        println!("{}", dir.display());
        return ExitCode::SUCCESS;
    }
    let found = Profiles::load(&dir);
    for problem in &found.problems {
        eprintln!("rec: skipped {problem}");
    }
    let (ProfileCommand::Show(name) | ProfileCommand::Path(Some(name))) = command else {
        if found.profiles.is_empty() {
            println!(
                "no profiles in {} yet: rec makes one the first time it records a game",
                dir.display()
            );
        }
        for p in &found.profiles {
            println!("{}: {}", p.id, game_line(p));
            let sets: Vec<String> = (p.overrides().into_iter())
                .map(|(key, value)| format!("{key} = {value}"))
                .collect();
            if sets.is_empty() {
                println!("  no settings of its own");
            } else {
                println!("  sets {}", sets.join(", "));
            }
        }
        return ExitCode::SUCCESS;
    };
    let Some(p) = found.get(name) else {
        eprintln!("rec: no profile `{name}` in {}", dir.display());
        return ExitCode::FAILURE;
    };
    if let ProfileCommand::Path(_) = command {
        println!("{}", p.path.display());
        return ExitCode::SUCCESS;
    }
    println!("{}", p.path.display());
    println!("game: {}", game_line(p));
    if p.player_names.is_empty() {
        println!("player names: none ([player] names, written into clips for AI tools)");
    } else {
        println!("player names: {}", p.player_names.join(", "));
    }
    let overrides = p.overrides();
    if overrides.is_empty() {
        println!("settings: none of its own, all from config.toml");
    } else {
        println!("settings (everything else from config.toml):");
        for (key, value) in overrides {
            println!("  {key} = {value}");
        }
    }
    if !p.ignored.is_empty() {
        println!("ignored, not per-game settings: {}", p.ignored.join(", "));
    }
    ExitCode::SUCCESS
}

/// "Team Fortress 2 (tf_win64.exe, Steam app 440)".
fn game_line(p: &Profile) -> String {
    let mut ids = p.exe.clone();
    ids.extend(p.steam_appid.map(|app| format!("Steam app {app}")));
    if ids.is_empty() {
        p.display_name().to_owned()
    } else {
        format!("{} ({})", p.display_name(), ids.join(", "))
    }
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
        Ok(Command::Star { clip, starred }) => run_star(clip.as_deref(), starred),
        Ok(Command::Prune { dry_run, quota_gb }) => run_prune(dry_run, quota_gb),
        Ok(Command::Profile(command)) => run_profile(&command),
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
    fn parses_profile_commands() {
        let p = |c| Ok(Command::Profile(c));
        assert_eq!(parse_str("profile"), p(ProfileCommand::List));
        assert_eq!(parse_str("profile list"), p(ProfileCommand::List));
        assert_eq!(
            parse_str("profile show tf2"),
            p(ProfileCommand::Show("tf2".into()))
        );
        assert_eq!(parse_str("profile path"), p(ProfileCommand::Path(None)));
        assert_eq!(
            parse_str("profile path tf_win64.exe"),
            p(ProfileCommand::Path(Some("tf_win64.exe".into())))
        );
        assert!(parse_str("profile show").is_err());
        assert!(parse_str("profile edit tf2").is_err());
        assert!(parse_str("profile list extra").is_err());
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

    #[test]
    fn parses_store_commands() {
        assert_eq!(
            parse_str("star"),
            Ok(Command::Star {
                clip: None,
                starred: true
            })
        );
        assert_eq!(
            parse_str("star 01JABC"),
            Ok(Command::Star {
                clip: Some("01JABC".into()),
                starred: true
            })
        );
        assert_eq!(
            parse_str("unstar a.mp4"),
            Ok(Command::Star {
                clip: Some("a.mp4".into()),
                starred: false
            })
        );
        assert_eq!(
            parse_str("prune"),
            Ok(Command::Prune {
                dry_run: false,
                quota_gb: None
            })
        );
        assert_eq!(
            parse_str("prune --quota 20 --dry-run"),
            Ok(Command::Prune {
                dry_run: true,
                quota_gb: Some(20)
            })
        );
        for bad in [
            "unstar",
            "star a b",
            "prune --quota",
            "prune --quota 0",
            "prune --quota 1.5",
            "prune now",
        ] {
            assert!(parse_str(bad).is_err(), "{bad}");
        }
    }
}
