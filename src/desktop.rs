//! Native desktop shell for the existing remote viewer, opened by `mj app`.
//!
//! Policy and TLS verification live here so the server runtime in
//! `remote.rs` depends on a small, security-reviewed interface.

// Server-only and Android builds compile this module without consuming it.
#![cfg_attr(
    not(all(feature = "desktop-app", not(target_os = "android"))),
    allow(dead_code)
)]

use anyhow::{Context, Result, anyhow, bail};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;
use url::Url;

#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
use tao::event::{Event, WindowEvent};
#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
use tao::platform::run_return::EventLoopExtRunReturn;
#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
use tao::window::{Icon, WindowBuilder};
#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
use wry::{NewWindowResponse, WebView, WebViewBuilder};

const TLS_PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub(crate) struct DesktopShellOptions {
    pub origin: Url,
    /// DER encoding of the private certificate/CA trusted for this invocation.
    pub certificate_der: Vec<u8>,
    /// Name of the pre-authenticated viewer session cookie.
    pub bootstrap_cookie_name: &'static str,
    /// Signed session cookie value installed into the webview's ephemeral
    /// cookie store before the viewer origin loads, so the window opens
    /// authenticated without the viewer-code screen. Memory-only: it is never
    /// part of a URL, argv, or a log line.
    pub bootstrap_cookie_value: String,
}

#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DesktopShellExit {
    WindowClosed,
}

#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum ShellEvent {
    /// The bootstrap cookie is stored; the viewer origin may load now.
    CookieInstalled,
    /// Close the window and report success, as if the user closed it.
    Close,
    Fatal(String),
}

/// Handle for failing or closing the running shell from outside the event
/// loop — the CLI uses it to tear the window down when the app-owned server
/// dies first, and to close it on termination signals. Sends are ignored once
/// the window is gone.
#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
pub(crate) struct DesktopShellRemote {
    proxy: EventLoopProxy<ShellEvent>,
}

#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
impl DesktopShellRemote {
    /// Close the window and fail `run` with `message`.
    pub fn fail(&self, message: String) {
        let _ = self.proxy.send_event(ShellEvent::Fatal(message));
    }

    /// Close the window cleanly, as if the user closed it.
    pub fn close(&self) {
        let _ = self.proxy.send_event(ShellEvent::Close);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavigationDecision {
    Internal,
    External,
    Block,
}

/// Exact origin allowed to remain inside the application window.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OriginPolicy {
    scheme: String,
    host: String,
    port: u16,
}

impl OriginPolicy {
    fn new(origin: &Url) -> Result<Self> {
        if origin.scheme() != "https" {
            bail!("desktop viewer origin must use HTTPS");
        }
        if !origin.username().is_empty() || origin.password().is_some() {
            bail!("desktop viewer origin must not contain credentials");
        }
        let host = origin
            .host_str()
            .context("desktop viewer origin has no host")?
            .to_ascii_lowercase();
        let port = origin
            .port_or_known_default()
            .context("desktop viewer origin has no port")?;
        Ok(Self {
            scheme: origin.scheme().to_string(),
            host,
            port,
        })
    }

    fn decide(&self, candidate: &str) -> NavigationDecision {
        let Ok(candidate) = Url::parse(candidate) else {
            return NavigationDecision::Block;
        };
        if self.matches(&candidate) {
            NavigationDecision::Internal
        } else if matches!(candidate.scheme(), "http" | "https") {
            NavigationDecision::External
        } else {
            NavigationDecision::Block
        }
    }

    fn matches(&self, candidate: &Url) -> bool {
        candidate.scheme() == self.scheme
            && candidate
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(&self.host))
            && candidate.port_or_known_default() == Some(self.port)
            && candidate.username().is_empty()
            && candidate.password().is_none()
    }
}

