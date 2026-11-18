//! Fetch payloads stay on disk. Dropping an async transfer cancels its child
//! processes; an unfinished durable fetch is replayed by the next daemon.
use super::*;
use std::io::{Read, Seek};
use tokio_util::sync::CancellationToken;

pub(super) async fn read(
    session: String,
    path: PathBuf,
    mode: FetchMode,
) -> Result<FetchedPath, ExportError> {
    let token = CancellationToken::new();
    let _cancel = token.clone().drop_guard();
    export_blocking("session file fetch", move || {
        let executor = CancellableProcessExecutor::new(Default::default()).with_cancellation(token);
        let layout = Controller::load()?.session_export_layout(&session, &executor)?;
        let arguments = match fetch_arguments(&layout, &path, mode) {
            Ok(arguments) => arguments,
            Err(error) => return Ok(Err(error)),
        };
        let command = worker_command_spec(layout, &session, arguments, "session file fetch")?;
        let mut spool = mj_checkpoint::archive::create_fetch_spool()?;
        let mut output = executor.execute_to_file(&command, &spool)?;
        spool.rewind()?;
        if output.status == mj_checkpoint::archive::EXPORT_REFUSED_EXIT_CODE {
            // Precondition refusals contain a diagnostic, never a payload.
            spool.read_to_end(&mut output.stdout)?;
        }
        if output.status != 0 {
            return Ok(worker_output(output, "session file fetch").map(|_| unreachable!()));
        }
        Ok(FetchedPath::read_from(spool).map_err(ExportError::Failed))
    })
    .await?
}

pub(super) async fn store(
    parent: String,
    session: String,
    fetched: FetchedPath,
) -> Result<Vec<u8>, ExportError> {
    let token = CancellationToken::new();
    let _cancel = token.clone().drop_guard();
    export_blocking("session fetched file storage", move || {
        let executor = CancellableProcessExecutor::new(Default::default()).with_cancellation(token);
        let layout = Controller::load()?.session_export_layout(&parent, &executor)?;
        let mut arguments = vec![
            "store-fetch".into(),
            "--session".into(),
            session,
            "--source".into(),
            fetched.path.clone(),
            "--length".into(),
            fetched.len().to_string(),
        ];
        if fetched.kind == mj_checkpoint::archive::FetchedKind::Directory {
            arguments.push("--directory".into());
        }
        let command =
            worker_command_spec(layout, &parent, arguments, "session fetched file storage")?;
        let output = executor.execute_with_stdin(&command, &mut fetched.reader())?;
        Ok(worker_output(output, "session fetched file storage"))
    })
    .await?
}
