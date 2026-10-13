//! Command-line parsing. Hand-rolled to keep the dependency list short: every
//! option takes a value, written `--name value` or `--name=value`.

use std::collections::HashMap;
use std::path::PathBuf;

pub const USAGE: &str = "\
rec-bench - measure what recording costs a game

usage:
  rec-bench run --process <exe> [options]
      Alternate PresentMon captures with recd off and on, and compare them.
      --process <exe>       the game's executable name, e.g. Deadlock.exe (required)
      --presentmon <path>   PresentMon console app (default: next to rec-bench or on PATH)
      --duration <secs>     length of each capture (default 60)
      --runs <n>            number of off/on pairs (default 3)
      --warmup <secs>       settle time before each capture (default 5)
      --on-warmup <secs>    minimum time a freshly started recd records before an \"on\"
                            capture (default: the replay length in your rec config)
      --recd <path>         recd.exe to start for the \"on\" phases (default: next to rec-bench)
      --save-every <secs>   also save a 10 s clip every N seconds during \"on\" captures
                            and report the frame-time spike around each
      --label <text>        name for this run (default: the process name)
      --out <dir>           results folder (default: bench/results)

  rec-bench analyze [--process <exe>] <file.csv>...
      Print statistics for existing PresentMon CSV captures.

  rec-bench idle [--duration <secs>] [--warmup <secs>] [--recd <path>]
      Measure recd's cost with no game running (default 60 s).";