/// Fail before creating a native window if the origin does not present the
/// certificate supplied by the app-owned server. Platform WebViews enforce the
/// same trust while rendering; this preflight turns setup failures into useful
/// terminal errors instead of an opaque blank window.
fn verify_pinned_tls(options: &DesktopShellOptions) -> Result<()> {
    let policy = OriginPolicy::new(&options.origin)?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(
            options.certificate_der.clone(),
        ))
        .context("load desktop TLS certificate")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("configure desktop TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(policy.host.clone())
        .map_err(|_| anyhow!("invalid desktop viewer TLS host: {}", policy.host))?;
    let addresses = options
        .origin
        .socket_addrs(|| Some(policy.port))
        .context("resolve desktop viewer origin")?;
    let address = addresses
        .into_iter()
        .next()
        .context("desktop viewer origin resolved to no addresses")?;
    let mut stream = TcpStream::connect_timeout(&address, TLS_PREFLIGHT_TIMEOUT)
        .with_context(|| format!("connect to desktop viewer at {address}"))?;
    stream
        .set_read_timeout(Some(TLS_PREFLIGHT_TIMEOUT))
        .context("set desktop TLS read timeout")?;
    stream
        .set_write_timeout(Some(TLS_PREFLIGHT_TIMEOUT))
        .context("set desktop TLS write timeout")?;
    let mut connection = rustls::ClientConnection::new(Arc::new(config), server_name)
        .context("create desktop TLS verifier")?;
    connection
        .complete_io(&mut stream)
        .context("verify desktop TLS certificate")?;
    Ok(())
}

