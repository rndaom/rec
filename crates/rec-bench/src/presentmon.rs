//! Finding and running PresentMon.

use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const RELEASES_URL: &str = "https://github.com/GameTechDev/PresentMon/releases";

/// ETW session name. A fixed name plus `--stop_existing_session` means a
/// previous interrupted bench cannot leave a stale session that blocks this one,
/// and lets [`terminate_session`] clean up after a PresentMon that was killed.
const SESSION_NAME: &str = "rec-bench";

/// Index of the best PresentMon executable among `names` (file names in one
/// directory): `PresentMon.exe` first, then the highest `PresentMon-*.exe`
/// (releases are named like `PresentMon-2.3.0-x64.exe`).
pub fn pick(names: &[String]) -> Option<usize> {
    let lower: Vec<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
    if let Some(i) = lower.iter().position(|n| n == "presentmon.exe") {
        return Some(i);
    }
    lower
        .iter()
        .enumerate()
        .filter(|(_, n)| n.starts_with("presentmon-") && n.ends_with(".exe"))
        .max_by(|a, b| a.1.cmp(b.1))
        .map(|(i, _)| i)
}

fn find_in(dir: &Path) -> Option<PathBuf> {
    let names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    pick(&names).map(|i| dir.join(&names[i]))
}

/// The message printed when PresentMon cannot be found.
pub fn not_found_message() -> String {
    format!(
        "PresentMon was not found.\n\
         Download the PresentMon console application from {RELEASES_URL}, then either put it \
         next to rec-bench.exe or on PATH, or pass --presentmon <path>."
    )
}

/// Resolves the PresentMon executable: `--presentmon`, else next to
/// `rec-bench.exe`, else on `PATH`.
pub fn locate(explicit: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok(path.to_owned())
        } else {
            Err(format!(
                "--presentmon {} does not exist.\n{}",
                path.display(),
                not_found_message()
            ))
        };
    }
    let own_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_owned));
    let path_dirs = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    own_dir
        .into_iter()
        .chain(path_dirs)
        .find_map(|dir| find_in(&dir))
        .ok_or_else(not_found_message)
}

/// Command-line arguments for one timed capture of `process`.
pub fn args(process: &str, csv: &Path, duration_secs: u32) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let mut flag = |name: &str, value: Option<OsString>| {
        args.push(name.into());
        args.extend(value);
    };
    flag("--process_name", Some(process.into()));
    flag("--output_file", Some(csv.into()));
    flag("--timed", Some(duration_secs.to_string().into()));
    flag("--terminate_after_timed", None);
    flag("--stop_existing_session", None);
    flag("--session_name", Some(SESSION_NAME.into()));
    flag("--no_console_stats", None);
    // Raw QPC counter values, so frames line up with our own QPC timestamps.
    flag("--qpc_time", None);
    args
}

/// How long `--terminate_existing_session` may take before it is given up on.
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Command-line arguments that make PresentMon stop the leftover ETW session
/// and exit.
pub fn terminate_args() -> Vec<OsString> {
    vec![
        "--terminate_existing_session".into(),
        "--session_name".into(),
        SESSION_NAME.into(),
    ]
}

/// Stops the real-time ETW session a killed PresentMon leaves running.
///
/// `TerminateProcess` gives PresentMon no chance to close its session, and a
/// real-time ETW session outlives its consumer. PresentMon can end it by
/// name. The caller treats a failure as a warning: the next capture starts
/// with `--stop_existing_session` anyway.
pub fn terminate_session(exe: &Path) -> Result<(), String> {
    let mut child = Command::new(exe)
        .args(terminate_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", exe.display()))?;
    let deadline = Instant::now() + TERMINATE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("PresentMon exited with {status}")),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "PresentMon did not finish within {} s and was stopped",
                    TERMINATE_TIMEOUT.as_secs()
                ));
            }
            Err(e) => return Err(format!("could not wait for PresentMon: {e}")),
        }
    }
}

