//! Read-only, TAP-bound mbx cache for one VM. All authorization comes from
//! launch arguments, before the proxy opens or fetches any object.

mod s3;
#[cfg(test)]
mod tests;
mod trim;

use std::fs::{File, FileTimes};
use std::io;
use std::net::SocketAddrV4;
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow, ensure};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use clap::Args;
use mbx_cache_protocol::{
    ACTION_RESULT_MEDIA_TYPE, ActionResult, BLOB_MEDIA_TYPE, Capabilities, CapabilityProtocol,
    Digest, NAMESPACE_HEADER, PROTOCOL_VERSION, TASK_ACTION_MANIFEST_MEDIA_TYPE,
    TaskActionManifest, canonical_json,
};
use sha2::{Digest as _, Sha256};
use socket2::{Domain, Protocol, Socket, Type};
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;
use tokio_util::io::ReaderStream;

use super::boot::BootAssets;
use super::net::Link;
use super::state::{Layout, MbxRemote, VmState};
use super::{sys, vm};
use s3::{ReadError, S3, core_error, read_body};

pub(super) const PORT: u16 = 7461;
const USER: &str = "mj-mbx-proxy";
const UNIT_ENGINE: &str = "/run/mj-mbx-proxy/engine";
const UNIT_CACHE: &str = "/run/mj-mbx-proxy/cache";
const STAGING_PREFIX: &str = ".mbx-stage-";
// Same JSON response limit as mbx-cache-core's S3 transport.
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Args)]
pub(super) struct ProxyArgs {
    #[arg(long)]
    device: String,
    #[arg(long)]
    listen: SocketAddrV4,
    #[arg(long)]
    bucket: String,
    #[arg(long, default_value = "")]
    prefix: String,
    #[arg(long)]
    region: Option<String>,
    #[arg(long = "namespace", required = true)]
    namespaces: Vec<String>,
    #[arg(long)]
    cache_dir: PathBuf,
    #[arg(long)]
    cache_budget: Option<u64>,
}

#[derive(Debug, Args)]
pub(super) struct CheckArgs {
    #[arg(long)]
    bucket: String,
    #[arg(long, default_value = "")]
    prefix: String,
    #[arg(long)]
    region: Option<String>,
    #[arg(long)]
    namespace: String,
}

pub(super) fn parse_remote(
    url: &str,
    region: Option<String>,
    namespaces: Vec<String>,
    budget: Option<u64>,
) -> Result<MbxRemote> {
    let location = url
        .strip_prefix("s3://")
        .context("--mbx-remote must start with s3://")?;
    let (bucket, prefix) = location.split_once('/').unwrap_or((location, ""));
    remote(
        bucket.to_owned(),
        prefix.to_owned(),
        region,
        namespaces,
        budget,
    )
}

fn remote(
    bucket: String,
    prefix: String,
    region: Option<String>,
    namespaces: Vec<String>,
    budget: Option<u64>,
) -> Result<MbxRemote> {
    ensure!(
        mj_core::config::valid_mbx_s3_bucket(&bucket),
        "invalid S3 bucket {bucket:?}"
    );
    let prefix = prefix.trim_matches('/').to_owned();
    ensure!(
        !prefix
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
            && !prefix.contains('%'),
        "S3 prefix cannot contain standalone '.' or '..' segments or percent escapes"
    );
    ensure!(
        !namespaces.is_empty(),
        "at least one mbx namespace is required"
    );
    for namespace in &namespaces {
        ensure!(
            valid_namespace(namespace),
            "invalid mbx namespace {namespace:?}"
        );
    }
    Ok(MbxRemote {
        bucket,
        prefix,
        region,
        budget,
        namespaces,
    })
}

fn valid_namespace(namespace: &str) -> bool {
    namespace.split('/').all(|segment| {
        !matches!(segment, "" | "." | "..")
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    })
}