#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
pub(crate) fn run(
    options: DesktopShellOptions,
    on_ready: impl FnOnce(DesktopShellRemote),
) -> Result<DesktopShellExit> {
    let policy = OriginPolicy::new(&options.origin)?;
    verify_pinned_tls(&options)?;

    let mut event_loop = EventLoopBuilder::<ShellEvent>::with_user_event().build();
    let event_proxy = event_loop.create_proxy();
    on_ready(DesktopShellRemote {
        proxy: event_loop.create_proxy(),
    });
    let window = WindowBuilder::new()
        .with_title("Mjolnir")
        .with_window_icon(Some(application_icon()?))
        .build(&event_loop)
        .context("create Mjolnir desktop window")?;

    let popup_policy = policy.clone();
    let builder = WebViewBuilder::new()
        .with_url("about:blank")
        .with_incognito(true)
        .with_new_window_req_handler(move |url, _features| {
            let _ = handle_navigation(&popup_policy, &url);
            NewWindowResponse::Deny
        });
    #[cfg(not(target_os = "macos"))]
    let builder = {
        let navigation_policy = policy.clone();
        builder.with_navigation_handler(move |url| {
            if url == "about:blank" {
                return true;
            }
            handle_navigation(&navigation_policy, &url)
        })
    };

    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder
            .build_gtk(window.gtk_window())
            .context("create Mjolnir WebKitGTK view")?
    };
    #[cfg(not(target_os = "linux"))]
    let webview = builder
        .build(&window)
        .context("create Mjolnir system WebView")?;

    #[cfg(target_os = "macos")]
    let _certificate_pin = install_platform_certificate_pin(
        &webview,
        &policy,
        &options.certificate_der,
        event_proxy.clone(),
    )?;
    #[cfg(not(target_os = "macos"))]
    install_platform_certificate_pin(
        &webview,
        &policy,
        &options.certificate_der,
        event_proxy.clone(),
    )?;
    // The viewer origin loads only after the cookie store confirms the
    // bootstrap cookie (via ShellEvent::CookieInstalled): loading earlier
    // would race the store and flash the login screen unauthenticated.
    install_bootstrap_cookie(&webview, &policy, &options, event_proxy.clone())?;

    let mut result = Ok(DesktopShellExit::WindowClosed);
    let _exit_code = event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            Event::UserEvent(ShellEvent::CookieInstalled) => {
                tracing::debug!("desktop bootstrap cookie stored; loading viewer origin");
                if let Err(error) = webview.load_url(options.origin.as_str()) {
                    result = Err(anyhow!(error).context("load Mjolnir desktop viewer"));
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(ShellEvent::Close) => *control_flow = ControlFlow::Exit,
            Event::UserEvent(ShellEvent::Fatal(message)) => {
                result = Err(anyhow!(message));
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
    drop(webview);
    drop(window);
    result
}

#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
fn application_icon() -> Result<Icon> {
    let image = image::load_from_memory_with_format(
        include_bytes!("icons/icon-192.png"),
        image::ImageFormat::Png,
    )?
    .into_rgba8();
    let (width, height) = image.dimensions();
    Icon::from_rgba(image.into_raw(), width, height).context("decode Mjolnir application icon")
}

#[cfg(all(feature = "desktop-app", not(target_os = "android")))]
fn handle_navigation(policy: &OriginPolicy, url: &str) -> bool {
    match policy.decide(url) {
        NavigationDecision::Internal => true,
        NavigationDecision::External => {
            if let Err(error) = webbrowser::open(url) {
                tracing::warn!(%url, %error, "failed to open external desktop link");
            }
            false
        }
        NavigationDecision::Block => false,
    }
}

#[cfg(all(feature = "desktop-app", target_os = "macos"))]
fn install_platform_certificate_pin(
    webview: &WebView,
    policy: &OriginPolicy,
    certificate_der: &[u8],
    event_proxy: EventLoopProxy<ShellEvent>,
) -> Result<impl Sized + use<>> {
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, ProtocolObject};
    use objc2::{DeclaredClass, MainThreadOnly, define_class, msg_send};
    use objc2_foundation::{
        MainThreadMarker, NSData, NSObjectProtocol, NSString, NSURLAuthenticationChallenge,
        NSURLCredential, NSURLSessionAuthChallengeDisposition,
    };
    use objc2_web_kit::{
        WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKWebView,
    };
    use wry::WebViewExtMacOS;

    struct MacNavigationDelegateIvars {
        certificate_der: Vec<u8>,
        policy: OriginPolicy,
        event_proxy: EventLoopProxy<ShellEvent>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = MacNavigationDelegateIvars]
        struct MacNavigationDelegate;

        unsafe impl NSObjectProtocol for MacNavigationDelegate {}

        unsafe impl WKNavigationDelegate for MacNavigationDelegate {
            #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
            fn decide_navigation(
                &self,
                _webview: &WKWebView,
                action: &WKNavigationAction,
                handler: &block2::DynBlock<dyn Fn(WKNavigationActionPolicy)>,
            ) {
                // Replacing Wry's navigation delegate also replaces its
                // download guard and builder navigation handler.
                let should_download = unsafe {
                    action.respondsToSelector(objc2::sel!(shouldPerformDownload))
                        && action.shouldPerformDownload()
                };
                if should_download {
                    handler.call((WKNavigationActionPolicy::Cancel,));
                    return;
                }

                let candidate = unsafe { action.request() }
                    .URL()
                    .and_then(|url| url.absoluteString())
                    .map(|url| url.to_string());
                let allow = candidate.as_deref().is_some_and(|url| {
                    url == "about:blank" || handle_navigation(&self.ivars().policy, url)
                });
                handler.call((if allow {
                    WKNavigationActionPolicy::Allow
                } else {
                    WKNavigationActionPolicy::Cancel
                },));
            }

            #[unsafe(method(webView:didReceiveAuthenticationChallenge:completionHandler:))]
            fn authenticate(
                &self,
                _webview: &WKWebView,
                challenge: &NSURLAuthenticationChallenge,
                handler: &block2::DynBlock<
                    dyn Fn(NSURLSessionAuthChallengeDisposition, *mut NSURLCredential),
                >,
            ) {
                handle_macos_authentication_challenge(self, challenge, handler);
            }
        }
    );

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecCertificateCreateWithData(
            allocator: *const std::ffi::c_void,
            data: *const objc2::runtime::AnyObject,
        ) -> *mut std::ffi::c_void;
        fn SecTrustSetAnchorCertificates(
            trust: *const std::ffi::c_void,
            anchors: *const objc2::runtime::AnyObject,
        ) -> i32;
        fn SecTrustSetAnchorCertificatesOnly(trust: *const std::ffi::c_void, only: bool) -> i32;
        fn SecTrustEvaluateWithError(
            trust: *const std::ffi::c_void,
            error: *mut *mut std::ffi::c_void,
        ) -> bool;
        fn CFRelease(cf: *const std::ffi::c_void);
    }

    fn handle_macos_authentication_challenge(
        delegate: &MacNavigationDelegate,
        challenge: &NSURLAuthenticationChallenge,
        handler: &block2::DynBlock<
            dyn Fn(NSURLSessionAuthChallengeDisposition, *mut NSURLCredential),
        >,
    ) {
        unsafe {
            let protection_space = challenge.protectionSpace();
            let server_trust = NSString::from_str("NSURLAuthenticationMethodServerTrust");
            if !protection_space
                .authenticationMethod()
                .isEqualToString(&server_trust)
            {
                handler.call((
                    NSURLSessionAuthChallengeDisposition::PerformDefaultHandling,
                    std::ptr::null_mut(),
                ));
                return;
            }

            let policy = &delegate.ivars().policy;
            let host_matches = protection_space
                .host()
                .to_string()
                .eq_ignore_ascii_case(&policy.host);
            let port_matches = protection_space.port() == policy.port as isize;
            let trust: *const std::ffi::c_void = msg_send![&*protection_space, serverTrust];
            let certificate_data = NSData::with_bytes(&delegate.ivars().certificate_der);
            let certificate = SecCertificateCreateWithData(
                std::ptr::null(),
                Retained::as_ptr(&certificate_data) as *const objc2::runtime::AnyObject,
            );

            let trusted =
                if host_matches && port_matches && !trust.is_null() && !certificate.is_null() {
                    let anchors: Retained<objc2::runtime::AnyObject> = msg_send![
                        objc2::runtime::AnyClass::get(c"NSArray").expect("NSArray is available"),
                        arrayWithObject: certificate as *mut objc2::runtime::AnyObject
                    ];
                    SecTrustSetAnchorCertificates(trust, Retained::as_ptr(&anchors)) == 0
                        && SecTrustSetAnchorCertificatesOnly(trust, true) == 0
                        && SecTrustEvaluateWithError(trust, std::ptr::null_mut())
                } else {
                    false
                };

            if !certificate.is_null() {
                CFRelease(certificate);
            }
            if trusted {
                let credential: *mut NSURLCredential = msg_send![
                    objc2::runtime::AnyClass::get(c"NSURLCredential")
                        .expect("NSURLCredential is available"),
                    credentialForTrust: trust
                ];
                handler.call((
                    NSURLSessionAuthChallengeDisposition::UseCredential,
                    credential,
                ));
            } else {
                let _ = delegate
                    .ivars()
                    .event_proxy
                    .send_event(ShellEvent::Fatal(format!(
                        "desktop WebView rejected an unexpected TLS certificate for https://{}:{}",
                        protection_space.host(),
                        protection_space.port()
                    )));
                handler.call((
                    NSURLSessionAuthChallengeDisposition::CancelAuthenticationChallenge,
                    std::ptr::null_mut(),
                ));
            }
        }
    }

    let mtm =
        MainThreadMarker::new().context("desktop WebView is not running on the main thread")?;
    let delegate = mtm
        .alloc::<MacNavigationDelegate>()
        .set_ivars(MacNavigationDelegateIvars {
            certificate_der: certificate_der.to_vec(),
            policy: policy.clone(),
            event_proxy,
        });
    let delegate: Retained<MacNavigationDelegate> = unsafe { msg_send![super(delegate), init] };
    let native_webview = webview.webview();
    unsafe {
        native_webview.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    }
    Ok(delegate)
}

