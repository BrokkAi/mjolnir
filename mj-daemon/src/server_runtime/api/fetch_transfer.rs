//! Fetch payloads stay on disk. Dropping an async transfer cancels its child
//! processes; an unfinished durable fetch is replayed by the next daemon.
use super::*;
use mj_core::targets::{CommandSpec, SshTarget, TargetLocator, posix_quote};
use std::io::{Read, Seek};
use tokio_util::sync::CancellationToken;

/// The SSH host a locator's commands run on; `None` for a local target.
fn remote_ssh(locator: &TargetLocator) -> Option<&SshTarget> {
    match locator {
        TargetLocator::AwsEc2 { ssh, .. } | TargetLocator::SshBare { ssh, .. } => Some(ssh),
        other => other.container_endpoint().and_then(|endpoint| endpoint.ssh),
    }
}

/// `worker fetch` on the source. A remote payload crosses the wire compressed;
/// the gzip is a no-op for a local target.
fn read_command(
    layout: SessionExportLayout,
    session: &str,
    arguments: Vec<String>,
) -> Result<CommandSpec> {
    Ok(worker_command_spec(layout, session, arguments, "session file fetch")?.gzip_ssh_stdout())
}

/// `worker store-fetch` arguments up to, not including, the payload's `--source`.
fn store_base_arguments(session: &str) -> Vec<String> {
    vec!["store-fetch".into(), "--session".into(), session.into()]
}

/// `worker store-fetch` on the parent. A file payload crosses the wire
/// compressed; a directory is already a Deflate zip, so gzip would only cost
/// CPU.
fn store_command(
    layout: SessionExportLayout,
    parent: &str,
    session: &str,
    fetched: &FetchedPath,
) -> Result<CommandSpec> {
    let mut arguments = store_base_arguments(session);
    arguments.extend([
        "--source".into(),
        fetched.path.clone(),
        "--length".into(),
        fetched.len().to_string(),
    ]);
    if fetched.kind == mj_checkpoint::archive::FetchedKind::Directory {
        arguments.push("--directory".into());
    }
    let command = worker_command_spec(layout, parent, arguments, "session fetched file storage")?;
    Ok(
        if fetched.kind == mj_checkpoint::archive::FetchedKind::File {
            command.gzip_ssh_stdin()
        } else {
            command
        },
    )
}

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
        let command = read_command(layout, &session, arguments)?;
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
        let command = store_command(layout, &parent, &session, &fetched)?;
        let output = executor.execute_with_stdin(&command, &mut fetched.reader())?;
        Ok(worker_output(output, "session fetched file storage"))
    })
    .await?
}

/// Fetch `path` of `session` into `parent` without the payload crossing the
/// daemon, when both sessions run on the same remote SSH host. `Ok(None)` means
/// they do not (a local target stays on the local path), so the caller reads
/// and stores through the daemon instead. On success the value is the stdout of
/// the destination's `worker store-fetch`.
pub(super) async fn try_transfer_same_ssh_host(
    parent: String,
    session: String,
    path: PathBuf,
) -> Result<Option<Vec<u8>>, ExportError> {
    let token = CancellationToken::new();
    let _cancel = token.clone().drop_guard();
    export_blocking("session same-host file fetch", move || {
        let executor = CancellableProcessExecutor::new(Default::default()).with_cancellation(token);
        let controller = Controller::load()?;
        let source = controller.session_export_layout(&session, &executor)?;
        let destination = controller.session_export_layout(&parent, &executor)?;
        let (Some(source_ssh), Some(destination_ssh)) = (
            remote_ssh(&source.backend),
            remote_ssh(&destination.backend),
        ) else {
            return Ok(Ok(None));
        };
        if source_ssh != destination_ssh {
            return Ok(Ok(None));
        }
        let fetch = match fetch_arguments(&source, &path, FetchMode::Content) {
            Ok(arguments) => arguments,
            Err(error) => return Ok(Err(error)),
        };
        let script = same_host_fetch_script(
            &host_worker_argv(&source.backend, &session, fetch)?,
            &host_worker_argv(
                &destination.backend,
                &parent,
                store_base_arguments(&session),
            )?,
        );
        let host = TargetLocator::SshBare {
            ssh: source_ssh.clone(),
            workspace: String::new(),
            worker_id: None,
        };
        let command = targets::locator_command(
            SshTrafficClass::Bulk,
            &host,
            vec![
                "sh".into(),
                "-c".into(),
                script,
                "mj-same-host-fetch".into(),
            ],
        )
        .purpose("session same-host file fetch");
        let output = executor.execute(&command)?;
        Ok(worker_output(output, "session same-host file fetch").map(Some))
    })
    .await?
}