pub(super) fn start_unit(
    layout: &Layout,
    state: &VmState,
    link: &Link,
    boot: &BootAssets,
) -> Result<()> {
    let remote = state
        .mbx_remote
        .as_ref()
        .context("missing proxy configuration")?;
    let cache = layout.mbx_cache();
    let boot_dir = layout.boot().join(&boot.tag);
    let engine = boot_dir.join("mbx-proxy-engine");
    {
        // Different VMs may launch concurrently. Serialize only account and
        // directory setup, releasing the lock before systemd waits for IMDS.
        let _global = layout.lock_global()?;
        let passwd = sys::run_unchecked(&sys::find_tool("getent")?, &["passwd", USER])?;
        match passwd.status.code() {
            Some(0) => {}
            Some(2) => {
                sys::run_tool(
                    "useradd",
                    &[
                        "--system",
                        "--user-group",
                        "--no-create-home",
                        "--shell",
                        "/sbin/nologin",
                        USER,
                    ],
                )
                .context("create the mbx proxy system user")?;
            }
            _ => {
                return Err(anyhow!(
                    "look up {USER}: {}",
                    String::from_utf8_lossy(&passwd.stderr)
                ));
            }
        }
        sys::run_tool(
            "install",
            &[
                "-d",
                "-m",
                "0700",
                "-o",
                USER,
                "-g",
                USER,
                cache.to_str().context("cache path is not UTF-8")?,
            ],
        )
        .context("prepare the mbx cache directory")?;
        // systemd can hide /home before resolving bind mount sources. Stage
        // the actual running engine next to its boot assets, whose reservation
        // the caller holds until systemd has mounted the executable.
        if !engine.is_file() {
            let mut source =
                File::open("/proc/self/exe").context("open the running mbx proxy engine")?;
            let mut staged = NamedTempFile::new_in(&boot_dir)?;
            io::copy(&mut source, &mut staged).context("stage the mbx proxy engine")?;
            staged
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o555))?;
            staged
                .persist(&engine)
                .context("publish the mbx proxy engine")?;
        }
    }

    let vm_unit = vm::unit_name(&state.name);
    let unit = format!("mj-mbx-proxy-{}.service", state.name);
    let mut args = vec![
        "--quiet".to_owned(),
        "--collect".to_owned(),
        format!("--unit={unit}"),
        format!("--property=BindsTo={vm_unit}"),
        format!("--property=After={vm_unit}"),
        "--property=Type=notify".to_owned(),
        "--property=NotifyAccess=main".to_owned(),
        format!("--property=User={USER}"),
        format!("--property=Group={USER}"),
        "--property=CapabilityBoundingSet=CAP_NET_RAW".to_owned(),
        "--property=AmbientCapabilities=CAP_NET_RAW".to_owned(),
        "--property=NoNewPrivileges=yes".to_owned(),
        "--property=ProtectSystem=strict".to_owned(),
        "--property=ProtectHome=yes".to_owned(),
        "--property=PrivateTmp=yes".to_owned(),
        "--property=UMask=0077".to_owned(),
        // The engine and cache are below a root-only directory. Bind only
        // these two paths into the service, preserving the host permissions.
        format!(
            "--property=BindReadOnlyPaths={}:{UNIT_ENGINE}",
            engine.display()
        ),
        format!("--property=BindPaths={}:{UNIT_CACHE}", cache.display()),
        format!("--property=ReadWritePaths={UNIT_CACHE}"),
        UNIT_ENGINE.to_owned(),
        "microvm".to_owned(),
        "mbx-proxy".to_owned(),
        "--device".to_owned(),
        link.tap.clone(),
        "--listen".to_owned(),
        SocketAddrV4::new(link.host, PORT).to_string(),
        format!("--bucket={}", remote.bucket),
        // systemd expands dollar variables in the command unless escaped.
        format!("--prefix={}", remote.prefix.replace('$', "$$")),
        format!("--cache-dir={UNIT_CACHE}"),
    ];
    if let Some(region) = &remote.region {
        args.push(format!("--region={}", region.replace('$', "$$")));
    }
    if let Some(budget) = remote.budget {
        args.push(format!("--cache-budget={budget}"));
    }
    for namespace in &remote.namespaces {
        args.push(format!("--namespace={namespace}"));
    }
    sys::run_tool("systemd-run", &args)
        .with_context(|| format!("start the mbx proxy unit:\n{}", vm::unit_journal(&unit)))?;
    Ok(())
}

