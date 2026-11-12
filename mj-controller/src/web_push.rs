//! Daemon-owned encrypted Web Push delivery. Durable intents precede HTTPS.
use crate::{daemon::RuntimeState, database as db};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::{
    aead, agreement, hkdf,
    rand::{SecureRandom, SystemRandom},
    signature::{self, KeyPair},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::{Arc, OnceLock},
    time::Duration,
};

static WAKE: OnceLock<tokio::sync::Notify> = OnceLock::new();
pub(crate) fn wake() {
    WAKE.get_or_init(Default::default).notify_one();
}
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .context("Web Push storage task panicked")?
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
    #[serde(default, rename = "expirationTime")]
    pub expiration_time: Option<f64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}
impl Subscription {
    pub(crate) fn validate(&self) -> Result<()> {
        let url = url::Url::parse(&self.endpoint)?;
        ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none(),
            "push endpoint must be HTTPS"
        );
        let key = B64.decode(&self.keys.p256dh)?;
        ensure!(key.len() == 65 && key[0] == 4, "invalid push public key");
        ensure!(
            B64.decode(&self.keys.auth)?.len() == 16,
            "invalid push authentication secret"
        );
        encrypt(self, b"")?; // ring also validates that the point lies on P-256.
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Notice {
    pub title: String,
    pub body: String,
    pub session_id: Option<String>,
    pub url: String,
    pub tag: String,
}

/// A fixed-size URL reference keeps summary push payloads independent of count.
pub(crate) fn summary_url(endpoint: &str, episode: &str) -> String {
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        format!("{endpoint}\0{episode}").as_bytes(),
    );
    format!("/?push-summary={}", B64.encode(digest.as_ref()))
}
pub(crate) struct Vapid(signature::EcdsaKeyPair);
impl Vapid {
    pub(crate) fn load() -> Result<Self> {
        let path = mj_core::config::data_dir().join("web-push-vapid.pk8");
        std::fs::create_dir_all(path.parent().context("key directory")?)?;
        if !path.exists() {
            let document = signature::EcdsaKeyPair::generate_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                &SystemRandom::new(),
            )
            .map_err(|_| anyhow::anyhow!("generate VAPID key"))?;
            // Atomic no-clobber publication also handles concurrent public-key requests.
            let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
            use std::io::Write;
            file.write_all(document.as_ref())?;
            file.as_file().sync_all()?;
            match file.persist_noclobber(&path) {
                Ok(_) => {}
                Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.error.into()),
            }
        }
        mj_core::config::sync_directory(path.parent().context("key directory")?)?;
        let bytes = std::fs::read(&path)?;
        Ok(Self(
            signature::EcdsaKeyPair::from_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                &bytes,
                &SystemRandom::new(),
            )
            .map_err(|_| anyhow::anyhow!("read VAPID key"))?,
        ))
    }
    pub(crate) fn public(&self) -> String {
        B64.encode(self.0.public_key().as_ref())
    }
    fn authorization(&self, endpoint: &str) -> Result<String> {
        let origin = url::Url::parse(endpoint)?.origin().ascii_serialization();
        // RFC 8292 permits at most 24 hours; use that documented validity.
        let claims = serde_json::json!({"aud":origin,"exp":chrono::Utc::now().timestamp()+24*60*60,"sub":"https://github.com/BrokkAi/mjolnir"});
        let signed = format!(
            "{}.{}",
            B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#),
            B64.encode(serde_json::to_vec(&claims)?)
        );
        let sig = self
            .0
            .sign(&SystemRandom::new(), signed.as_bytes())
            .map_err(|_| anyhow::anyhow!("sign VAPID JWT"))?;
        Ok(format!(
            "vapid t={signed}.{}, k={}",
            B64.encode(sig.as_ref()),
            self.public()
        ))
    }
}
struct Length(usize);
impl hkdf::KeyType for Length {
    fn len(&self) -> usize {
        self.0
    }
}
fn expand(salt: &[u8], input: &[u8], info: &[u8], len: usize) -> Result<Vec<u8>> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(input);
    let infos = [info];
    let okm = prk
        .expand(&infos, Length(len))
        .map_err(|_| anyhow::anyhow!("HKDF expand"))?;
    let mut out = vec![0; len];
    okm.fill(&mut out)
        .map_err(|_| anyhow::anyhow!("HKDF fill"))?;
    Ok(out)
}
fn encrypt(subscription: &Subscription, payload: &[u8]) -> Result<Vec<u8>> {
    let rng = SystemRandom::new();
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| anyhow::anyhow!("push ECDH key"))?;
    let public = private
        .compute_public_key()
        .map_err(|_| anyhow::anyhow!("push ECDH public key"))?;
    let receiver = B64.decode(&subscription.keys.p256dh)?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &receiver);
    let shared = agreement::agree_ephemeral(private, &peer, |bytes| bytes.to_vec())
        .map_err(|_| anyhow::anyhow!("invalid push ECDH key"))?;
    encrypt_shared(
        &shared,
        &B64.decode(&subscription.keys.auth)?,
        &receiver,
        public.as_ref(),
        payload,
    )
}
fn encrypt_shared(
    shared: &[u8],
    auth: &[u8],
    receiver: &[u8],
    sender: &[u8],
    payload: &[u8],
) -> Result<Vec<u8>> {
    let mut info = b"WebPush: info\0".to_vec();
    info.extend(receiver);
    info.extend(sender);
    let ikm = expand(auth, shared, &info, 32)?;
    let mut salt = [0; 16];
    SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| anyhow::anyhow!("push salt"))?;
    encrypt_record(&ikm, salt, sender, payload)
}
fn encrypt_record(ikm: &[u8], salt: [u8; 16], sender: &[u8], payload: &[u8]) -> Result<Vec<u8>> {
    let key = expand(&salt, ikm, b"Content-Encoding: aes128gcm\0", 16)?;
    let nonce = expand(&salt, ikm, b"Content-Encoding: nonce\0", 12)?;
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &key)
            .map_err(|_| anyhow::anyhow!("push AES key"))?,
    );
    let mut record = payload.to_vec();
    record.push(2);
    key.seal_in_place_append_tag(
        aead::Nonce::try_assume_unique_for_key(&nonce)
            .map_err(|_| anyhow::anyhow!("push nonce"))?,
        aead::Aad::empty(),
        &mut record,
    )
    .map_err(|_| anyhow::anyhow!("push encryption"))?;
    // RFC 8291 requires one record. rs exceeds the final encrypted record length.
    let rs = u32::try_from(record.len() + 1)?;
    let mut out = salt.to_vec();
    out.extend(rs.to_be_bytes());
    out.push(u8::try_from(sender.len())?);
    out.extend(sender);
    out.extend(record);
    Ok(out)
}
fn phone_line(text: &str, fallback: &str) -> String {
    let text = text.lines().next().unwrap_or("").trim();
    // Presentation length, not a transport or admission limit.
    let text = text.chars().take(180).collect::<String>();
    if text.is_empty() {
        fallback.into()
    } else {
        text
    }
}