/// `<worker_root>/hel worker <arguments>` as the SSH host runs it: directly on
/// a bare host, through the engine's `exec -i` for a container.
fn host_worker_argv(
    locator: &TargetLocator,
    session: &str,
    arguments: Vec<String>,
) -> Result<Vec<String>> {
    let mut argv = vec![
        format!("{}/hel", targets::worker_root(locator, session)?),
        "worker".into(),
    ];
    argv.extend(arguments);
    Ok(match locator.container_endpoint() {
        None => argv,
        Some(endpoint) => {
            let exec =
                targets::drained_container_exec(endpoint.engine, endpoint.container_id, argv);
            std::iter::once(exec.program).chain(exec.args).collect()
        }
    })
}

/// Reads the spool's header line and hands the payload that follows it to the
/// destination command as stdin, with no further copy: the file offset is the
/// start of the payload. A `length` the header promised but the spool does not
/// hold (a full disk, a dying source) is an error before the destination runs.
/// Perl is the one scripting tool that decodes the JSON header, seeks and
/// `exec`s without a second process in between. Any failure exits 70.
const SPOOL_TO_STORE: &str = r#"use strict; use warnings; use JSON::PP;
BEGIN { $SIG{__DIE__} = sub { return if $^S; print STDERR $_[0]; exit 70 } }
my ($spool, $separator, @base) = @ARGV;
die "usage: spool -- command
" unless defined $separator && $separator eq '--' && @base;
open(my $fh, '<:raw', $spool) or die "open the fetch spool: $!
";
my $line = <$fh>;
die "the fetched path has no header line
" unless defined $line;
my $header = eval { JSON::PP->new->utf8->decode($line) };
die "decode fetched path header: $@" unless ref $header eq 'HASH';
my $kind = $header->{kind};
die "unexpected fetched kind
" unless defined $kind && ($kind eq 'file' || $kind eq 'directory');
die "the fetched path header has no path
" unless defined $header->{path};
my $offset = tell($fh);
my $actual = (stat($fh))[7] - $offset;
die "incomplete fetch transfer: expected $header->{length} bytes, received $actual
"
    if defined $header->{length} && $actual != $header->{length};
sysseek($fh, $offset, 0) or die "seek the fetch spool: $!
";
open(STDIN, '<&', $fh) or die "attach the fetch spool: $!
";
my $path = $header->{path};
utf8::encode($path);
my @command = (@base, '--source', $path, '--length', $actual,
    $kind eq 'directory' ? ('--directory') : ());
exec { $command[0] } @command or die "exec $command[0]: $!
";
"#;