pub(super) fn trim_cache(layout: &Layout, budget: Option<u64>) -> Result<u64> {
    trim::trim(&layout.mbx_cache(), budget)
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    // The engine handles microvm commands before the normal worker TLS setup.
    let _ = rustls::crypto::ring::default_provider().install_default();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("start mbx proxy runtime")
}

pub(super) fn run(args: ProxyArgs) -> Result<i32> {
    let remote = remote(
        args.bucket,
        args.prefix,
        args.region,
        args.namespaces,
        args.cache_budget,
    )?;
    runtime()?.block_on(async {
        // Readiness covers only the listener. Credentials are fetched on the
        // first request, so an IMDS hiccup or a missing role never keeps the
        // VM from starting; each request then fails with the reason instead.
        let listener = bind(&args.device, args.listen)?;
        let app = router(Arc::new(Proxy::new(remote, args.cache_dir)));
        notify_ready()?;
        axum::serve(listener, app)
            .await
            .context("serve mbx proxy")?;
        Ok(0)
    })
}

pub(super) fn check(args: CheckArgs) -> Result<i32> {
    let remote = remote(
        args.bucket,
        args.prefix,
        args.region,
        vec![args.namespace],
        None,
    )?;
    let (message, code) = runtime()?.block_on(check_remote(&remote));
    println!("{message}");
    Ok(code)
}

async fn check_remote(remote: &MbxRemote) -> (String, i32) {
    let result = match S3::connect(remote).await {
        Ok(Some(store)) => check_access(&store, &remote.namespaces[0]).await,
        Ok(None) => return ("no instance role attached".to_owned(), 1),
        Err(error) => Err(error.into()),
    };
    check_result(remote, result)
}

async fn check_access(store: &S3, namespace: &str) -> Result<(), ReadError> {
    let route = format!("/v1/action-results/blake3/{}/0", "0".repeat(64));
    store.get(namespace, &route).await.map(drop)
}

fn check_result(remote: &MbxRemote, result: Result<(), ReadError>) -> (String, i32) {
    match result {
        Ok(()) => ("remote cache readable".to_owned(), 0),
        Err(error) => {
            let hint = if error.access_denied() {
                " (the role needs s3:GetObject on the prefix and s3:ListBucket on the bucket for the prefix)"
            } else {
                ""
            };
            (
                format!(
                    "role cannot read {}/{}: {}{hint}",
                    remote.bucket,
                    remote.prefix,
                    error.diagnostic()
                ),
                1,
            )
        }
    }
}

fn bind(device: &str, listen: SocketAddrV4) -> Result<TcpListener> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    socket
        .bind_device(Some(device.as_bytes()))
        .with_context(|| format!("bind mbx proxy to {device}"))?;
    socket.set_reuse_address(true)?;
    socket
        .bind(&std::net::SocketAddr::V4(listen).into())
        .with_context(|| format!("listen on {listen} via {device}"))?;
    // Linux caps this to its configured somaxconn.
    socket.listen(libc::SOMAXCONN)?;
    socket.set_nonblocking(true)?;
    TcpListener::from_std(socket.into()).context("register mbx proxy listener")
}