#[derive(Clone, Debug, PartialEq)]
pub struct RunArgs {
    pub process: String,
    pub presentmon: Option<PathBuf>,
    pub duration: u32,
    pub runs: u32,
    pub warmup: u32,
    /// Override for the "on" warm-up; `None` means the configured replay length.
    pub on_warmup: Option<u32>,
    pub recd: Option<PathBuf>,
    pub save_every: Option<u32>,
    pub label: Option<String>,
    pub out: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AnalyzeArgs {
    pub files: Vec<PathBuf>,
    pub process: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IdleArgs {
    pub duration: u32,
    pub warmup: u32,
    pub recd: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Help,
    Run(RunArgs),
    Analyze(AnalyzeArgs),
    Idle(IdleArgs),
}

/// Splits `args` into `--option value` pairs (checked against `known`) and
/// positional arguments.
fn split(
    args: &[String],
    known: &[&'static str],
) -> Result<(HashMap<&'static str, String>, Vec<String>), String> {
    let mut options = HashMap::new();
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let Some(flag) = arg.strip_prefix("--") else {
            positional.push(arg.clone());
            continue;
        };
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (flag, None),
        };
        let Some(&name) = known.iter().find(|&&k| k == name) else {
            return Err(format!("unknown option `--{name}`"));
        };
        let value = match inline {
            Some(value) => value,
            None => iter
                .next()
                .cloned()
                .ok_or_else(|| format!("`--{name}` needs a value"))?,
        };
        options.insert(name, value);
    }
    Ok((options, positional))
}

fn number(
    options: &HashMap<&'static str, String>,
    name: &str,
    default: Option<u32>,
    range: std::ops::RangeInclusive<u32>,
) -> Result<Option<u32>, String> {
    let Some(text) = options.get(name) else {
        return Ok(default);
    };
    match text.parse::<u32>() {
        Ok(n) if range.contains(&n) => Ok(Some(n)),
        _ => Err(format!(
            "`--{name}` must be a whole number from {} to {}, not `{text}`",
            range.start(),
            range.end()
        )),
    }
}

fn path(options: &HashMap<&'static str, String>, name: &str) -> Option<PathBuf> {
    options.get(name).map(PathBuf::from)
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some((command, rest)) = args.split_first() else {
        return Ok(Command::Help);
    };
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(Command::Help);
    }
    match command.as_str() {
        "help" | "-h" | "--help" => Ok(Command::Help),
        "run" => {
            let known = [
                "process",
                "presentmon",
                "duration",
                "runs",
                "warmup",
                "on-warmup",
                "recd",
                "save-every",
                "label",
                "out",
            ];
            let (options, positional) = split(rest, &known)?;
            if let Some(extra) = positional.first() {
                return Err(format!("unexpected argument `{extra}`"));
            }
            let process = options
                .get("process")
                .filter(|p| !p.trim().is_empty())
                .ok_or("`--process <exe name>` is required")?
                .trim()
                .to_string();
            let duration = number(&options, "duration", Some(60), 5..=3600)?.unwrap_or(60);
            let save_every = number(&options, "save-every", None, 1..=3600)?;
            if save_every.is_some_and(|every| every >= duration) {
                return Err("`--save-every` must be shorter than `--duration`".into());
            }
            Ok(Command::Run(RunArgs {
                process,
                presentmon: path(&options, "presentmon"),
                duration,
                runs: number(&options, "runs", Some(3), 1..=50)?.unwrap_or(3),
                warmup: number(&options, "warmup", Some(5), 0..=600)?.unwrap_or(5),
                on_warmup: number(&options, "on-warmup", None, 0..=3600)?,
                recd: path(&options, "recd"),
                save_every,
                label: options
                    .get("label")
                    .filter(|l| !l.trim().is_empty())
                    .cloned(),
                out: path(&options, "out").unwrap_or_else(|| PathBuf::from("bench/results")),
            }))
        }
        "analyze" => {
            let (options, positional) = split(rest, &["process"])?;
            if positional.is_empty() {
                return Err("`analyze` needs at least one CSV file".into());
            }
            Ok(Command::Analyze(AnalyzeArgs {
                files: positional.into_iter().map(PathBuf::from).collect(),
                process: options.get("process").cloned(),
            }))
        }
        "idle" => {
            let (options, positional) = split(rest, &["duration", "warmup", "recd"])?;
            if let Some(extra) = positional.first() {
                return Err(format!("unexpected argument `{extra}`"));
            }
            Ok(Command::Idle(IdleArgs {
                duration: number(&options, "duration", Some(60), 5..=3600)?.unwrap_or(60),
                warmup: number(&options, "warmup", Some(5), 0..=600)?.unwrap_or(5),
                recd: path(&options, "recd"),
            }))
        }
        other => Err(format!("unknown command `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(line: &str) -> Result<Command, String> {
        let args: Vec<String> = line.split_whitespace().map(String::from).collect();
        parse(&args)
    }

    fn run(line: &str) -> RunArgs {
        match parse_str(line).unwrap() {
            Command::Run(args) => args,
            other => panic!("not a run command: {other:?}"),
        }
    }

    #[test]
    fn help_forms() {
        assert_eq!(parse_str(""), Ok(Command::Help));
        assert_eq!(parse_str("--help"), Ok(Command::Help));
        assert_eq!(parse_str("help"), Ok(Command::Help));
        assert_eq!(parse_str("run --help"), Ok(Command::Help));
    }

    #[test]
    fn run_defaults() {
        let a = run("run --process Game.exe");
        assert_eq!(a.process, "Game.exe");
        assert_eq!((a.duration, a.runs, a.warmup), (60, 3, 5));
        assert_eq!(a.save_every, None);
        assert_eq!(a.on_warmup, None);
        assert_eq!(a.presentmon, None);
        assert_eq!(a.recd, None);
        assert_eq!(a.label, None);
        assert_eq!(a.out, PathBuf::from("bench/results"));
    }

    #[test]
    fn run_all_options_in_both_spellings() {
        let a = run(
            "run --process Game.exe --presentmon C:/pm/PresentMon.exe --duration 30 \
             --runs=5 --warmup 10 --on-warmup=90 --recd D:/rec/recd.exe --save-every=7 --label=my-test --out results",
        );
        assert_eq!(a.presentmon, Some(PathBuf::from("C:/pm/PresentMon.exe")));
        assert_eq!((a.duration, a.runs, a.warmup), (30, 5, 10));
        assert_eq!(a.recd, Some(PathBuf::from("D:/rec/recd.exe")));
        assert_eq!(a.save_every, Some(7));
        assert_eq!(a.on_warmup, Some(90));
        assert_eq!(a.label.as_deref(), Some("my-test"));
        assert_eq!(a.out, PathBuf::from("results"));
    }

    #[test]
    fn run_rejects_bad_input() {
        assert!(parse_str("run").unwrap_err().contains("--process"));
        assert!(parse_str("run --process G.exe --runs 0").is_err());
        assert!(parse_str("run --process G.exe --runs many").is_err());
        assert!(parse_str("run --process G.exe --duration 1").is_err());
        assert!(parse_str("run --process G.exe --bogus 1").is_err());
        assert!(
            parse_str("run --process")
                .unwrap_err()
                .contains("needs a value")
        );
        assert!(parse_str("run --process G.exe extra").is_err());
        assert!(
            parse_str("run --process G.exe --duration 30 --save-every 30").is_err(),
            "a save interval as long as the capture never fires"
        );
    }

    #[test]
    fn analyze_takes_files() {
        match parse_str("analyze a.csv b.csv --process Game.exe").unwrap() {
            Command::Analyze(a) => {
                assert_eq!(a.files, [PathBuf::from("a.csv"), PathBuf::from("b.csv")]);
                assert_eq!(a.process.as_deref(), Some("Game.exe"));
            }
            other => panic!("{other:?}"),
        }
        assert!(parse_str("analyze").is_err());
    }

    #[test]
    fn idle_options() {
        assert_eq!(
            parse_str("idle").unwrap(),
            Command::Idle(IdleArgs {
                duration: 60,
                warmup: 5,
                recd: None
            })
        );
        assert_eq!(
            parse_str("idle --duration 10 --recd x/recd.exe").unwrap(),
            Command::Idle(IdleArgs {
                duration: 10,
                warmup: 5,
                recd: Some(PathBuf::from("x/recd.exe"))
            })
        );
        assert!(parse_str("idle now").is_err());
    }

    #[test]
    fn unknown_command() {
        assert!(parse_str("record").unwrap_err().contains("unknown command"));
    }
}