fn eligible(attention: &mj_core::attention::SessionAttention) -> bool {
    // Extend this policy expression when enabling another attention category.
    matches!(
        attention,
        mj_core::attention::SessionAttention::NeedsInput
            | mj_core::attention::SessionAttention::Important
    )
}

fn episodes(runtime: &mj_client::runtime_feed::RuntimeProjection) -> Vec<(String, String, Notice)> {
    use mj_core::attention::SessionAttention;
    runtime
        .metadata
        .session_attention
        .iter()
        .filter_map(|(id, attention)| {
            if !eligible(attention) {
                return None;
            }
            let record = runtime.records.get(id)?;
            let mut identities = Vec::new();
            let mut question = String::new();
            let belongs_to_session = |source: &str| {
                let mut owner = source;
                let mut seen = BTreeSet::new();
                while owner != id && seen.insert(owner.to_owned()) {
                    let Some(parent) = runtime.subagents.get(owner) else {
                        break;
                    };
                    owner = &parent.parent_session_id;
                }
                owner == id
            };
            for (source, tail) in &runtime.transcripts {
                if !belongs_to_session(source) {
                    continue;
                }
                for form in &tail.header.pending_elicitations {
                    identities.push(format!(
                        "{source}:form:{}:{}",
                        form.id,
                        form.question_text()
                    ));
                    if question.is_empty() {
                        question = form.question_text();
                    }
                }
                if tail.header.pending_elicitations.is_empty()
                    && source == id
                    && let Some(outcome) = &tail.header.last_turn_outcome
                {
                    identities.push(format!("{source}:turn:{}", outcome.completed_ordinal));
                }
            }
            for (source, native) in &runtime.native_agents {
                if native.pending_input && belongs_to_session(&native.agent.owner_session_id) {
                    identities.push(format!("{source}:native:{}", native.generation_ordinal));
                }
            }
            identities.sort();
            // The database owner assigns a persistent sequence on entry or a
            // newly added source. Removing an answered form is not a new episode.
            let identity = serde_json::to_string(&(attention, identities))
                .expect("serializable attention sources");
            let mut reply = String::new();
            if let Some(tail) = runtime.transcripts.get(id) {
                for (_, item) in &tail.items {
                    if let mj_core::state::TranscriptBody::Agent { chunks, .. } = &item.body {
                        reply = mj_core::transcript::materialized_chunks_text(chunks);
                    }
                }
            }
            let question_chunks = runtime
                .sessions
                .get(id)
                .and_then(|view| view.operational.as_deref())
                .and_then(|live| live.assessment.as_deref())
                .and_then(|assessment| assessment.question_message.as_deref())
                .and_then(|message| {
                    runtime
                        .transcripts
                        .get(id)?
                        .items
                        .values()
                        .find(|item| item.stable_id == message)
                })
                .and_then(|item| match &item.body {
                    mj_core::state::TranscriptBody::Agent { chunks, .. } => Some(chunks),
                    _ => None,
                });
            if question.is_empty()
                && let Some(chunks) = question_chunks
            {
                question = mj_core::transcript::materialized_chunks_text(chunks);
            }
            let body = if *attention == SessionAttention::NeedsInput {
                phone_line(&question, "Needs your input")
            } else {
                phone_line(&reply, "Has a reply for you")
            };
            Some((
                id.clone(),
                identity,
                Notice {
                    title: record.listed_title().to_owned(),
                    body,
                    session_id: Some(id.clone()),
                    url: format!("/#conversation/{id}"),
                    tag: id.clone(),
                },
            ))
        })
        .collect()
}

