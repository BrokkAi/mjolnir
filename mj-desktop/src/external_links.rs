//! Own external browser launches independently of the native event loop.

use std::io;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
enum State {
    Idle,
    Running,
    Closed,
}

pub(super) struct ExternalLinks {
    state: Arc<Mutex<State>>,
}

impl ExternalLinks {
    pub(super) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::Idle)),
        }
    }

    pub(super) fn launch(
        &self,
        launch: impl FnOnce() -> Result<(), String> + Send + 'static,
        report_error: impl FnOnce(String) + Send + 'static,
    ) -> io::Result<()> {
        {
            let mut state = self.state.lock().unwrap();
            match *state {
                State::Idle => *state = State::Running,
                State::Running => {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "an external browser launch is still running",
                    ));
                }
                State::Closed => {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the desktop window is closed",
                    ));
                }
            }
        }
        // The guard also releases admission if spawning the thread fails.
        let running = RunningLaunch(self.state.clone());
        std::thread::Builder::new()
            .name("mj-desktop-browser".to_owned())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(launch));
                let error = match outcome {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error),
                    Err(_) => Some("external browser launcher panicked".to_owned()),
                };
                if let Some(error) = error {
                    report_error(error);
                }
                drop(running);
            })?;
        Ok(())
    }
}

impl Drop for ExternalLinks {
    fn drop(&mut self) {
        *self.state.lock().unwrap() = State::Closed;
        // webbrowser can wait for a terminal browser until the user quits it.
        // A single std thread may finish independently; unlike a Tokio blocking
        // task, it cannot hold runtime shutdown or process exit open.
    }
}

struct RunningLaunch(Arc<Mutex<State>>);

impl Drop for RunningLaunch {
    fn drop(&mut self) {
        let mut state = self.0.lock().unwrap();
        if matches!(*state, State::Running) {
            *state = State::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn pending_launch_does_not_block_navigation_or_window_close() {
        let links = ExternalLinks::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        links
            .launch(
                move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Err("launcher finished after close".to_owned())
                },
                move |error| finished_tx.send(error).unwrap(),
            )
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(
            links.launch(|| Ok(()), |_| {}).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        let state = links.state.clone();
        let (closed_tx, closed_rx) = mpsc::channel();
        let close = std::thread::spawn(move || {
            drop(links);
            closed_tx.send(()).unwrap();
        });
        let closed = closed_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        closed.expect("closing must not wait for the browser launcher");
        close.join().unwrap();
        assert_eq!(
            finished_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "launcher finished after close"
        );
        assert!(matches!(*state.lock().unwrap(), State::Closed));
    }

    #[test]
    fn launcher_panics_are_reported() {
        let links = ExternalLinks::new();
        let (error_tx, error_rx) = mpsc::channel();
        links
            .launch(
                || panic!("broken launcher"),
                move |error| error_tx.send(error).unwrap(),
            )
            .unwrap();
        assert_eq!(
            error_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "external browser launcher panicked"
        );
    }
}
