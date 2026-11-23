//! A modal terminal whose network I/O is owned by a cancellable task.
use super::DashboardContext;
use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use mj_client::daemon::login_terminal::{TerminalInput, TerminalOutput};
use mj_tui::login_terminal::LoginPane;
use tokio::sync::mpsc;
use tokio_util::task::AbortOnDropHandle;

pub(super) enum Update {
    Ready,
    Frame(TerminalOutput),
    Failed(String),
}

async fn connection(
    profile: String,
    cols: u16,
    rows: u16,
    mut input: mpsc::UnboundedReceiver<TerminalInput>,
    updates: mpsc::Sender<Update>,
) -> Result<()> {
    let client = crate::daemon::connect_or_start().await?.http().await?;
    let mut terminal = client.open_login_terminal(profile, cols, rows).await?;
    let terminal_input = terminal.input();
    updates
        .send(Update::Ready)
        .await
        .context("login pane closed")?;
    let receive = async {
        loop {
            let frame = terminal.next_output().await?;
            let finished = matches!(
                frame,
                TerminalOutput::Exited { .. } | TerminalOutput::Failed(_)
            );
            updates
                .send(Update::Frame(frame))
                .await
                .context("login pane closed")?;
            if finished {
                return Ok(());
            }
        }
    };
    let send = async {
        while let Some(input) = input.recv().await {
            if let Err(error) = terminal_input.send_input(input).await {
                // A terminal that just exited can close input before the
                // output reader sees its exit status.
                if error
                    .downcast_ref::<mj_client::http::HttpApiError>()
                    .is_some_and(|error| error.status.as_u16() == 404)
                {
                    return std::future::pending().await;
                }
                return Err(error);
            }
        }
        Ok(())
    };
    tokio::select! {
        result = receive => result,
        result = send => result,
    }
}

pub(super) struct ActiveLogin {
    pane: LoginPane,
    profile: String,
    generation: u64,
    completed: Option<Result<(), String>>,
    input: mpsc::UnboundedSender<TerminalInput>,
    output: mpsc::Receiver<Update>,
    _job: AbortOnDropHandle<()>,
}

enum PaneAction {
    Continue,
    Close,
    Quit,
}

