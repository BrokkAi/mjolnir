//! Dedicated login terminal framing, shared by every placement.
use super::*;

#[derive(Debug, Serialize, Deserialize)]
pub enum TerminalInput {
    Bytes(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum TerminalOutput {
    Bytes(Vec<u8>),
    Exited { code: u32 },
    Failed(String),
}

/// Owns the placement lease until the dedicated terminal closes.
pub struct LoginTerminal {
    pub stream: TcpStream,
    pub connector: Option<std::sync::Arc<connector::DaemonConnector>>,
}

impl DaemonClient {
    /// Consume a dedicated client; ordinary control traffic must use another connection.
    pub async fn login_terminal(
        mut self,
        profile_id: String,
        cols: u16,
        rows: u16,
    ) -> Result<LoginTerminal> {
        match self
            .request(DaemonAction::LoginTerminal {
                profile_id,
                cols,
                rows,
            })
            .await?
        {
            DaemonReply::LoginTerminalReady => Ok(LoginTerminal {
                stream: self.stream,
                connector: self.connector,
            }),
            reply => bail!("unexpected login terminal reply {reply:?}"),
        }
    }
}