#[cfg(all(feature = "desktop-app", target_os = "windows"))]
fn install_platform_certificate_pin(
    webview: &WebView,
    policy: &OriginPolicy,
    certificate_der: &[u8],
    event_proxy: EventLoopProxy<ShellEvent>,
) -> Result<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW,
        COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_CANCEL, ICoreWebView2_14,
    };
    use webview2_com::{ServerCertificateErrorDetectedEventHandler, take_pwstr};
    use windows_core::{Interface, PWSTR};
    use wry::WebViewExtWindows;

    let webview: ICoreWebView2_14 = webview
        .webview()
        .cast()
        .context("WebView2 runtime does not support certificate error handling")?;
    let expected = certificate_der.to_vec();
    let policy = policy.clone();
    let handler = ServerCertificateErrorDetectedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else {
            return Ok(());
        };
        let mut request_uri = PWSTR::null();
        unsafe { args.RequestUri(&mut request_uri)? };
        let request_uri = take_pwstr(request_uri);
        let uri_matches = Url::parse(&request_uri)
            .ok()
            .is_some_and(|url| policy.matches(&url));
        let certificate_matches = unsafe { args.ServerCertificate() }
            .and_then(|certificate| {
                let mut pem = PWSTR::null();
                unsafe { certificate.ToPemEncoding(&mut pem)? };
                Ok(take_pwstr(pem))
            })
            .ok()
            .and_then(|pem| decode_certificate_pem(&pem))
            .is_some_and(|actual| actual == expected);

        if uri_matches && certificate_matches {
            unsafe { args.SetAction(COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW)? };
        } else {
            unsafe { args.SetAction(COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_CANCEL)? };
            let _ = event_proxy.send_event(ShellEvent::Fatal(format!(
                "desktop WebView rejected an unexpected TLS certificate for {request_uri}"
            )));
        }
        Ok(())
    }));
    let mut token = 0;
    unsafe { webview.add_ServerCertificateErrorDetected(&handler, &mut token) }
        .context("install WebView2 certificate pin")?;
    Ok(())
}

