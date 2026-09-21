use super::*;

mod revocations;
pub(super) use revocations::ViewerRevocations;

pub(super) fn code_locked(state: &ServerState) -> bool {
    state
        .code_guard
        .lock()
        .expect("viewer code guard poisoned")
        .locked_at(Instant::now())
}

pub(super) fn record_code_failure(state: &ServerState) {
    state
        .code_guard
        .lock()
        .expect("viewer code guard poisoned")
        .record_failure_at(Instant::now());
}

pub(super) fn reset_code_failures(state: &ServerState) {
    *state.code_guard.lock().expect("viewer code guard poisoned") = CodeGuard::default();
}

pub(super) fn generate_viewer_code() -> AnyResult<String> {
    // Rejection sampling avoids modulo bias in the deliberately small code
    // space. Online attempts are separately rate-limited.
    const RANGE: u32 = 1_000_000;
    const LIMIT: u32 = u32::MAX - (u32::MAX % RANGE);
    loop {
        let mut bytes = [0_u8; 4];
        getrandom::fill(&mut bytes)
            .map_err(|error| anyhow::anyhow!("generate Mjolnir viewer code: {error}"))?;
        let value = u32::from_le_bytes(bytes);
        if value < LIMIT {
            return Ok(format!("{:06}", value % RANGE));
        }
    }
}

/// A stable credential with a signing purpose separate from session cookies.
pub(super) fn derive_login_token(key: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    mac.update(b"mjolnir:viewer-login-token:v1");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

pub(super) fn generate_cookie_key() -> AnyResult<[u8; COOKIE_KEY_BYTES]> {
    let mut key = [0_u8; COOKIE_KEY_BYTES];
    getrandom::fill(&mut key)
        .map_err(|error| anyhow::anyhow!("generate Mjolnir cookie key: {error}"))?;
    Ok(key)
}

/// A random name for one viewer, minted at unlock.
///
/// The cookie used to sign only an expiry, which meant two phones unlocking in
/// the same second received byte-identical cookies and one phone's cookie
/// changed on every login. Nothing keyed to it could mean anything: a draft
/// would have leaked between phones and vanished on re-login. This is the
/// identity everything per-viewer hangs from.
pub(super) fn generate_viewer_id() -> AnyResult<String> {
    let mut id = [0_u8; 16];
    getrandom::fill(&mut id)
        .map_err(|error| anyhow::anyhow!("generate Mjolnir viewer id: {error}"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id))
}

pub(super) fn signed_cookie_value(key: &[u8], viewer: &str, expiry: u64) -> String {
    // The signed text separates its parts with a character the parts cannot
    // contain, so no two different pairs can produce the same signed text.
    let canonical = format!("{viewer}|{expiry}");
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts arbitrary key lengths");
    mac.update(canonical.as_bytes());
    let signature =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    format!("{viewer}.{expiry}.{signature}")
}

#[cfg(test)]
pub(super) fn session_cookie_valid(key: &[u8], value: &str, now: u64) -> bool {
    cookie_viewer(key, value, now).is_some()
}

/// Mint a signed viewer-session cookie value without the HTTP login flow.
///
/// The desktop shell pre-authorizes its WebView with this: it runs as the same
/// user as the daemon and reads the same persisted signing key, so possession
/// of the key is the credential. The cookie carries the ephemeral TTL — a
/// desktop window re-mints on every launch, so it never needs a long life.
pub fn mint_desktop_session_cookie(key: &[u8]) -> AnyResult<String> {
    let viewer = generate_viewer_id()?;
    Ok(signed_cookie_value(
        key,
        &viewer,
        now_unix().saturating_add(EPHEMERAL_SESSION_TTL.as_secs()),
    ))
}

/// The viewer a cookie names, or `None` when the cookie is not valid.
pub(super) fn cookie_viewer(key: &[u8], value: &str, now: u64) -> Option<String> {
    validate_cookie(key, Some(value), now)
        .ok()
        .map(|cookie| cookie.viewer)
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ViewerCookie {
    viewer: String,
    expiry: u64,
}

fn validate_cookie(
    key: &[u8],
    value: Option<&str>,
    now: u64,
) -> Result<ViewerCookie, &'static str> {
    let value = value.ok_or("absent")?;
    let [viewer, expiry, _] = value.split('.').collect::<Vec<_>>()[..] else {
        return Err("malformed");
    };
    let expiry = expiry.parse::<u64>().map_err(|_| "malformed")?;
    let expected = signed_cookie_value(key, viewer, expiry);
    if !constant_time_eq(expected.as_bytes(), value.as_bytes()) {
        return Err("bad_signature");
    }
    if now >= expiry {
        return Err("expired");
    }
    Ok(ViewerCookie {
        viewer: viewer.to_owned(),
        expiry,
    })
}

fn request_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| cookie_value(header, COOKIE_NAME))
}

pub(super) fn authenticated_viewer(
    state: &ServerState,
    headers: &HeaderMap,
) -> Result<ViewerCookie, ApiError> {
    let now = now_unix();
    let cookie = validate_cookie(&state.cookie_key, request_cookie(headers), now)
        .and_then(|cookie| {
            if state.viewer_revocations.contains(&cookie.viewer, now) {
                Err("revoked")
            } else {
                Ok(cookie)
            }
        })
        .map_err(|reason| {
            tracing::debug!(reason, "viewer session cookie rejected");
            ApiError::unauthorized()
        })?;
    Ok(cookie)
}

