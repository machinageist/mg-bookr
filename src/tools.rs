// Author: Jeff
// Date: 2026-09-19
// Description: Run a helper program (pdfinfo, pdftoppm, bsdtar, ffprobe, ffmpeg) safely
// Notes: Always an argv list, never a shell, so a file name can never become a command. Each run
//        has a time limit and an output cap: a broken or hostile book must not hang a scan or
//        fill memory. A tool that is not installed is a plain error the caller can shrug off

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const POLL: Duration = Duration::from_millis(20);

// What a finished run left behind
pub struct Output {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

// Run `argv` and fail unless it succeeds; see run_status for the limits
pub fn run(argv: &[&str], timeout: Duration, max_bytes: usize) -> Result<Vec<u8>> {
    let out = run_status(argv, timeout, max_bytes)?;
    if !out.success {
        bail!("{} failed", argv.first().copied().unwrap_or("program"))
    }
    Ok(out.stdout)
}

// Read a pipe to its end in a thread, keeping at most one byte over `max` (to spot overflow),
// so a chatty tool never blocks on a full pipe while we wait for it
fn drain(pipe: impl Read + Send + 'static, max: usize) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = pipe.take(max as u64 + 1).read_to_end(&mut out);
        out
    })
}

// Run `argv`, wait at most `timeout`, keep at most `max_bytes` of stdout and of stderr.
// Both streams come back for tools that answer on either: mg-vault refuses on stderr
pub fn run_status(argv: &[&str], timeout: Duration, max_bytes: usize) -> Result<Output> {
    let (program, args) = argv.split_first().context("no program to run")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("{program} is not available"))?;
    let stdout = drain(child.stdout.take().context("no output pipe")?, max_bytes);
    let stderr = drain(child.stderr.take().context("no error pipe")?, max_bytes);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} took longer than {}s", timeout.as_secs())
        }
        std::thread::sleep(POLL);
    };
    let joined = |reader: JoinHandle<Vec<u8>>| {
        reader
            .join()
            .map_err(|_| anyhow::anyhow!("reading {program}'s output failed"))
    };
    let (stdout, stderr) = (joined(stdout)?, joined(stderr)?);
    if stdout.len() > max_bytes || stderr.len() > max_bytes {
        bail!("{program} printed more than {max_bytes} bytes")
    }
    Ok(Output {
        success: status.success(),
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_returned_and_limits_hold() {
        assert_eq!(
            run(&["printf", "hi"], Duration::from_secs(5), 10).unwrap(),
            b"hi"
        );
        assert!(
            run(&["printf", "0123456789abc"], Duration::from_secs(5), 10).is_err(),
            "over the cap"
        );
        assert!(
            run(&["sleep", "5"], Duration::from_millis(200), 10).is_err(),
            "over the time"
        );
        assert!(run(&["false"], Duration::from_secs(5), 10).is_err());
        assert!(run(&["mg-bookr-no-such-tool"], Duration::from_secs(5), 10).is_err());
        // a name with shell characters is just an argument, never interpreted
        assert_eq!(
            run(&["printf", "%s", "$(id);rm"], Duration::from_secs(5), 64).unwrap(),
            b"$(id);rm"
        );
    }

    #[test]
    fn a_failing_run_still_returns_what_it_said_on_both_streams() {
        let script = "printf out; printf err >&2; exit 3";
        let out = run_status(&["sh", "-c", script], Duration::from_secs(5), 10).unwrap();
        assert!(!out.success);
        assert_eq!(out.stdout, b"out");
        assert_eq!(out.stderr, b"err");
        let noisy = "head -c 20 /dev/zero >&2";
        assert!(
            run_status(&["sh", "-c", noisy], Duration::from_secs(5), 10).is_err(),
            "stderr has the same cap"
        );
    }
}
