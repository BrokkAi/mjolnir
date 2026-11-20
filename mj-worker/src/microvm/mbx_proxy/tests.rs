use super::*;
use axum::body::Bytes;
use mbx_cache_core::{InstanceRoleCredentials, S3Credentials, TemporaryCredentials};
use mj_core::subprocess::run_with_input;
use std::collections::HashMap;
use std::fs;
use std::net::Ipv4Addr;
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::Duration;

const NAMESPACE: &str = "example/project";
const OTHER: &str = "example/second";
static LARGE: [u8; 128 * 1024] = [0xa5; 128 * 1024];

#[derive(Default)]
struct FakeS3 {
    objects: Mutex<HashMap<String, Bytes>>,
    reads: Mutex<Vec<String>>,
}

async fn fake_s3(
    State(fake): State<Arc<FakeS3>>,
    method: Method,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let key = uri.path().to_owned();
    match (method, key.as_str()) {
        (Method::PUT, "/latest/api/token") => "fixture-token".into_response(),
        (Method::GET, "/latest/meta-data/iam/security-credentials/") => {
            "fixture-role".into_response()
        }
        (Method::GET, "/latest/meta-data/iam/security-credentials/fixture-role") => {
            Json(serde_json::json!({
                "Code": "Success",
                "AccessKeyId": "fixture-id",
                "SecretAccessKey": "fixture-secret",
                "Token": "fixture-token",
                "Expiration": "2099-01-01T00:00:00Z",
            }))
            .into_response()
        }
        (Method::GET, "/latest/meta-data/placement/region") => "us-east-1".into_response(),
        (method, _) => {
            assert!(
                headers[header::AUTHORIZATION]
                    .to_str()
                    .unwrap()
                    .starts_with("AWS4-HMAC-SHA256 ")
            );
            let mut objects = fake.objects.lock().unwrap();
            match method {
                Method::PUT => {
                    objects.insert(key, body);
                    ([(header::ETAG, "\"fixture-etag\"")], "").into_response()
                }
                Method::GET | Method::HEAD => {
                    fake.reads.lock().unwrap().push(key.clone());
                    if key.contains("/denied/") {
                        return (
                            StatusCode::FORBIDDEN,
                            "<Error><Code>AccessDenied</Code></Error>",
                        )
                            .into_response();
                    }
                    match objects.get(&key) {
                        Some(bytes) => {
                            ([(header::ETAG, "\"fixture-etag\"")], bytes.clone()).into_response()
                        }
                        None => (
                            StatusCode::NOT_FOUND,
                            "<Error><Code>NoSuchKey</Code></Error>",
                        )
                            .into_response(),
                    }
                }
                _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
            }
        }
    }
}