fn notify_ready() -> Result<()> {
    use std::os::unix::ffi::OsStrExt as _;

    let name = std::env::var_os("NOTIFY_SOCKET").context("missing systemd NOTIFY_SOCKET")?;
    let address = match name.as_bytes().strip_prefix(b"@") {
        Some(abstract_name) => SocketAddr::from_abstract_name(abstract_name)?,
        None => SocketAddr::from_pathname(name)?,
    };
    UnixDatagram::unbound()?
        .send_to_addr(b"READY=1", &address)
        .context("notify systemd that the mbx proxy is listening")?;
    Ok(())
}

struct Proxy {
    remote: MbxRemote,
    cache: PathBuf,
    budget: u64,
    trim_progress: Mutex<TrimProgress>,
    /// Connected on first use; a failed connection is retried by the next request.
    store: tokio::sync::OnceCell<S3>,
}

#[derive(Default)]
struct TrimProgress {
    published: u64,
    running: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Blob,
    Action,
    Manifest,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blobs",
            Self::Action => "action-results",
            Self::Manifest => "action-manifests",
        }
    }

    fn media_type(self) -> &'static str {
        match self {
            Self::Blob => BLOB_MEDIA_TYPE,
            Self::Action => ACTION_RESULT_MEDIA_TYPE,
            Self::Manifest => TASK_ACTION_MANIFEST_MEDIA_TYPE,
        }
    }
}

fn object(path: &str) -> Option<(Kind, Digest)> {
    let mut parts = path.strip_prefix("/v1/")?.split('/');
    let kind = match parts.next()? {
        "blobs" => Kind::Blob,
        "action-results" => Kind::Action,
        "action-manifests" => Kind::Manifest,
        _ => return None,
    };
    let algorithm = parts.next()?;
    if kind != Kind::Blob && algorithm != "blake3" {
        return None;
    }
    let hash = parts.next()?;
    let size = parts.next()?;
    if parts.next().is_some() || !size.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let digest = Digest {
        algorithm: algorithm.to_owned(),
        hash: hash.to_owned(),
        size: size.parse().ok()?,
    };
    digest.validate().ok()?;
    Some((kind, digest))
}

fn router(proxy: Arc<Proxy>) -> Router {
    Router::new().fallback(handle).with_state(proxy)
}

async fn handle(State(proxy): State<Arc<Proxy>>, request: Request) -> Response {
    let path = request.uri().path();
    let capabilities = path == "/v1/capabilities";
    let object = object(path);
    if request.method() != Method::GET
        || request.uri().query().is_some()
        || (!capabilities && object.is_none())
    {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, "GET")],
            "read-only mbx cache",
        )
            .into_response();
    }
    let Some(namespace) = namespace(request.headers(), &proxy.remote) else {
        return (StatusCode::FORBIDDEN, "mbx namespace is not allowed").into_response();
    };
    let Some((kind, digest)) = object else {
        let mut document = Capabilities::new(CapabilityProtocol::new(PROTOCOL_VERSION, 0));
        document.digest_algorithms = vec!["blake3".to_owned(), "sha256".to_owned()];
        return Json(document).into_response();
    };
    match proxy.read(namespace, kind, &digest).await {
        Ok(Some(response)) => response,
        Ok(None) => (StatusCode::NOT_FOUND, "object absent from S3").into_response(),
        Err(error) => {
            let message = error.diagnostic();
            eprintln!("mbx cache read for {namespace}: {message}");
            (StatusCode::BAD_GATEWAY, message).into_response()
        }
    }
}

fn namespace<'a>(headers: &'a HeaderMap, remote: &MbxRemote) -> Option<&'a str> {
    let mut values = headers.get_all(NAMESPACE_HEADER).iter();
    let namespace = values.next()?.to_str().ok()?;
    (values.next().is_none()
        && valid_namespace(namespace)
        && remote.namespaces.iter().any(|ns| ns == namespace))
    .then_some(namespace)
}

impl Proxy {
    fn new(remote: MbxRemote, cache: PathBuf) -> Self {
        let budget = remote.budget.unwrap_or_else(|| trim::cache_budget(&cache));
        Self {
            remote,
            cache,
            budget,
            trim_progress: Mutex::default(),
            store: tokio::sync::OnceCell::new(),
        }
    }

