//! `rec-bench`: measure what recording costs a game.
//!
//! * `run` alternates PresentMon captures with `recd` off and on and
//!   compares frame rate and frame pacing against the budget in
//!   `docs/decisions.md` (D11).
//! * `idle` measures `recd`'s cost with no game running.
//! * `analyze` prints statistics for PresentMon CSVs captured by hand.
//!
//! See `bench/README.md`.

mod bench;
mod budget;
mod capture;
mod cli;
mod presentmon;
mod recd;
mod report;
mod sampler;
mod stats;
mod sys;

use std::process::ExitCode;

use cli::Command;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match cli::parse(&args) {
        Ok(Command::Help) => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Command::Run(args)) => bench::run(&args),
        Ok(Command::Analyze(args)) => bench::analyze(&args),
        Ok(Command::Idle(args)) => bench::idle(&args),
        Err(message) => {
            eprintln!("rec-bench: {message}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("rec-bench: {message}");
            ExitCode::FAILURE
        }
    }
}