/// Starts a timed capture. PresentMon's own output goes to `log` (it is small
/// with `--no_console_stats`, but an unread pipe could still block it).
pub fn spawn(
    exe: &Path,
    process: &str,
    csv: &Path,
    duration_secs: u32,
    log: &Path,
) -> Result<Child, String> {
    let log_file =
        File::create(log).map_err(|e| format!("could not create {}: {e}", log.display()))?;
    let log_err = log_file
        .try_clone()
        .map_err(|e| format!("could not duplicate the log handle: {e}"))?;
    Command::new(exe)
        .args(args(process, csv, duration_secs))
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_err)
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", exe.display()))
}

/// Whether PresentMon's output looks like a permissions problem.
pub fn looks_like_access_error(log: &str) -> bool {
    let log = log.to_ascii_lowercase();
    [
        "access",
        "denied",
        "privilege",
        "administrator",
        "elevat",
        "performance log users",
    ]
    .iter()
    .any(|needle| log.contains(needle))
}

pub const ACCESS_HINT: &str = "PresentMon needs administrator rights or membership of the \
     \"Performance Log Users\" group to read ETW events. Run rec-bench from an elevated \
     terminal, or add your account to that group and sign out and in again.";

/// Explains a PresentMon run that failed or produced no file.
pub fn explain_failure(exit_code: Option<i32>, log: &str) -> String {
    let mut message = match exit_code {
        Some(0) => "PresentMon finished but wrote no CSV file".to_string(),
        Some(code) => format!("PresentMon exited with code {code}"),
        None => "PresentMon was terminated".to_string(),
    };
    let log = log.trim();
    if !log.is_empty() {
        let tail: Vec<&str> = log.lines().rev().take(6).collect();
        message.push_str(":\n  ");
        message.push_str(&tail.into_iter().rev().collect::<Vec<_>>().join("\n  "));
    }
    if looks_like_access_error(log) || (log.is_empty() && exit_code != Some(0)) {
        message.push('\n');
        message.push_str(ACCESS_HINT);
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_name_wins() {
        let n = names(&["PresentMon-2.3.0-x64.exe", "presentmon.exe", "other.exe"]);
        assert_eq!(pick(&n), Some(1));
    }

    #[test]
    fn highest_versioned_name_is_used() {
        let n = names(&[
            "notes.txt",
            "PresentMon-2.1.0-x64.exe",
            "PresentMon-2.3.0-x64.exe",
        ]);
        assert_eq!(pick(&n), Some(2));
    }

    #[test]
    fn unrelated_files_do_not_match() {
        let n = names(&["PresentMonService.exe", "PresentMon-2.3.0-x64.msi", "a.exe"]);
        assert_eq!(pick(&n), None);
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn command_line_matches_the_spec() {
        let a: Vec<String> = args("Game.exe", Path::new("out.csv"), 60)
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        assert_eq!(
            a,
            [
                "--process_name",
                "Game.exe",
                "--output_file",
                "out.csv",
                "--timed",
                "60",
                "--terminate_after_timed",
                "--stop_existing_session",
                "--session_name",
                "rec-bench",
                "--no_console_stats",
                "--qpc_time",
            ]
        );
    }

    #[test]
    fn terminate_command_line_names_the_capture_session() {
        let a: Vec<String> = terminate_args()
            .into_iter()
            .map(|s| s.into_string().unwrap())
            .collect();
        assert_eq!(
            a,
            [
                "--terminate_existing_session",
                "--session_name",
                "rec-bench"
            ]
        );
    }

    #[test]
    fn missing_explicit_path_is_reported_with_the_download_link() {
        let err = locate(Some(Path::new("Z:/definitely/not/here/PresentMon.exe"))).unwrap_err();
        assert!(
            err.contains(RELEASES_URL) && err.contains("does not exist"),
            "{err}"
        );
    }

    #[test]
    fn access_errors_get_the_permissions_hint() {
        let msg = explain_failure(
            Some(1),
            "error: failed to start trace session: Access is denied.",
        );
        assert!(msg.contains("Performance Log Users"), "{msg}");
        let msg = explain_failure(Some(1), "error: unknown option");
        assert!(!msg.contains("Performance Log Users"), "{msg}");
        // No output at all and a failure exit: permissions are the usual cause.
        assert!(explain_failure(Some(1), "").contains("Performance Log Users"));
        assert!(!explain_failure(Some(0), "").contains("Performance Log Users"));
    }
}