async fn serve(app: Router) -> String {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    mj_core::runtime::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn upstream() -> (Arc<FakeS3>, String) {
    let fake = Arc::new(FakeS3::default());
    let endpoint = serve(
        Router::new()
            .fallback(fake_s3)
            .layer(axum::extract::DefaultBodyLimit::disable())
            .with_state(fake.clone()),
    )
    .await;
    (fake, endpoint)
}

fn test_proxy(endpoint: &str, prefix: &str, namespaces: &[&str], cache: &Path) -> Proxy {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let endpoint = endpoint.parse().unwrap();
    let remote = remote(
        "bucket".to_owned(),
        prefix.to_owned(),
        Some("us-east-1".to_owned()),
        namespaces.iter().map(|ns| (*ns).to_owned()).collect(),
    )
    .unwrap();
    let store = S3::new(
        &remote,
        "us-east-1".to_owned(),
        InstanceRoleCredentials::new(reqwest::Url::clone(&endpoint)).unwrap(),
        TemporaryCredentials {
            credentials: S3Credentials {
                access_key_id: "fixture-id".to_owned(),
                secret_access_key: "fixture-secret".to_owned(),
                session_token: Some("fixture-token".to_owned()),
            },
            expires_at: SystemTime::now() + Duration::from_secs(3600),
        },
        Some(endpoint),
    )
    .unwrap();
    Proxy {
        remote,
        store,
        cache: cache.to_owned(),
    }
}

fn blob_path(digest: &Digest) -> String {
    format!(
        "/v1/blobs/{}/{}/{}",
        digest.algorithm, digest.hash, digest.size
    )
}

const CHECK_FIXTURE: &str = "MJ_TEST_MBX_CHECK_FIXTURE";

#[test]
fn mbx_proxy_protocol_boundary() {
    if std::env::var_os(CHECK_FIXTURE).is_some() {
        let args = [
            "mbx-check",
            "--bucket",
            "bucket",
            "--prefix",
            "denied",
            "--namespace",
            NAMESPACE,
        ];
        let status = super::super::cli::main(args.into_iter().map(Into::into).collect()).unwrap();
        std::process::exit(status);
    }
    runtime().unwrap().block_on(protocol_boundary());
}

async fn protocol_boundary() {
    let (fake, endpoint) = upstream().await;
    let cache = tempfile::tempdir().unwrap();
    let proxy = test_proxy(&endpoint, "cache", &[NAMESPACE, OTHER], cache.path());
    let denied = test_proxy(&endpoint, "denied", &[NAMESPACE], cache.path());
    let check_endpoint = endpoint.clone();
    let check = mj_core::runtime::spawn_blocking(move || {
        execute(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "microvm::mbx_proxy::tests::mbx_proxy_protocol_boundary",
                    "--nocapture",
                ])
                .env(CHECK_FIXTURE, "1")
                .env("AWS_EC2_METADATA_DISABLED", "false")
                .env("AWS_EC2_METADATA_SERVICE_ENDPOINT", &check_endpoint)
                .env("MBX_REMOTE_S3_ENDPOINT", check_endpoint),
        )
    })
    .await
    .unwrap();
    assert_eq!(
        check.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(String::from_utf8_lossy(&check.stdout).lines().any(|line| line ==
        "role cannot read bucket/denied: AccessDenied (the role needs s3:GetObject on the prefix and s3:ListBucket on the bucket for the prefix)"));
    let denied_url = serve(router(denied)).await;
    let url = serve(router(proxy)).await;
    let digest = Digest::blake3(&LARGE);
    let path = blob_path(&digest);
    let key = format!("/bucket/cache/{NAMESPACE}{path}");
    let client = reqwest::Client::new();
    let get = |base: &str, ns: &str| {
        client
            .get(format!("{base}{path}"))
            .header(NAMESPACE_HEADER, ns)
    };
    for method in [Method::PUT, Method::POST] {
        assert_eq!(
            client
                .request(method, format!("{url}{path}"))
                .body(LARGE.to_vec())
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
    assert_eq!(
        client
            .get(format!("{url}/unknown"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        client
            .get(format!("{url}{path}"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&url, "foreign/repository")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(&url, "../project").send().await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        fake.reads.lock().unwrap().len(),
        1,
        "only Doctor should have reached S3"
    );
    let caps: Capabilities = client
        .get(format!("{url}/v1/capabilities"))
        .header(NAMESPACE_HEADER, NAMESPACE)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(caps.protocol.major, PROTOCOL_VERSION);
    assert_eq!(caps.features, Default::default());
    assert!(caps.compressors.is_empty());
    let response = get(&denied_url, NAMESPACE).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response.text().await.unwrap().contains("AccessDenied"));
    assert_eq!(
        get(&url, NAMESPACE).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );

    fake.objects
        .lock()
        .unwrap()
        .insert(key.clone(), Bytes::from(vec![0x5a; LARGE.len()]));
    let mismatch = get(&url, NAMESPACE).send().await.unwrap();
    assert_eq!(mismatch.status(), StatusCode::BAD_GATEWAY);
    assert!(
        mismatch
            .text()
            .await
            .unwrap()
            .contains("failed digest verification")
    );
    assert_eq!(fs::read_dir(cache.path()).unwrap().count(), 0);

    fake.objects
        .lock()
        .unwrap()
        .insert(key.clone(), Bytes::from_static(&LARGE));
    let bytes = get(&url, NAMESPACE)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(bytes.as_ref() == LARGE);
    fake.objects.lock().unwrap().remove(&key);
    assert_eq!(
        get(&url, OTHER).send().await.unwrap().status(),
        StatusCode::NOT_FOUND,
        "same digest in a different namespace"
    );
    let bytes = get(&url, NAMESPACE)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(bytes.as_ref() == LARGE);
}

fn execute(command: &mut Command) -> Output {
    run_with_input(command, &[]).unwrap()
}

fn checked(command: &mut Command) -> String {
    let output = execute(command);
    assert!(
        output.status.success(),
        "{command:?}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
#[ignore = "needs root with network and mount namespace privileges"]
fn mbx_proxy_device_binding() {
    const CHILD: &str = "MJ_TEST_MBX_BINDING_NETNS";
    if std::env::var_os(CHILD).is_none() {
        assert!(sys::is_root(), "run this ignored test with sudo -E");
        let name = "microvm::mbx_proxy::tests::mbx_proxy_device_binding";
        checked(
            Command::new("unshare")
                .args(["--net", "--mount", "--"])
                .arg(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", name, "--nocapture"])
                .env(CHILD, "1"),
        );
        return;
    }

    struct Links([String; 2]);
    impl Drop for Links {
        fn drop(&mut self) {
            for name in &self.0 {
                let output = execute(Command::new("ip").args(["netns", "delete", name]));
                if !output.status.success() {
                    eprintln!(
                        "delete test network namespace {name}: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            }
        }
    }
    let id = std::process::id();
    let links = Links([format!("mbx-a-{id}"), format!("mbx-b-{id}")]);
    checked(Command::new("ip").args(["link", "set", "lo", "up"]));
    for (index, ns) in links.0.iter().enumerate() {
        let dev = format!("mjt{index}");
        let host = 1 + 4 * index;
        let guest = host + 1;
        for args in [
            vec!["netns", "add", ns],
            vec![
                "link", "add", &dev, "type", "veth", "peer", "name", "guest", "netns", ns,
            ],
            vec!["addr", "add", &format!("198.18.0.{host}/30"), "dev", &dev],
            vec!["link", "set", &dev, "up"],
            vec![
                "-n",
                ns,
                "addr",
                "add",
                &format!("198.18.0.{guest}/30"),
                "dev",
                "guest",
            ],
            vec!["-n", ns, "link", "set", "guest", "up"],
            vec![
                "-n",
                ns,
                "route",
                "add",
                "default",
                "via",
                &format!("198.18.0.{host}"),
            ],
        ] {
            checked(Command::new("ip").args(args));
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let path = rt.block_on(async {
        let (fake, endpoint) = upstream().await;
        let path = blob_path(&Digest::blake3(&LARGE));
        for (index, namespace) in [NAMESPACE, OTHER].into_iter().enumerate() {
            fake.objects.lock().unwrap().insert(
                format!("/bucket/cache/{namespace}{path}"),
                Bytes::from_static(&LARGE),
            );
            let listener = bind(
                &format!("mjt{index}"),
                SocketAddrV4::new(Ipv4Addr::new(198, 18, 0, 1 + 4 * index as u8), PORT),
            )
            .unwrap();
            let app = router(test_proxy(&endpoint, "cache", &[namespace], cache.path()));
            mj_core::runtime::spawn(async move { axum::serve(listener, app).await.unwrap() });
        }
        path
    });
    let fetch = |netns: &str, host: u8, namespace: &str, source: Option<&str>| {
        let mut cmd = Command::new("ip");
        cmd.args([
            "netns",
            "exec",
            netns,
            "curl",
            "-fsS",
            "--max-time",
            "5",
            "--noproxy",
            "*",
            "-H",
            &format!("{NAMESPACE_HEADER}: {namespace}"),
            &format!("http://198.18.0.{host}:{PORT}{path}"),
        ]);
        if let Some(source) = source {
            cmd.args(["--interface", source]);
        }
        execute(&mut cmd)
    };
    assert!(fetch(&links.0[0], 1, NAMESPACE, None).stdout == LARGE);
    assert!(fetch(&links.0[1], 5, OTHER, None).stdout == LARGE);
    let denied = fetch(&links.0[0], 5, OTHER, None);
    assert_eq!(
        denied.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    // Add B's source to A, then arrange the return route on the isolated
    // host so a real TCP handshake can complete despite the spoofed source.
    checked(Command::new("ip").args([
        "-n",
        &links.0[0],
        "addr",
        "add",
        "198.18.0.6/32",
        "dev",
        "guest",
    ]));
    checked(Command::new("ip").args(["route", "add", "198.18.0.6/32", "dev", "mjt0"]));
    assert!(fetch(&links.0[0], 1, NAMESPACE, Some("198.18.0.6")).stdout == LARGE);
    let spoofed = fetch(&links.0[0], 5, OTHER, Some("198.18.0.6"));
    assert_eq!(
        spoofed.status.code(),
        Some(7),
        "{}",
        String::from_utf8_lossy(&spoofed.stderr)
    );
}

// Run with MJ_TEST_MBX_BIN=/absolute/path/to/mbx. Use the release version in
// mj-build-cache::mbx::MBX_VERSION (the same artifact Mjolnir stages for hosts).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the pinned mbx client; set MJ_TEST_MBX_BIN"]
async fn mbx_proxy_real_client() {
    let binary =
        std::env::var_os("MJ_TEST_MBX_BIN").expect("set MJ_TEST_MBX_BIN to the pinned mbx binary");
    assert_eq!(
        checked(Command::new(&binary).arg("--version")).trim(),
        format!("mbx {}", mj_build_cache::mbx::MBX_VERSION)
    );

    let (fake, endpoint) = upstream().await;
    let fixture = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let workspace = fixture.path().join("project");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"mbx_proxy_probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n",
    )
    .unwrap();
    // CI seeds a checked-in lockfile. Generate its fixture form before the
    // first build so both runs compute the same task identity.
    fs::write(workspace.join("Cargo.lock"), "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"mbx_proxy_probe\"\nversion = \"0.1.0\"\n").unwrap();
    fs::write(workspace.join("src/main.rs"), "fn main() { std::io::Write::write_all(&mut std::io::stdout(), include_bytes!(\"fixture.bin\")).unwrap(); }\n").unwrap();
    fs::write(workspace.join("src/fixture.bin"), LARGE).unwrap();
    fs::create_dir(fixture.path().join("proxy")).unwrap();
    let proxy = serve(router(test_proxy(
        &endpoint,
        "cache",
        &[NAMESPACE],
        &fixture.path().join("proxy"),
    )))
    .await;

    let build = |phase: &str, remote: &str, mode: &str| {
        let mut cmd = Command::new(&binary);
        for (key, _) in std::env::vars_os() {
            let text = key.to_string_lossy();
            if text.starts_with("MBX_") || text.starts_with("AWS_") {
                cmd.env_remove(&key);
            }
        }
        cmd.current_dir(&workspace)
            .args(["build", "--offline"])
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env("XDG_CONFIG_HOME", fixture.path().join("config"))
            .env(
                "MBX_CACHE_DIR",
                fixture.path().join(format!("{phase}-cache")),
            )
            .env("MBX_TARGET_ROOT", fixture.path().join("targets"))
            .env("MBX_REMOTE_URL", remote)
            .env("MBX_REMOTE_NAMESPACE", NAMESPACE)
            .env("MBX_REMOTE_MODE", mode)
            .env("CI", "true");
        if phase == "seed" {
            cmd.env("MBX_REMOTE_TRUST_WRITES", "true")
                .env("MBX_REMOTE_S3_ENDPOINT", &endpoint)
                .env("MBX_REMOTE_S3_REGION", "us-east-1")
                .env("AWS_ACCESS_KEY_ID", "fixture-id")
                .env("AWS_SECRET_ACCESS_KEY", "fixture-secret");
        }
        let output = execute(&mut cmd);
        let log = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.status.success(), "{phase}: {log}");
        log
    };

    let seed_log = build("seed", "s3://bucket/cache", "read-write");
    let object_count = fake.objects.lock().unwrap().len();
    assert!(object_count > 0, "{seed_log}");
    assert!(
        fake.objects
            .lock()
            .unwrap()
            .iter()
            .any(|(path, bytes)| path.contains("/blobs/") && bytes.len() > 65536)
    );
    // The new target directory must start cold as well as the new CAS.
    let target = workspace.join("target");
    if fs::symlink_metadata(&target).unwrap().is_symlink() {
        fs::remove_file(target).unwrap();
    } else {
        fs::remove_dir_all(target).unwrap();
    }
    let targets = fixture.path().join("targets");
    if targets.is_dir() {
        fs::remove_dir_all(targets).unwrap();
    }
    fake.reads.lock().unwrap().clear();
    let read_log = build("read", &proxy, "read-only");
    eprintln!("{read_log}");
    assert!(
        fake.reads
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.contains("/action-results/")),
        "{read_log}"
    );
    let objects = fake.objects.lock().unwrap();
    let reads = fake.reads.lock().unwrap();
    assert!(
        reads.iter().any(|path| path.contains("/blobs/")
            && objects.get(path).is_some_and(|bytes| bytes.len() > 65536)),
        "{read_log}\nseed: {seed_log}\nS3 reads: {reads:?}\nS3 objects: {:?}",
        objects.keys().collect::<Vec<_>>()
    );
    assert_eq!(objects.len(), object_count);
    assert!(
        read_log.lines().any(|line| line
            .starts_with("mbx[cache]: object cache: 1 hits, 0 misses, 1 prefetched;")
            && line.contains("0 B uploaded")),
        "{read_log}"
    );
}
