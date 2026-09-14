//! A relay proxy must reach a worker whose root is longer than `sun_path`.
//!
//! macOS caps a Unix socket address at 104 bytes. Worker roots under
//! `<data_dir>/workers/<32-hex session id>/` pass that routinely, and the
//! proxy used to exit with "path must be shorter than SUN_LEN" instead of
//! connecting.

#![cfg(unix)]

use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use mj_core::local_sockets::{bind_unix_listener, unix_socket_path_limit};

const ACCEPT_WAIT: Duration = Duration::from_secs(10);

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

fn accept_within(listener: &UnixListener, wait: Duration, proxy: &mut Child) -> UnixStream {
    listener
        .set_nonblocking(true)
        .expect("poll the fake worker listener");
    let deadline = Instant::now() + wait;
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if let Some(status) = proxy.try_wait().expect("poll the relay proxy") {
                    panic!("the relay proxy exited before connecting: {status}");
                }
                assert!(
                    Instant::now() < deadline,
                    "the relay proxy never connected to a long control.sock path"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => panic!("accept the relay proxy connection: {error}"),
        }
    }
}

#[test]
fn a_proxy_connects_to_a_worker_root_longer_than_sun_path() {
    let temp = tempfile::tempdir().expect("create a proxy test root");
    let mut root = temp.path().to_owned();
    while root.join("control.sock").as_os_str().len() <= 120 {
        root.push("nested-worker-root-component");
    }
    std::fs::create_dir_all(&root).expect("create the nested worker root");
    let socket = root.join("control.sock");
    assert!(
        socket.as_os_str().len() > unix_socket_path_limit(),
        "the test root must be long enough to need the relative-name connect"
    );

    let listener = bind_unix_listener(&socket).expect("bind a fake control socket");

    let mut command = Command::new(env!("CARGO_BIN_EXE_mj-worker"));
    command
        .args(["worker", "proxy", "--root"])
        .arg(&root)
        .env("MJ_DATA_DIR", temp.path().join("data"))
        .env("MJ_CONFIG_DIR", temp.path().join("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut proxy = ReapChild(command.spawn().expect("start the relay proxy"));

    let worker = accept_within(&listener, ACCEPT_WAIT, &mut proxy.0);
    drop(worker);
}
