// Author: Jeff
// Date: 2026-09-19
// Description: A small client for mpv's JSON IPC socket
// Notes: mpv speaks one JSON object per line. A reply carries the request_id we sent; events
//        ({"event": …}) can arrive between replies at any time. `command` waits for its own
//        reply and keeps the events it passes on the way, so whoever reads events next still
//        sees them in order. Every read has a time limit, so a stuck mpv can never hang a
//        command, and a partial line left by a timeout is kept for the next read

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

// how long a command waits for mpv's reply
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
// a reply line longer than this is not mpv talking to us
const MAX_LINE: usize = 1024 * 1024;

pub struct Mpv {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    // events read while waiting for a reply, oldest first
    events: VecDeque<Value>,
    // the start of a line a timed-out read left behind
    partial: Vec<u8>,
}

impl Mpv {
    // Connect to a running mpv's socket
    pub fn connect(path: &Path) -> Result<Self> {
        let writer = UnixStream::connect(path)
            .with_context(|| format!("nothing is listening at {}", path.display()))?;
        writer.set_write_timeout(Some(REPLY_TIMEOUT))?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Mpv {
            reader,
            writer,
            next_id: 1,
            events: VecDeque::new(),
            partial: Vec::new(),
        })
    }

    // Send one command and return its data (null when it has none)
    pub fn command(&mut self, args: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = json!({ "command": args, "request_id": id }).to_string();
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .context("mpv went away")?;
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("mpv did not answer")
            }
            let Some(message) = self.read_message(left)? else {
                continue;
            };
            if message.get("request_id").and_then(Value::as_u64) != Some(id) {
                // an event, or a reply to someone else's request on this socket
                if message.get("event").is_some() {
                    self.events.push_back(message);
                }
                continue;
            }
            return match message.get("error").and_then(Value::as_str) {
                Some("success") => Ok(message.get("data").cloned().unwrap_or(Value::Null)),
                Some(error) => bail!("mpv: {error}"),
                None => bail!("mpv answered without a status"),
            };
        }
    }

    // A property's value. "property unavailable" (nothing loaded yet) and "property not found"
    // (a user-data key nobody has set yet) both read as null
    pub fn get(&mut self, name: &str) -> Result<Value> {
        match self.command(json!(["get_property", name])) {
            Err(e)
                if matches!(
                    e.to_string().as_str(),
                    "mpv: property unavailable" | "mpv: property not found"
                ) =>
            {
                Ok(Value::Null)
            }
            other => other,
        }
    }

    // Set a property
    pub fn set(&mut self, name: &str, value: Value) -> Result<()> {
        self.command(json!(["set_property", name, value]))
            .map(|_| ())
    }

    // Ask for a property-change event whenever `name` changes (and once now)
    pub fn observe(&mut self, id: u64, name: &str) -> Result<()> {
        self.command(json!(["observe_property", id, name]))
            .map(|_| ())
    }

    // The next event, waiting at most `wait`; None when none came, an error when mpv is gone
    pub fn next_event(&mut self, wait: Duration) -> Result<Option<Value>> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            match self.read_message(left)? {
                Some(message) if message.get("event").is_some() => return Ok(Some(message)),
                // a stray reply: nobody is waiting for it any more
                Some(_) => continue,
                None => return Ok(None),
            }
        }
    }

    // Read one JSON line within `wait`: Some(message), None on timeout, an error when mpv is gone
    fn read_message(&mut self, wait: Duration) -> Result<Option<Value>> {
        self.reader.get_ref().set_read_timeout(Some(wait))?;
        match self.reader.read_until(b'\n', &mut self.partial) {
            Ok(0) => bail!("mpv went away"),
            Ok(_) if !self.partial.ends_with(b"\n") => bail!("mpv went away mid-line"),
            Ok(_) => {
                let line = std::mem::take(&mut self.partial);
                // one bad line is skipped, not fatal
                Ok(serde_json::from_slice(&line).ok())
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if self.partial.len() > MAX_LINE {
                    bail!("mpv sent a line longer than {MAX_LINE} bytes")
                }
                Ok(None)
            }
            Err(e) => Err(e).context("reading from mpv"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::thread;

    // A fake mpv: answers each request from `answer`, after sending `before` events
    fn fake(
        answer: impl Fn(&Value) -> Value + Send + 'static,
        before: Vec<Value>,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mpv.sock");
        let listener = UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut out = stream.try_clone().unwrap();
            let lines = BufReader::new(stream).lines();
            for line in lines {
                let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
                for event in &before {
                    writeln!(out, "{event}").unwrap();
                }
                let mut reply = answer(&request["command"]);
                reply["request_id"] = request["request_id"].clone();
                writeln!(out, "{reply}").unwrap();
            }
        });
        (dir, path)
    }

    #[test]
    fn a_reply_is_matched_by_id_and_events_on_the_way_are_kept_in_order() {
        let events = vec![
            json!({"event":"property-change","id":1,"name":"pause","data":true}),
            json!({"event":"seek"}),
        ];
        let (_dir, path) = fake(|_| json!({"error":"success","data":42.5}), events);
        let mut mpv = Mpv::connect(&path).unwrap();
        assert_eq!(mpv.get("time-pos").unwrap(), json!(42.5));
        let first = mpv.next_event(Duration::from_millis(50)).unwrap().unwrap();
        assert_eq!(first["name"], "pause");
        assert_eq!(
            mpv.next_event(Duration::from_millis(50)).unwrap().unwrap()["event"],
            "seek"
        );
        assert!(
            mpv.next_event(Duration::from_millis(50)).unwrap().is_none(),
            "nothing more"
        );
    }

    #[test]
    fn errors_are_plain_and_an_unavailable_property_is_null() {
        let (_dir, path) = fake(
            |c| match c[0].as_str() {
                Some("get_property") if c[1] == "user-data/x" => {
                    json!({"error":"property not found"})
                }
                Some("get_property") => json!({"error":"property unavailable"}),
                _ => json!({"error":"invalid parameter"}),
            },
            vec![],
        );
        let mut mpv = Mpv::connect(&path).unwrap();
        assert_eq!(mpv.get("time-pos").unwrap(), Value::Null);
        assert_eq!(mpv.get("user-data/x").unwrap(), Value::Null);
        assert_eq!(
            mpv.set("speed", json!(9)).unwrap_err().to_string(),
            "mpv: invalid parameter"
        );
    }

    #[test]
    fn a_silent_mpv_times_out_and_a_closed_one_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mpv.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // read the request, say half a line, then hang up
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf).unwrap();
            stream.write_all(b"{\"event\":").unwrap();
            thread::sleep(Duration::from_millis(2500));
        });
        let mut mpv = Mpv::connect(&path).unwrap();
        assert_eq!(
            mpv.command(json!(["get_property", "pause"]))
                .unwrap_err()
                .to_string(),
            "mpv did not answer"
        );
        server.join().unwrap();
        assert!(
            mpv.next_event(Duration::from_millis(200)).is_err(),
            "the socket closed"
        );
        assert!(Mpv::connect(&dir.path().join("none.sock")).is_err());
    }
}