    fn record_publish(self: &Arc<Self>, size: u64) {
        // A scan every budget/20 published bytes bounds the overshoot between
        // scans to 5% of the budget per proxy, plus in-flight writes, without
        // scanning the directory on every write.
        let threshold = self.budget / 20;
        {
            let mut progress = self.trim_progress.lock().unwrap();
            progress.published = progress.published.saturating_add(size);
            if progress.running || progress.published <= threshold {
                return;
            }
            progress.published = 0;
            progress.running = true;
        }
        let proxy = Arc::clone(self);
        mj_core::runtime::spawn_blocking(move || {
            loop {
                if let Err(error) = trim::trim(&proxy.cache, Some(proxy.budget)) {
                    eprintln!("trim the mbx host cache: {error:#}");
                }
                let mut progress = proxy.trim_progress.lock().unwrap();
                if progress.published <= threshold {
                    progress.running = false;
                    break;
                }
                // Catch writes that crossed the threshold during the scan
                // even if there are no more requests after this one.
                progress.published = 0;
            }
        });
    }

    fn publish(
        self: &Arc<Self>,
        temporary: NamedTempFile,
        entry: &Path,
        size: u64,
    ) -> Result<File> {
        match temporary.persist_noclobber(entry) {
            Ok(file) => {
                self.record_publish(size);
                Ok(file)
            }
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                Ok(error.file.into_file())
            }
            Err(error) if error.error.raw_os_error() == Some(libc::ENOSPC) => {
                eprintln!("mbx host cache is full; serving the object without caching it");
                Ok(error.file.into_file())
            }
            Err(error) => Err(error.error)
                .with_context(|| format!("publish mbx cache entry {}", entry.display())),
        }
    }

    async fn store(&self) -> Result<&S3, ReadError> {
        Ok(self
            .store
            .get_or_try_init(|| async {
                S3::connect(&self.remote)
                    .await?
                    .context("no instance role attached")
            })
            .await?)
    }

    fn entry(&self, namespace: &str, kind: Kind, digest: &Digest) -> PathBuf {
        // Lengths and segment boundaries are preserved, and the guest's
        // namespace and prefix never become filesystem path components.
        let identity = serde_json::to_vec(&(
            &self.remote.bucket,
            &self.remote.prefix,
            namespace,
            kind.as_str(),
            digest,
        ))
        .expect("serialize cache identity");
        self.cache.join(hex::encode(Sha256::digest(identity)))
    }

    async fn read(
        self: &Arc<Self>,
        namespace: &str,
        kind: Kind,
        digest: &Digest,
    ) -> Result<Option<Response>, ReadError> {
        let entry = self.entry(namespace, kind, digest);
        if kind != Kind::Manifest {
            match File::open(&entry) {
                Ok(file) => {
                    let size = file.metadata()?.len();
                    if let Err(error) =
                        file.set_times(FileTimes::new().set_accessed(SystemTime::now()))
                    {
                        eprintln!(
                            "update mbx cache access time for {}: {error}",
                            entry.display()
                        );
                    }
                    return Ok(Some(file_response(file, kind, size)));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(anyhow!(error).context("open mbx cache entry").into()),
            }
        }
        let key = format!(
            "/v1/{}/{}/{}/{}",
            kind.as_str(),
            digest.algorithm,
            digest.hash,
            digest.size
        );
        let Some(mut upstream) = self.store().await?.get(namespace, &key).await? else {
            return Ok(None);
        };
        if kind == Kind::Blob {
            let file = match stage_blob(&mut upstream, digest, &self.cache).await {
                Ok(file) => self.publish(file, &entry, digest.size)?,
                Err(error) if no_space(&error) => {
                    eprintln!("mbx host cache is full; serving the blob without caching it");
                    // The partially consumed response cannot be rewound.
                    // The unit's private temp directory is writable and may
                    // be on a different filesystem from the cache.
                    let Some(mut retry) = self.store().await?.get(namespace, &key).await? else {
                        return Ok(None);
                    };
                    stage_blob(&mut retry, digest, &std::env::temp_dir())
                        .await?
                        .into_file()
                }
                Err(error) => return Err(error.into()),
            };
            return Ok(Some(file_response(file, kind, digest.size)));
        }
        let etag = upstream.headers().get(header::ETAG).cloned();
        let bytes = read_body(upstream, MAX_JSON_BYTES).await?;
        if kind == Kind::Manifest {
            let etag = etag
                .filter(strong_etag)
                .context("S3 action manifest has no strong ETag")?;
            let manifest: TaskActionManifest =
                serde_json::from_slice(&bytes).context("decode action manifest")?;
            ensure_read(
                manifest.validate()
                    && manifest.selector_digest() == *digest
                    && canonical_json(&manifest).map_err(core_error)? == bytes,
                "S3 action manifest does not match its canonical selector",
            )?;
            return Ok(Some(
                (
                    [
                        (
                            header::CONTENT_TYPE,
                            HeaderValue::from_static(kind.media_type()),
                        ),
                        (header::ETAG, etag),
                    ],
                    bytes,
                )
                    .into_response(),
            ));
        }

        let action: ActionResult =
            serde_json::from_slice(&bytes).context("decode action result")?;
        ensure_read(
            action.version == 1 && action.action == *digest,
            "S3 action result does not match the requested action",
        )?;
        let cached = async {
            let temporary = staging_file(&self.cache)?;
            let mut output = tokio::fs::File::from_std(temporary.reopen()?);
            output.write_all(&bytes).await?;
            output.flush().await?;
            self.publish(temporary, &entry, bytes.len() as u64)
                .map(drop)
        }
        .await;
        if let Err(error) = cached {
            if !no_space(&error) {
                return Err(error.into());
            }
            eprintln!("mbx host cache is full; serving the action result without caching it");
        }
        Ok(Some(
            ([(header::CONTENT_TYPE, kind.media_type())], bytes).into_response(),
        ))
    }
}