impl ActiveLogin {
    pub(super) fn start(profile: &str, generation: u64) -> Result<Self> {
        let (width, height) = crossterm::terminal::size().context("read login pane size")?;
        let (cols, rows) = LoginPane::terminal_size(width, height);
        let (input, input_rx) = mpsc::unbounded_channel();
        // Backpressure reaches the daemon's PTY instead of accumulating a
        // transcript faster than the dashboard can render it.
        let (updates, output) = mpsc::channel(1);
        let worker_profile = profile.to_owned();
        let job = AbortOnDropHandle::new(mj_core::runtime::spawn(async move {
            let worker = AbortOnDropHandle::new(mj_core::runtime::spawn(connection(
                worker_profile,
                cols,
                rows,
                input_rx,
                updates.clone(),
            )));
            let error = match worker.await {
                Ok(Ok(())) => return,
                Ok(Err(error)) => format!("{error:#}"),
                Err(error) => format!("login connection task failed: {error}"),
            };
            let message = format!(
                "The daemon connection ended during login or harness preparation; retry Login. {error}"
            );
            if updates.send(Update::Failed(message)).await.is_err() {
                tracing::debug!(%error, "closed login pane connection ended");
            }
        }));
        Ok(Self {
            pane: LoginPane::new(profile, cols, rows),
            profile: profile.into(),
            generation,
            completed: None,
            input,
            output,
            _job: job,
        })
    }
    pub(super) fn render(&self, frame: &mut ratatui::Frame<'_>, screen: ratatui::layout::Rect) {
        self.pane.render(frame, screen);
    }
    fn send(&self, input: TerminalInput) {
        if self.input.send(input).is_err() {
            // Completion belongs to the connection owner. Its terminal result
            // may already be queued; input cannot invent a competing result.
            tracing::debug!(profile = %self.profile, "login input closed; awaiting connection result");
        }
    }
    fn accept(&mut self, update: Option<Update>) -> Option<Result<(), String>> {
        let result = match update {
            Some(Update::Ready) => {
                self.pane.status(
                    "Sign in on the daemon. Browser localhost callbacks may need that computer.",
                );
                return None;
            }
            Some(Update::Frame(TerminalOutput::Bytes(bytes))) => {
                let reply = self.pane.output(&bytes);
                if !reply.is_empty() {
                    self.send(TerminalInput::Bytes(reply));
                }
                return None;
            }
            Some(Update::Frame(TerminalOutput::Exited { code: 0 })) => Ok(()),
            Some(Update::Frame(TerminalOutput::Exited { code })) => {
                Err(format!("Login exited with code {code}."))
            }
            Some(Update::Frame(TerminalOutput::Failed(message)))
            | Some(Update::Failed(message)) => Err(message),
            None => Err(
                "The daemon connection ended during login or harness preparation; retry Login."
                    .into(),
            ),
        };
        self.pane.status(match &result {
            Ok(()) => "Signed in. Enter returns to Settings.".into(),
            Err(message) => format!("{message} Enter returns to Settings."),
        });
        self.completed = Some(result.clone());
        Some(result)
    }
    fn event(&mut self, event: Event) -> PaneAction {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    if key.code == KeyCode::Char('q') {
                        return PaneAction::Quit;
                    }
                    if key.code == KeyCode::Char(']') {
                        return PaneAction::Close;
                    }
                }
                if self.completed.is_some() && matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                    return PaneAction::Close;
                }
                if self.completed.is_none()
                    && let Some(bytes) = self.pane.key(key)
                {
                    self.send(TerminalInput::Bytes(bytes));
                }
            }
            Event::Paste(text) if self.completed.is_none() => {
                self.send(TerminalInput::Bytes(self.pane.paste(&text)))
            }
            Event::Resize(width, height) => {
                let (cols, rows) = LoginPane::terminal_size(width, height);
                self.pane.resize(cols, rows);
                if self.completed.is_none() {
                    self.send(TerminalInput::Resize { cols, rows });
                }
            }
            _ => {}
        }
        PaneAction::Continue
    }
}

pub(super) async fn wait(login: &mut Option<ActiveLogin>) -> Option<Update> {
    match login {
        Some(login) if login.completed.is_none() => login.output.recv().await,
        _ => std::future::pending().await,
    }
}

/// Fair selection keeps an output-heavy login and warm chats independently
/// live even though the enclosing dashboard select prioritizes terminal input.
pub(super) async fn pump(
    login: &mut Option<ActiveLogin>,
    chats: &mut std::collections::BTreeMap<String, mj_chat::chat::ActiveChat>,
) -> Option<Option<Update>> {
    tokio::select! {
        update = wait(login) => Some(update),
        () = super::pump_chats(chats) => None,
    }
}

impl DashboardContext {
    pub(super) fn accept_login_update(&mut self, update: Option<Update>) {
        if let Some(login) = &mut self.login
            && let Some(result) = login.accept(update)
        {
            self.dashboard
                .setup_login_finished(login.generation, &login.profile, result.into());
        }
    }
    pub(super) fn login_event(&mut self, event: Event) {
        let action = self
            .login
            .as_mut()
            .expect("login pane owns input")
            .event(event);
        if matches!(action, PaneAction::Continue) {
            return;
        }
        if let Some(login) = self.login.take()
            && login.completed.is_none()
        {
            self.dashboard.setup_login_finished(
                login.generation,
                &login.profile,
                mj_tui::SetupLoginOutcome::Cancelled,
            );
        }
        if matches!(action, PaneAction::Quit) {
            self.request_shutdown();
        }
    }
}
