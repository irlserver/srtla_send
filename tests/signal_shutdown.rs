//! Signal shutdown integration tests (Unix).
//!
//! These spawn the real `srtla_send` binary and pin that SIGTERM and SIGINT
//! produce a prompt, clean exit (code 0) rather than death by the default
//! signal action, which a supervisor reports as a failure.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_srtla_send");
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll well past `PROMPT_EXIT` so a slow exit is reported as too slow rather
/// than as "never exited".
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);
const PROMPT_EXIT: Duration = Duration::from_secs(2);

fn free_udp_port() -> u16 {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind ephemeral udp");
    sock.local_addr().unwrap().port()
}

struct SenderProc {
    child: Child,
    logs: Arc<Mutex<String>>,
}

impl SenderProc {
    fn spawn(args: &[&str]) -> Self {
        let mut child = Command::new(BIN)
            .args(args)
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn srtla_send");

        // tracing writes to stdout; panics/anyhow errors land on stderr. Pump both
        // into one buffer so log assertions see everything the binary emitted.
        let logs = Arc::new(Mutex::new(String::new()));
        let streams: [Box<dyn std::io::Read + Send>; 2] = [
            Box::new(child.stdout.take().expect("capture stdout")),
            Box::new(child.stderr.take().expect("capture stderr")),
        ];
        for stream in streams {
            let sink = logs.clone();
            thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    let mut buf = sink.lock().unwrap();
                    buf.push_str(&line);
                    buf.push('\n');
                }
            });
        }

        Self { child, logs }
    }

    fn logs(&self) -> String {
        self.logs.lock().unwrap().clone()
    }

    fn wait_for_log(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.logs().contains(needle) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Block until the sender has installed its handler for `signum`.
    ///
    /// The handlers are registered after the "listening for SRT" line is
    /// logged, so signalling straight off that line can still hit the default
    /// action and kill the process. `SigCgt` in `/proc/<pid>/status` is the
    /// kernel's own record of caught signals, so it closes that window
    /// without a guessed sleep.
    #[cfg(target_os = "linux")]
    fn wait_for_handler(&self, signum: u32, timeout: Duration) -> bool {
        let path = format!("/proc/{}/status", self.child.id());
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let caught = std::fs::read_to_string(&path).ok().and_then(|status| {
                let mask = status.lines().find_map(|l| l.strip_prefix("SigCgt:"))?;
                u64::from_str_radix(mask.trim(), 16).ok()
            });
            if caught.is_some_and(|mask| mask & (1 << (signum - 1)) != 0) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// No portable way to observe the handler off Linux; give startup a moment.
    #[cfg(not(target_os = "linux"))]
    fn wait_for_handler(&self, _signum: u32, _timeout: Duration) -> bool {
        thread::sleep(Duration::from_millis(500));
        true
    }

    fn signal(&self, sig: &str) {
        let status = Command::new("kill")
            .args([format!("-{sig}"), self.child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -{sig} failed");
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                // A signal death has no exit code; report it as -1 so it can
                // never be mistaken for the clean 0 these tests require.
                Ok(Some(status)) => return Some(status.code().unwrap_or(-1)),
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for SenderProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn assert_signal_exits_cleanly(sig: &str, signum: u32) {
    let dir = tempfile::tempdir().unwrap();
    let ips = dir.path().join("ips.txt");
    std::fs::write(&ips, "127.0.0.1\n").expect("write ips file");
    let port = free_udp_port().to_string();
    let mut proc = SenderProc::spawn(&[port.as_str(), "127.0.0.1", "9999", ips.to_str().unwrap()]);

    assert!(
        proc.wait_for_log("listening for SRT", START_TIMEOUT),
        "sender never started; logs:\n{}",
        proc.logs()
    );
    assert!(
        proc.wait_for_handler(signum, START_TIMEOUT),
        "sender never installed a SIG{sig} handler; logs:\n{}",
        proc.logs()
    );

    let t0 = Instant::now();
    proc.signal(sig);
    let code = proc.wait_exit(EXIT_TIMEOUT);
    let elapsed = t0.elapsed();

    assert_eq!(
        code,
        Some(0),
        "SIG{sig} must exit 0; logs:\n{}",
        proc.logs()
    );
    assert!(
        elapsed < PROMPT_EXIT,
        "SIG{sig} must exit promptly (< {PROMPT_EXIT:?}); took {elapsed:?}"
    );
}

#[test]
fn sigterm_exits_cleanly_and_promptly() {
    assert_signal_exits_cleanly("TERM", 15);
}

#[test]
fn sigint_exits_cleanly_and_promptly() {
    assert_signal_exits_cleanly("INT", 2);
}
