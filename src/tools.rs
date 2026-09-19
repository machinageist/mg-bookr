// Author: Jeff
// Date: 2026-09-19
// Description: Run a helper program (pdfinfo, pdftoppm, bsdtar, ffprobe, ffmpeg) safely
// Notes: Always an argv list, never a shell, so a file name can never become a command. Each run
//        has a time limit and an output cap: a broken or hostile book must not hang a scan or
//        fill memory. A tool that is not installed is a plain error the caller can shrug off

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const POLL: Duration = Duration::from_millis(20);

// Run `argv`, wait at most `timeout`, keep at most `max_bytes` of stdout; stderr is dropped
pub fn run(argv: &[&str], timeout: Duration, max_bytes: usize) -> Result<Vec<u8>> {
    let (program, args) = argv.split_first().context("no program to run")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("{program} is not available"))?;
    let mut stdout = child.stdout.take().context("no output pipe")?;
    // read in a thread so a chatty tool cannot block on a full pipe while we wait
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = (&mut stdout)
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut out);
        out
    });
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
    let out = reader
        .join()
        .map_err(|_| anyhow::anyhow!("reading {program}'s output failed"))?;
    if out.len() > max_bytes {
        bail!("{program} printed more than {max_bytes} bytes")
    }
    if !status.success() {
        bail!("{program} failed ({status})")
    }
    Ok(out)
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
}