/// Only phone logins explicitly opt into sliding expiry. Legacy and desktop
/// cookies have no signed policy marker, so retain their original expiry.
fn renewal_policy(state: &ServerState, viewer: &str) -> Option<(Duration, bool)> {
    if viewer.starts_with("session:") {
        Some((EPHEMERAL_SESSION_TTL, false))
    } else if viewer.starts_with("phone:") {
        Some(if state.session_ttl.is_zero() {
            (EPHEMERAL_SESSION_TTL, false)
        } else {
            (state.session_ttl, true)
        })
    } else {
        None
    }
}

pub(super) fn renew_viewer_response(
    state: &ServerState,
    cookie: &ViewerCookie,
    response: &mut Response<Body>,
) -> Result<(), ApiError> {
    let now = now_unix();
    if state.viewer_revocations.contains(&cookie.viewer, now)
        || cookie.expiry <= now
        || renewal_policy(state, &cookie.viewer).is_none()
    {
        return Ok(());
    }
    let renewed = viewer_session_cookie(state, &cookie.viewer, now)?;
    response.headers_mut().entry(SET_COOKIE).or_insert(renewed);
    Ok(())
}

pub(super) fn viewer_session_cookie(
    state: &ServerState,
    viewer: &str,
    now: u64,
) -> Result<HeaderValue, ApiError> {
    let (validity, persistent) =
        renewal_policy(state, viewer).expect("new phone cookies carry a renewal policy");
    let value = signed_cookie_value(
        &state.cookie_key,
        viewer,
        now.saturating_add(validity.as_secs()),
    );
    session_cookie_header(
        &value,
        persistent.then_some(validity.as_secs()),
        state.secure_cookie,
    )
}

pub(super) async fn revoke_viewer(
    state: &ServerState,
    headers: &HeaderMap,
) -> Result<(), ApiError> {
    let now = now_unix();
    let Ok(cookie) = validate_cookie(&state.cookie_key, request_cookie(headers), now) else {
        return Ok(());
    };
    // Include any renewed response that was generated before this logout but
    // has not reached the browser yet, even if it used a newer cookie expiry.
    let renewal_ttl = renewal_policy(state, &cookie.viewer).map_or(Duration::ZERO, |(ttl, _)| ttl);
    let revocations = state.viewer_revocations.clone();
    tokio::task::spawn_blocking(move || {
        let result = revocations.revoke(cookie.viewer, cookie.expiry, renewal_ttl);
        if let Err(error) = &result {
            // Report even if the HTTP caller disconnected while disk I/O ran.
            tracing::error!(%error, "could not persist viewer logout");
        }
        result
    })
    .await
    .map_err(|error| {
        tracing::error!(%error, "viewer logout task failed");
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not persist logout; retry",
        )
    })?
    .map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not persist logout; retry",
        )
    })
}

pub(super) fn session_cookie_header(
    value: &str,
    max_age: Option<u64>,
    secure: bool,
) -> Result<HeaderValue, ApiError> {
    let mut header = format!("{COOKIE_NAME}={value}; Path=/; HttpOnly; SameSite=Strict");
    if secure {
        header.push_str("; Secure");
    }
    if let Some(max_age) = max_age {
        header.push_str(&format!("; Max-Age={max_age}"));
    }
    HeaderValue::from_str(&header)
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "cookie creation failed"))
}

pub(super) fn clear_cookie_header(secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict{secure}; Max-Age=0"
    ))
    .expect("static cookie header is valid")
}

/// The stored-state key for the viewer making this request.
pub(super) fn viewer_client_id(state: &ServerState, headers: &HeaderMap) -> Option<String> {
    let cookie = headers
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| cookie_value(header, COOKIE_NAME))?;
    cookie_viewer(&state.cookie_key, cookie, now_unix()).map(|viewer| format!("phone:{viewer}"))
}

pub(super) fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(cookie_name, _)| *cookie_name == name)
        .map(|(_, value)| value)
}

pub(super) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

pub(super) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_rejection_reasons_distinguish_eviction_expiry_and_tampering() {
        let key = b"a test key";
        assert_eq!(validate_cookie(key, None, 100), Err("absent"));
        assert_eq!(validate_cookie(key, Some("bad"), 100), Err("malformed"));
        assert_eq!(
            validate_cookie(key, Some("viewer.bad.signature"), 100),
            Err("malformed")
        );
        let expired = signed_cookie_value(key, "viewer", 100);
        assert_eq!(validate_cookie(key, Some(&expired), 100), Err("expired"));
        let wrong = signed_cookie_value(b"other key", "viewer", 200);
        assert_eq!(
            validate_cookie(key, Some(&wrong), 100),
            Err("bad_signature")
        );
        let valid = signed_cookie_value(key, "viewer", 200);
        assert_eq!(
            validate_cookie(key, Some(&valid), 100),
            Ok(ViewerCookie {
                viewer: "viewer".into(),
                expiry: 200
            })
        );
    }
}
