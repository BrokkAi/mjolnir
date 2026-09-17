use super::*;

pub(super) struct ResolvedServerArgs {
    pub(super) bind: SocketAddr,
    pub(super) viewer_url: String,
    pub(super) tls_files: Option<(PathBuf, PathBuf)>,
    pub(super) tailscale: Option<TailscaleTls>,
    pub(super) fallback_reason: Option<String>,
}

pub(super) async fn resolve_server_args(
    args: ServerArgs,
    termination: tokio_util::sync::CancellationToken,
) -> Result<ResolvedServerArgs> {
    let configured_bind: SocketAddr = args.bind.parse().context("parse web viewer bind address")?;
    match (args.tls_cert, args.tls_key) {
        (Some(cert), Some(key)) => {
            let scheme = "https";
            return Ok(ResolvedServerArgs {
                bind: configured_bind,
                viewer_url: format!("{scheme}://{configured_bind}"),
                tls_files: Some((cert, key)),
                tailscale: None,
                fallback_reason: None,
            });
        }
        (None, None) => {}
        _ => bail!("web viewer TLS requires both a certificate and private key"),
    }

    if !args.tailscale_detect {
        return Ok(loopback_server_args(
            configured_bind,
            Some("automatic Tailscale detection is disabled".into()),
        ));
    }

    let tls_root = mj_core::config::data_dir().join("viewer");
    let prepared = run_tailscale_blocking(termination.clone(), move |executor| {
        crate::tailscale::prepare_tailscale_tls(&tls_root, executor)
    })
    .await;
    match prepared {
        Ok(tailscale) => {
            let bind = tailscale_bind(configured_bind);
            let viewer_url = format!(
                "https://{}:{}",
                tailscale.cert_domain(),
                configured_bind.port()
            );
            Ok(ResolvedServerArgs {
                bind,
                viewer_url,
                tls_files: Some((
                    tailscale.cert_path().to_owned(),
                    tailscale.key_path().to_owned(),
                )),
                tailscale: Some(tailscale),
                fallback_reason: None,
            })
        }
        Err(error) if termination.is_cancelled() => Err(error),
        Err(error) => {
            let reason = format!("{error:#}");
            tracing::debug!(error = reason, "Tailscale HTTPS unavailable for web viewer");
            Ok(loopback_server_args(configured_bind, Some(reason)))
        }
    }
}

pub(super) fn tailscale_bind(configured_bind: SocketAddr) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::UNSPECIFIED, configured_bind.port()))
}

pub(super) fn loopback_server_args(
    bind: SocketAddr,
    fallback_reason: Option<String>,
) -> ResolvedServerArgs {
    ResolvedServerArgs {
        bind,
        viewer_url: format!("http://{bind}"),
        tls_files: None,
        tailscale: None,
        fallback_reason,
    }
}

pub(super) async fn run_tailscale_blocking<T>(
    termination: tokio_util::sync::CancellationToken,
    operation: impl FnOnce(&CancellableProcessExecutor) -> Result<T> + Send + 'static,
) -> Result<T>
where
    T: Send + 'static,
{
    let cancelled = Arc::new(AtomicBool::new(false));
    let executor_cancelled = cancelled.clone();
    let mut task = tokio::task::spawn_blocking(move || {
        let executor = CancellableProcessExecutor::new(executor_cancelled)
            .with_deadline(TAILSCALE_COMMAND_TIMEOUT);
        operation(&executor)
    });
    tokio::select! {
        result = &mut task => result.context("Tailscale background task panicked")?,
        _ = termination.cancelled() => {
            cancelled.store(true, Ordering::Release);
            let _ = task.await;
            bail!("Tailscale operation cancelled during web viewer shutdown")
        }
    }
}

pub(super) fn spawn_tailscale_cert_renewer(
    tailscale: TailscaleTls,
    rustls: axum_server::tls_rustls::RustlsConfig,
    termination: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TAILSCALE_RENEW_INTERVAL);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = termination.cancelled() => return,
                _ = interval.tick() => {}
            }
            let renewing = tailscale.clone();
            let result = run_tailscale_blocking(termination.clone(), move |executor| {
                renewing.renew(executor)
            })
            .await;
            if let Err(error) = result {
                if !termination.is_cancelled() {
                    tracing::warn!(
                        error = format!("{error:#}"),
                        "Tailscale certificate renewal failed"
                    );
                }
                continue;
            }
            if let Err(error) = rustls
                .reload_from_pem_file(tailscale.cert_path(), tailscale.key_path())
                .await
            {
                tracing::warn!(%error, "could not activate renewed Tailscale certificate");
            }
        }
    })
}