#[cfg(all(feature = "desktop-app", target_os = "linux"))]
fn install_platform_certificate_pin(
    webview: &WebView,
    policy: &OriginPolicy,
    certificate_der: &[u8],
    event_proxy: EventLoopProxy<ShellEvent>,
) -> Result<()> {
    use gio::prelude::TlsCertificateExt;
    use webkit2gtk::{WebContextExt, WebViewExt};
    use wry::WebViewExtUnix;

    let expected = certificate_der.to_vec();
    let policy = policy.clone();
    webview.webview().connect_load_failed_with_tls_errors(
        move |webview, failing_uri, certificate, _errors| {
            let uri_matches = Url::parse(failing_uri)
                .ok()
                .is_some_and(|url| policy.matches(&url));
            let certificate_matches = certificate
                .certificate()
                .is_some_and(|actual| actual.as_ref() == expected.as_slice());
            if uri_matches
                && certificate_matches
                && let Some(context) = webview.context()
            {
                context.allow_tls_certificate_for_host(certificate, &policy.host);
                webview.load_uri(failing_uri);
                return true;
            }
            let _ = event_proxy.send_event(ShellEvent::Fatal(format!(
                "desktop WebView rejected an unexpected TLS certificate for {failing_uri}"
            )));
            true
        },
    );
    Ok(())
}