fn ensure_read(condition: bool, message: &'static str) -> Result<()> {
    ensure!(condition, "{message}");
    Ok(())
}

fn strong_etag(value: &HeaderValue) -> bool {
    let value = value.as_bytes();
    matches!(value, [b'"', inner @ .., b'"'] if (1..=256).contains(&inner.len())
        && inner.iter().all(|b| (b'!'..=b'~').contains(b) && *b != b'"'))
}

async fn stage_blob(
    upstream: &mut reqwest::Response,
    digest: &Digest,
    directory: &Path,
) -> Result<NamedTempFile> {
    let temporary = staging_file(directory)?;
    let mut output = tokio::fs::File::from_std(temporary.reopen()?);
    let mut written = 0;
    while let Some(chunk) = upstream.chunk().await? {
        written += chunk.len() as u64;
        ensure!(written <= digest.size, "S3 blob exceeds the declared size");
        output.write_all(&chunk).await?;
    }
    output.flush().await?;
    drop(output);
    let digest = digest.clone();
    mj_core::runtime::spawn_blocking(move || {
        ensure!(
            digest.matches_file(temporary.path()).map_err(core_error)?,
            "S3 blob failed digest verification"
        );
        Ok(temporary)
    })
    .await
    .context("verify S3 blob")?
}

fn staging_file(directory: &Path) -> io::Result<NamedTempFile> {
    tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .tempfile_in(directory)
}

fn no_space(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.raw_os_error() == Some(libc::ENOSPC))
    })
}

fn file_response(file: File, kind: Kind, size: u64) -> Response {
    (
        [
            (header::CONTENT_TYPE, kind.media_type().to_owned()),
            (header::CONTENT_LENGTH, size.to_string()),
        ],
        Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(file))),
    )
        .into_response()
}
