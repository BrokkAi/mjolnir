//! A relay proxy whose worker dies must exit, even while its client still
//! holds the proxy's stdin open.
//!
//! Before the fix the proxy finished forwarding and then hung in Tokio
//! runtime shutdown: the global stdin reader owns a blocking pool thread
//! whose read cannot be cancelled, and the controller does not close the
//! proxy's stdin while it waits for a reply. The proxy stayed alive with no
//! worker behind it, its stdout never closed, and the controller waited out
//! its full relay timeout before noticing.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const ACCEPT_WAIT: Duration = Duration::from_secs(10);
const EXIT_WAIT: Duration = Duration::from_secs(10);

/// The proxy under test is a real process. Kill it however the test ends.
struct ReapChild(Child);

impl Drop for ReapChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn accept_within(listener: &UnixListener, wait: Duration) -> UnixStream {
    listener
        .set_nonblocking(true)
        .expect("poll the fake worker listener");
    let deadline = Instant::now() + wait;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("read the accepted proxy connection");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "relay proxy never connected to control.sock"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("accept the relay proxy connection: {error}"),
        }
    }
}

#[test]
fn a_proxy_exits_when_its_worker_dies_while_its_client_holds_stdin_open() {
    let root = tempfile::tempdir().expect("create a proxy test root");
    let listener =
        UnixListener::bind(root.path().join("control.sock")).expect("bind a fake control socket");

    let mut command = Command::new(env!("CARGO_BIN_EXE_mj"));
    command
        .args(["worker", "proxy", "--root"])
        .arg(root.path())
        .env("MJ_DATA_DIR", root.path().join("data"))
        .env("MJ_CONFIG_DIR", root.path().join("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut proxy = ReapChild(command.spawn().expect("start the relay proxy"));
    let mut client_stdin = proxy.0.stdin.take().expect("relay proxy stdin");
    // Hold the read end open: a closed stdout would end the proxy for the
    // wrong reason.
    let _client_stdout = proxy.0.stdout.take().expect("relay proxy stdout");

    let mut worker = accept_within(&listener, ACCEPT_WAIT);
    worker
        .set_read_timeout(Some(ACCEPT_WAIT))
        .expect("bound the fake worker read");
    // The proxy forwards nothing until its client speaks first.
    client_stdin
        .write_all(b"hello\n")
        .expect("send the first request");
    client_stdin.flush().expect("flush the first request");
    let mut request = [0_u8; 6];
    worker
        .read_exact(&mut request)
        .expect("the proxy forwards the first request");
    assert_eq!(&request, b"hello\n");

    // The worker dies. Its client does not: it still owns the proxy's stdin,
    // waiting for a response that will never arrive.
    drop(worker);
    drop(listener);

    let deadline = Instant::now() + EXIT_WAIT;
    let status = loop {
        if let Some(status) = proxy.0.try_wait().expect("poll the relay proxy") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "relay proxy stayed alive after its worker closed the control socket"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        status.success(),
        "a proxy that outlives its worker exits cleanly, got {status}"
    );
    drop(client_stdin);
}