/// Store the bootstrap session cookie in the webview's ephemeral cookie
/// store, then post `ShellEvent::CookieInstalled` so the event loop loads the
/// viewer origin. The cookie mirrors the attributes the server itself mints —
/// host-only for the loopback origin, `Path=/`, `Secure`, `SameSite=Strict`,
/// no expiry so it dies with the webview — plus `HttpOnly` where the
/// platform cookie API exposes it (WKWebView's public property keys do not).
#[cfg(all(feature = "desktop-app", target_os = "macos"))]
fn install_bootstrap_cookie(
    webview: &WebView,
    policy: &OriginPolicy,
    options: &DesktopShellOptions,
    event_proxy: EventLoopProxy<ShellEvent>,
) -> Result<()> {
    use objc2::Message;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{
        NSDictionary, NSHTTPCookie, NSHTTPCookieDomain, NSHTTPCookieName, NSHTTPCookiePath,
        NSHTTPCookieSameSitePolicy, NSHTTPCookieSameSiteStrict, NSHTTPCookieSecure,
        NSHTTPCookieValue, NSString,
    };
    use wry::WebViewExtMacOS;

    let keys = unsafe {
        [
            NSHTTPCookieName,
            NSHTTPCookieValue,
            NSHTTPCookieDomain,
            NSHTTPCookiePath,
            NSHTTPCookieSecure,
            NSHTTPCookieSameSitePolicy,
        ]
    };
    let values: [Retained<AnyObject>; 6] = [
        NSString::from_str(options.bootstrap_cookie_name),
        NSString::from_str(&options.bootstrap_cookie_value),
        NSString::from_str(&policy.host),
        NSString::from_str("/"),
        NSString::from_str("TRUE"),
        unsafe { NSHTTPCookieSameSiteStrict }.retain(),
    ]
    .map(|value| value.into_super().into_super());
    let properties: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_retained_objects(&keys, &values);
    let cookie = unsafe { NSHTTPCookie::cookieWithProperties(&properties) }
        .context("construct desktop viewer session cookie")?;
    let completion = block2::RcBlock::new(move || {
        let _ = event_proxy.send_event(ShellEvent::CookieInstalled);
    });
    unsafe {
        webview
            .webview()
            .configuration()
            .websiteDataStore()
            .httpCookieStore()
            .setCookie_completionHandler(&cookie, Some(&*completion));
    }
    Ok(())
}

#[cfg(all(feature = "desktop-app", target_os = "windows"))]
fn install_bootstrap_cookie(
    webview: &WebView,
    policy: &OriginPolicy,
    options: &DesktopShellOptions,
    event_proxy: EventLoopProxy<ShellEvent>,
) -> Result<()> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        COREWEBVIEW2_COOKIE_SAME_SITE_KIND_STRICT, ICoreWebView2_2,
    };
    use windows_core::{HSTRING, Interface};
    use wry::WebViewExtWindows;

    let webview: ICoreWebView2_2 = webview
        .webview()
        .cast()
        .context("WebView2 runtime does not support cookie management")?;
    unsafe {
        let manager = webview
            .CookieManager()
            .context("open WebView2 cookie manager")?;
        let cookie = manager
            .CreateCookie(
                &HSTRING::from(options.bootstrap_cookie_name),
                &HSTRING::from(options.bootstrap_cookie_value.as_str()),
                &HSTRING::from(policy.host.as_str()),
                &HSTRING::from("/"),
            )
            .context("construct desktop viewer session cookie")?;
        cookie
            .SetIsSecure(true)
            .context("mark desktop viewer session cookie secure")?;
        cookie
            .SetIsHttpOnly(true)
            .context("mark desktop viewer session cookie HTTP-only")?;
        cookie
            .SetSameSite(COREWEBVIEW2_COOKIE_SAME_SITE_KIND_STRICT)
            .context("restrict desktop viewer session cookie site")?;
        manager
            .AddOrUpdateCookie(&cookie)
            .context("store desktop viewer session cookie")?;
    }
    let _ = event_proxy.send_event(ShellEvent::CookieInstalled);
    Ok(())
}

#[cfg(all(feature = "desktop-app", target_os = "linux"))]
fn install_bootstrap_cookie(
    webview: &WebView,
    policy: &OriginPolicy,
    options: &DesktopShellOptions,
    event_proxy: EventLoopProxy<ShellEvent>,
) -> Result<()> {
    use webkit2gtk::{CookieManagerExt, WebViewExt, WebsiteDataManagerExt};
    use wry::WebViewExtUnix;

    let manager = webview
        .webview()
        .website_data_manager()
        .and_then(|data| data.cookie_manager())
        .context("desktop webview exposes no cookie manager")?;
    // Negative max-age means no expiry: a session cookie that dies with the
    // webview, like every cookie the ephemeral desktop server itself issues.
    let mut cookie = soup::Cookie::new(
        options.bootstrap_cookie_name,
        &options.bootstrap_cookie_value,
        &policy.host,
        "/",
        -1,
    );
    cookie.set_secure(true);
    cookie.set_http_only(true);
    cookie.set_same_site_policy(soup::SameSitePolicy::Strict);
    manager.add_cookie(&mut cookie, gio::Cancellable::NONE, move |result| {
        let event = match result {
            Ok(()) => ShellEvent::CookieInstalled,
            Err(error) => {
                ShellEvent::Fatal(format!("install desktop viewer session cookie: {error}"))
            }
        };
        let _ = event_proxy.send_event(event);
    });
    Ok(())
}