pub(crate) async fn run(state: Arc<RuntimeState>) -> Result<()> {
    let vapid = Arc::new(blocking(Vapid::load).await?);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut revisions = state.revisions();
    let mut sends = tokio::task::JoinSet::new();
    blocking(db::push_recover).await?;
    loop {
        let runtime = state.runtime_publication()?;
        let presence = runtime.metadata.operator_presence;
        let unavailable = runtime
            .metadata
            .session_attention
            .iter()
            .filter(|(_, attention)| {
                !eligible(attention)
                    && matches!(
                        attention,
                        mj_core::attention::SessionAttention::Unknown
                            | mj_core::attention::SessionAttention::Unreachable
                            | mj_core::attention::SessionAttention::Failed
                    )
            })
            .map(|(id, _)| id.clone())
            .collect();
        blocking(move || db::push_observe(presence, episodes(&runtime), unavailable)).await?;
        let due = blocking(|| db::push_claim(chrono::Utc::now().timestamp())).await?;
        for delivery in due {
            let client = client.clone();
            let vapid = vapid.clone();
            sends.spawn(async move { deliver(&client, &vapid, delivery).await });
        }
        let next = blocking(db::push_next_due).await?;
        // Serialize each device/tag so an older request cannot replace a newer
        // card. Other sessions and devices continue independently.
        let deadline = next.and_then(|at| {
            let delay = Duration::from_secs(
                at.saturating_sub(chrono::Utc::now().timestamp()).max(0) as u64,
            );
            // A terminal rejection uses i64::MAX. If the platform cannot
            // represent that instant, only a real state change wakes the task.
            tokio::time::Instant::now().checked_add(delay)
        });
        tokio::select! {
            changed=revisions.changed()=>{changed.context("push runtime feed closed")?;},
            _=WAKE.get_or_init(Default::default).notified()=>{},
            completed=sends.join_next(),if !sends.is_empty()=>{let result=completed.context("push task disappeared")??;result?;},
            _=async {match deadline {Some(deadline)=>tokio::time::sleep_until(deadline).await,None=>std::future::pending().await}}=>{},
        }
    }
}
async fn deliver(
    client: &reqwest::Client,
    vapid: &Vapid,
    delivery: db::PushDelivery,
) -> Result<()> {
    let endpoint = delivery.subscription.endpoint.clone();
    let episode = delivery.episode.clone();
    let body = encrypt(&delivery.subscription, delivery.payload.as_bytes())?;
    // At-least-once: a crash after remote acceptance but before this receipt can
    // repeat a send. Stable session tags replace the card; they cannot prevent
    // a second alert after dismissal. Never discard unknown outcomes.
    let response = client
        .post(&endpoint)
        .header("Authorization", vapid.authorization(&endpoint)?)
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        // RFC 8030 requires TTL. Retain notifications for one day when a phone is offline.
        .header("TTL", "86400")
        .body(body)
        .send()
        .await;
    let mut service_status = None;
    let mut retry_after = None;
    let error;
    match response {
        Ok(response) => {
            let status = response.status();
            service_status = Some(status.as_u16());
            let accepted = status.is_success();
            error = (!accepted).then(|| format!("push service returned {status}"));
            if status.as_u16() == 429 || status.is_server_error() {
                retry_after = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|value| {
                        value
                            .parse::<i64>()
                            .ok()
                            .map(|seconds| {
                                chrono::Utc::now()
                                    .timestamp()
                                    .saturating_add(seconds.max(0))
                            })
                            .or_else(|| {
                                chrono::DateTime::parse_from_rfc2822(value)
                                    .ok()
                                    .map(|date| date.timestamp())
                            })
                    });
            } else if !accepted {
                // Permanent rejection: keep the error visible until re-enrollment.
                retry_after = Some(i64::MAX);
            }
        }
        Err(failure) => {
            // Subscription URLs are capabilities: retain diagnostics without them.
            error = Some(format!(
                "push transport failed; outcome unknown: {}",
                failure.without_url()
            ));
        }
    }
    if let Some(error) = &error {
        tracing::warn!(%error,"Web Push delivery deferred");
    }
    // Exponential backoff has no guessed retry cap. The persisted due time
    // prevents hot retry loops when the service omits Retry-After.
    let seconds = 1_i64.checked_shl(delivery.failures).unwrap_or(i64::MAX);
    let at = retry_after.unwrap_or_else(|| chrono::Utc::now().timestamp().saturating_add(seconds));
    blocking(move || {
        db::push_result(
            delivery.subscription,
            delivery.enrollment,
            episode,
            service_status,
            at,
            error,
        )
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn decode(value: &str) -> Vec<u8> {
        B64.decode(value).unwrap()
    }
    #[test]
    fn rfc8291_published_encryption_vector_and_vapid_signature_verify() {
        // RFC 8291 Appendix A: independent, published interoperability fixture.
        let receiver = decode(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        );
        let sender = decode(
            "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8",
        );
        let mut info = b"WebPush: info\0".to_vec();
        info.extend(&receiver);
        info.extend(&sender);
        let ikm = expand(
            &decode("BTBZMqHH6r4Tts7J_aSIgg"),
            &decode("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"),
            &info,
            32,
        )
        .unwrap();
        assert_eq!(
            B64.encode(&ikm),
            "S4lYMb_L0FxCeq0WhDx813KgSYqU26kOyzWUdsXYyrg"
        );
        let encrypted = encrypt_record(
            &ikm,
            decode("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap(),
            &sender,
            b"When I grow up, I want to be a watermelon",
        )
        .unwrap();
        assert_eq!(
            B64.encode(&encrypted[86..]),
            "8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ"
        );
        let rng = SystemRandom::new();
        let doc = signature::EcdsaKeyPair::generate_pkcs8(
            &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .unwrap();
        let vapid = Vapid(
            signature::EcdsaKeyPair::from_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                doc.as_ref(),
                &rng,
            )
            .unwrap(),
        );
        let header = vapid
            .authorization("https://push.example.com/send/device")
            .unwrap();
        let token = header
            .strip_prefix("vapid t=")
            .unwrap()
            .split(',')
            .next()
            .unwrap();
        let parts = token.split('.').collect::<Vec<_>>();
        signature::UnparsedPublicKey::new(
            &signature::ECDSA_P256_SHA256_FIXED,
            vapid.0.public_key(),
        )
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &decode(parts[2]),
        )
        .unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&decode(parts[1])).unwrap();
        assert_eq!(claims["aud"], "https://push.example.com");
        assert!(claims["exp"].as_i64().unwrap() > chrono::Utc::now().timestamp());
    }
}
