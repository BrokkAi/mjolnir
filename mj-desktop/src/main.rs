//! Native Mjolnir desktop application.

#[cfg(any(
    target_os = "macos",
    target_os = "windows",
    all(target_os = "linux", target_env = "gnu")
))]
use anyhow::Context;
use anyhow::{Result, bail};

fn main() {
    if let Err(error) = run_cli() {
        eprintln!("mj-desktop: {error:#}");
        std::process::exit(1);
    }
}

fn run_cli() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    match args.next().as_deref() {
        Some(argument) if argument == "--version" || argument == "-V" => {
            println!("mj-desktop {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some(argument) if argument == "--help" || argument == "-h" => {
            println!("Usage: mj-desktop\n\nOpen the Mjolnir web viewer in a native window.");
            return Ok(());
        }
        Some(argument) => bail!("unexpected argument {}", argument.to_string_lossy()),
        None => {}
    }
    if let Some(argument) = args.next() {
        bail!("unexpected argument {}", argument.to_string_lossy());
    }

    #[cfg(any(
        target_os = "macos",
        target_os = "windows",
        all(target_os = "linux", target_env = "gnu")
    ))]
    {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("build desktop runtime")?;
        runtime.block_on(run_desktop_app())
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "windows",
        all(target_os = "linux", target_env = "gnu")
    )))]
    {
        bail!("the native desktop application supports macOS, Windows, and GNU/Linux")
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "windows",
    all(target_os = "linux", target_env = "gnu")
))]
mod supported {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use anyhow::{Context, Result, bail, ensure};
    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{HeaderMap, Request, Response, StatusCode, header};
    use mj_controller::hel_desktop::{DesktopLaunch, sibling_executable};
    use mj_controller::hel_server::COOKIE_NAME;

    /// Headers that describe one transport hop rather than the end-to-end
    /// request and therefore must not cross the desktop proxy.
    const HOP_BY_HOP: [header::HeaderName; 7] = [
        header::CONNECTION,
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
    ];

    #[derive(Clone)]
    struct ProxyState {
        client: reqwest::Client,
        upstream: String,
    }

    pub(super) async fn run_desktop_app() -> Result<()> {
        let launch = controller_bootstrap()?;
        let certified = rcgen::generate_simple_self_signed(vec![
            "localhost".to_owned(),
            "127.0.0.1".to_owned(),
        ])
        .context("generate the desktop TLS certificate")?;
        let certificate_der = certified.cert.der().to_vec();
        let key_der = certified.key_pair.serialize_der();
        let tls =
            axum_server::tls_rustls::RustlsConfig::from_der(vec![certificate_der.clone()], key_der)
                .await
                .context("build the desktop TLS configuration")?;

        let proxy_state = ProxyState {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .context("build the desktop proxy client")?,
            upstream: launch.viewer_url.trim_end_matches('/').to_owned(),
        };
        let router = Router::new().fallback(proxy).with_state(proxy_state);

        let handle = axum_server::Handle::new();
        let server = axum_server::bind_rustls("127.0.0.1:0".parse::<SocketAddr>()?, tls)
            .handle(handle.clone());
        let server_task = tokio::spawn(server.serve(router.into_make_service()));

        let bound = handle
            .listening()
            .await
            .context("bind the desktop proxy listener")?;
        let origin: url::Url = format!("https://localhost:{}/", bound.port())
            .parse()
            .context("build the desktop viewer origin")?;

        println!("Opening the Mjolnir desktop viewer at {origin}");
        let (shell_tx, shell_rx) =
            tokio::sync::oneshot::channel::<mj_desktop::DesktopShellRemote>();
        let watchdog = tokio::spawn(async move {
            let outcome = server_task.await;
            if let Ok(shell) = shell_rx.await {
                shell.fail(match outcome {
                    Ok(Ok(())) => "the desktop proxy exited unexpectedly".to_owned(),
                    Ok(Err(error)) => format!("the desktop proxy failed: {error}"),
                    Err(error) => format!("the desktop proxy panicked: {error}"),
                });
            }
        });

        let shell_result = mj_desktop::run(
            mj_desktop::DesktopShellOptions {
                origin,
                certificate_der,
                bootstrap_cookie_name: COOKIE_NAME,
                bootstrap_cookie_value: launch.bootstrap_cookie_value,
            },
            move |shell| {
                let _ = shell_tx.send(shell);
            },
        );

        handle.shutdown();
        watchdog.abort();
        shell_result.map(|_| ())
    }