#[cfg(any(test, target_os = "windows"))]
fn decode_certificate_pem(pem: &str) -> Option<Vec<u8>> {
    use base64::Engine;

    let encoded = pem
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("-----"))
        .collect::<String>();
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn origin_policy_allows_only_the_exact_https_origin() {
        let policy = OriginPolicy::new(&Url::parse("https://localhost:43123/").unwrap()).unwrap();
        assert_eq!(
            policy.decide("https://LOCALHOST:43123/sessions?q=1#active"),
            NavigationDecision::Internal
        );
        assert_eq!(
            policy.decide("https://localhost:43124/"),
            NavigationDecision::External
        );
        assert_eq!(
            policy.decide("http://localhost:43123/"),
            NavigationDecision::External
        );
        assert_eq!(
            policy.decide("javascript:alert(1)"),
            NavigationDecision::Block
        );
        assert_eq!(policy.decide("not a url"), NavigationDecision::Block);
    }

    #[test]
    fn origin_policy_rejects_insecure_or_credentialed_origins() {
        assert!(OriginPolicy::new(&Url::parse("http://localhost:1234/").unwrap()).is_err());
        assert!(OriginPolicy::new(&Url::parse("https://user@localhost:1234/").unwrap()).is_err());
    }

    #[test]
    fn certificate_pem_decoder_ignores_armor_and_whitespace() {
        let bytes = b"desktop certificate";
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let pem = format!("-----BEGIN CERTIFICATE-----\n{encoded}\n-----END CERTIFICATE-----\n");
        assert_eq!(
            decode_certificate_pem(&pem).as_deref(),
            Some(bytes.as_slice())
        );
    }

    #[test]
    fn tls_preflight_accepts_only_the_configured_certificate() {
        let expected = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
            .expect("expected certificate");
        let expected_der = expected.cert.der().to_vec();
        let (origin, server) = spawn_tls_server(&expected);
        verify_pinned_tls(&shell_options(origin, expected_der))
            .expect("expected certificate must pass");
        server.join().expect("server thread");

        let presented = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
            .expect("presented certificate");
        let unexpected = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()])
            .expect("unexpected certificate");
        let (origin, server) = spawn_tls_server(&presented);
        let error = verify_pinned_tls(&shell_options(origin, unexpected.cert.der().to_vec()))
            .expect_err("different certificate must fail");
        assert!(format!("{error:#}").contains("verify desktop TLS certificate"));
        server.join().expect("server thread");
    }

    fn shell_options(origin: Url, certificate_der: Vec<u8>) -> DesktopShellOptions {
        DesktopShellOptions {
            origin,
            certificate_der,
            bootstrap_cookie_name: "mj_desktop_session",
            bootstrap_cookie_value: "test-cookie".to_string(),
        }
    }

    fn spawn_tls_server(certificate: &rcgen::CertifiedKey) -> (Url, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind TLS listener");
        let address = listener.local_addr().expect("TLS listener address");
        let certs = vec![CertificateDer::from(certificate.cert.der().to_vec())];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certificate.key_pair.serialize_der(),
        ));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("TLS versions")
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .expect("server certificate");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept TLS client");
            let mut connection =
                rustls::ServerConnection::new(Arc::new(config)).expect("server connection");
            let _ = connection.complete_io(&mut stream);
        });
        (
            Url::parse(&format!("https://{address}/")).expect("origin URL"),
            server,
        )
    }
}
