//! `rec`: control the rec background service from a terminal, script or
//! macro tool.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

use rec_core::config;
use rec_core::ipc::{self, RecorderState, Request, Response};

const USAGE: &str = "\
rec - control the rec background recorder

usage:
  rec status            show what is being recorded
  rec save [SECONDS]    save a clip (default length from config)
  rec mark              add a marker at the current moment
  rec pause | resume    pause or resume buffering
  rec reload            reload the config file
  rec quit              stop the background service
  rec config            print the config file path";

/// Windows `ERROR_PIPE_BUSY`: every pipe instance is serving another client.
const ERROR_PIPE_BUSY: i32 = 231;

#[derive(Debug, PartialEq)]
enum Command {
    Help,
    ConfigPath,
    Send(Request),
}

fn parse(args: &[String]) -> Result<Command, String> {
    let mut args = args.iter().map(String::as_str);
    let command = match args.next() {
        None | Some("help" | "-h" | "--help") => Command::Help,
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

fn connect() -> io::Result<File> {
    let name = ipc::pipe_name();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match OpenOptions::new().read(true).write(true).open(&name) {
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            result => return result,
        }
    }
}

fn send(request: &Request) -> Result<Response, String> {
    let mut pipe = connect().map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => "rec is not running (start recd first)".to_string(),
        _ => format!("could not connect to rec: {e}"),
    })?;
    pipe.write_all(ipc::encode_line(request).as_bytes())
        .and_then(|()| pipe.flush())
        .map_err(|e| format!("could not send request: {e}"))?;

    let mut line = String::new();
    BufReader::new(&pipe)
        .read_line(&mut line)
        .map_err(|e| format!("could not read response: {e}"))?;
    ipc::decode_line(&line).map_err(|e| format!("invalid response from rec: {e}"))
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
        Ok(Command::Send(request)) => match send(&request) {
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
}