    fn controller_bootstrap() -> Result<DesktopLaunch> {
        let executable = controller_executable()?;
        let mut command = std::process::Command::new(&executable);
        command.arg("desktop-bootstrap");
        let output = hel::hel_subprocess::run_with_input(&mut command, &[])
            .with_context(|| format!("start Mjolnir controller {}", executable.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr.trim();
            if detail.is_empty() {
                bail!("Mjolnir controller exited with {}", output.status);
            }
            bail!("Mjolnir controller exited with {}: {detail}", output.status);
        }
        DesktopLaunch::from_json(&output.stdout)
    }

    fn controller_executable() -> Result<PathBuf> {
        let path = if let Some(path) = hel::hel_config::env_override_os("CONTROLLER_BINARY") {
            PathBuf::from(path)
        } else {
            let current = std::env::current_exe().context("locate the mj-desktop executable")?;
            sibling_executable(&current, "mj")
        };
        ensure!(
            path.is_file(),
            "Mjolnir controller is missing: {}; install mj beside mj-desktop or set MJ_CONTROLLER_BINARY",
            path.display()
        );
        Ok(path)
    }

    async fn proxy(State(state): State<ProxyState>, request: Request<Body>) -> Response<Body> {
        match forward(state, request).await {
            Ok(response) => response,
            Err(error) => {
                let mut response = Response::new(Body::from(format!(
                    "the desktop proxy could not reach the Mjolnir daemon: {error:#}"
                )));
                *response.status_mut() = StatusCode::BAD_GATEWAY;
                response
            }
        }
    }

    async fn forward(state: ProxyState, request: Request<Body>) -> Result<Response<Body>> {
        let path_and_query = request
            .uri()
            .path_and_query()
            .map_or("/", |part| part.as_str());
        let url = format!("{}{path_and_query}", state.upstream);
        let (parts, body) = request.into_parts();

        let mut upstream = state
            .client
            .request(parts.method, url)
            .body(reqwest::Body::wrap_stream(body.into_data_stream()));
        for (name, value) in filtered(&parts.headers) {
            upstream = upstream.header(name.clone(), value.clone());
        }
        let response = upstream.send().await.context("forward to the daemon")?;

        let mut builder = Response::builder().status(response.status());
        for (name, value) in filtered(response.headers()) {
            builder = builder.header(name.clone(), value.clone());
        }
        builder
            .body(Body::from_stream(response.bytes_stream()))
            .context("assemble the proxied response")
    }

    fn filtered(
        headers: &HeaderMap,
    ) -> impl Iterator<Item = (&header::HeaderName, &header::HeaderValue)> {
        headers
            .iter()
            .filter(|(name, _)| *name != header::HOST && !HOP_BY_HOP.contains(name))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn hop_by_hop_and_host_headers_are_not_forwarded() {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, "localhost:1".parse().unwrap());
            headers.insert(header::CONNECTION, "keep-alive".parse().unwrap());
            headers.insert(header::COOKIE, "mj_viewer_session=x".parse().unwrap());
            headers.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
            let kept: Vec<_> = filtered(&headers).map(|(name, _)| name.clone()).collect();
            assert_eq!(kept.len(), 2, "{kept:?}");
            assert!(kept.contains(&header::COOKIE));
            assert!(kept.contains(&header::ACCEPT));
        }
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "windows",
    all(target_os = "linux", target_env = "gnu")
))]
use supported::run_desktop_app;