/// The script that runs `source` and `destination` back to back on one host:
/// the source's output lands in a private temporary file, then feeds the
/// destination. Both are argv vectors for that host. The payload never leaves
/// the machine. A refusal (`EXPORT_REFUSED_EXIT_CODE`) is passed through with
/// its output; any other source failure exits with the source's status.
fn same_host_fetch_script(source: &[String], destination: &[String]) -> String {
    let quote = |argv: &[String]| {
        argv.iter()
            .map(|word| posix_quote(word))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let refused = mj_checkpoint::archive::EXPORT_REFUSED_EXIT_CODE;
    format!(
        r#"umask 077
tmp=$(mktemp -d "${{TMPDIR:-/tmp}}/mj-fetch-XXXXXX") || exit 70
trap 'rm -rf -- "$tmp"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
{source} >"$tmp/spool"
status=$?
if [ "$status" -eq {refused} ]; then
    cat -- "$tmp/spool"
    exit {refused}
fi
[ "$status" -eq 0 ] || exit "$status"
perl -e {program} "$tmp/spool" -- {destination}
"#,
        source = quote(source),
        destination = quote(destination),
        program = posix_quote(SPOOL_TO_STORE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_checkpoint::archive::FetchedKind;

    fn ssh_layout(session: &str) -> SessionExportLayout {
        SessionExportLayout {
            backend: TargetLocator::SshBare {
                ssh: SshTarget {
                    destination: "ubuntu@203.0.113.9".into(),
                    ssh_args: vec![],
                },
                workspace: format!(".local/share/hel/workspaces/{session}"),
                worker_id: None,
            },
            workspace_root: "/w".into(),
            primary_repository: "project".into(),
            repositories: Vec::new(),
            report_root: "/reports".into(),
            managed_worktree: None,
        }
    }

    fn fetched(kind: FetchedKind) -> FetchedPath {
        FetchedPath::from_file(kind, "/w/out".into(), tempfile::tempfile().unwrap()).unwrap()
    }

    #[test]
    fn remote_fetch_compresses_the_read_and_only_a_file_store() {
        let read = read_command(
            ssh_layout("child-session"),
            "child-session",
            vec!["fetch".into()],
        )
        .unwrap();
        assert!(read.ssh_gzip_stdout && !read.ssh_gzip_stdin);
        for (kind, compressed) in [(FetchedKind::File, true), (FetchedKind::Directory, false)] {
            let store = store_command(
                ssh_layout("parent-session"),
                "parent-session",
                "child-session",
                &fetched(kind),
            )
            .unwrap();
            assert_eq!(store.ssh_gzip_stdin, compressed, "{kind:?}");
            assert!(!store.ssh_gzip_stdout);
        }
    }

    /// Runs the same-host script on this machine, with `TMPDIR` pointing at
    /// `scratch` so the test can see what the script leaves behind.
    #[cfg(unix)]
    fn run_script(source: &str, destination: &str, scratch: &Path) -> std::process::Output {
        let shell = |body: &str| vec!["sh".to_owned(), "-c".into(), body.to_owned(), "w".into()];
        let script = same_host_fetch_script(&shell(source), &shell(destination));
        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", &script])
            .env("TMPDIR", scratch)
            .env("MJ_TEST_SCRATCH", scratch);
        mj_core::subprocess::run_with_input(&mut command, &[]).unwrap()
    }

    /// A source that writes a header line and then a payload file.
    #[cfg(unix)]
    fn source_of(kind: &str, length: Option<u64>, payload: &Path) -> String {
        let length = length.map_or(String::new(), |length| format!(r#","length":{length}"#));
        format!(
            r#"printf '%s\n' '{{"kind":"{kind}","path":"/w/it'\''s ünï.bin"{length}}}'; cat '{}'"#,
            payload.display()
        )
    }

    /// A destination that keeps its stdin and arguments where the test can read them.
    #[cfg(unix)]
    const STORE: &str = r#"cat >"$MJ_TEST_SCRATCH/stored" && printf '%s\n' "$@" >"$MJ_TEST_SCRATCH/args"; printf '{"saved":true}'"#;

    #[cfg(unix)]
    fn leftover(scratch: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(scratch)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("mj-fetch-"))
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    #[test]
    fn same_host_fetch_hands_the_payload_to_the_store_without_leaving_a_spool() {
        let dir = tempfile::tempdir().unwrap();
        // More than a pipe buffer, with bytes a text-mode copy would damage.
        let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let file = dir.path().join("payload");
        std::fs::write(&file, &payload).unwrap();
        let scratch = tempfile::tempdir().unwrap();

        for (kind, directory) in [("file", false), ("directory", true)] {
            let output = run_script(
                &source_of(kind, Some(payload.len() as u64), &file),
                // `w` is `$0`, so the store sees only the appended arguments.
                STORE,
                scratch.path(),
            );
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, br#"{"saved":true}"#);
            assert_eq!(
                std::fs::read(scratch.path().join("stored")).unwrap(),
                payload
            );
            let args = std::fs::read_to_string(scratch.path().join("args")).unwrap();
            let expected = format!(
                "--source\n/w/it's ünï.bin\n--length\n{}\n{}",
                payload.len(),
                if directory { "--directory\n" } else { "" }
            );
            assert_eq!(args, expected);
            assert_eq!(leftover(scratch.path()), Vec::<String>::new());
        }
    }

    #[cfg(unix)]
    #[test]
    fn same_host_fetch_passes_a_refusal_through_and_stops_on_any_failure() {
        let scratch = tempfile::tempdir().unwrap();
        let refused = run_script(
            &format!(
                "echo 'path is outside the export root'; exit {}",
                mj_checkpoint::archive::EXPORT_REFUSED_EXIT_CODE
            ),
            STORE,
            scratch.path(),
        );
        assert_eq!(
            refused.status.code(),
            Some(mj_checkpoint::archive::EXPORT_REFUSED_EXIT_CODE)
        );
        assert_eq!(refused.stdout, b"path is outside the export root\n");

        // A full disk or a dying worker: the status is the source's own.
        let failed = run_script("echo disk full >&2; exit 7", STORE, scratch.path());
        assert_eq!(failed.status.code(), Some(7));
        assert!(String::from_utf8_lossy(&failed.stderr).contains("disk full"));

        assert!(
            !scratch.path().join("stored").exists(),
            "the store never ran"
        );
        assert_eq!(leftover(scratch.path()), Vec::<String>::new());
    }

    #[cfg(unix)]
    #[test]
    fn same_host_fetch_rejects_a_short_or_malformed_spool_before_storing() {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("payload");
        std::fs::write(&payload, vec![7u8; 100_000]).unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let sources = [
            // The source exited cleanly but the spool is shorter than it promised.
            (
                source_of("file", Some(100_001), &payload),
                "incomplete fetch transfer",
            ),
            (
                source_of("listing", None, &payload),
                "unexpected fetched kind",
            ),
            (
                "printf 'not json\\n'".to_owned(),
                "decode fetched path header",
            ),
            (":".to_owned(), "no header line"),
        ];
        for (source, message) in sources {
            let output = run_script(&source, STORE, scratch.path());
            assert_eq!(output.status.code(), Some(70), "{message}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(message),
                "{message}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!scratch.path().join("stored").exists(), "{message}");
            assert_eq!(leftover(scratch.path()), Vec::<String>::new(), "{message}");
        }
    }
}
