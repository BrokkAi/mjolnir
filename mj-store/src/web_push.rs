//! Daemon-owned encrypted Web Push delivery. Durable intents precede HTTPS.
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use mj_client::runtime_feed::RuntimeProjection;
use mj_core::attention::{EntryMessage, NotificationKind, SessionAttention};
use ring::{
    aead, agreement, hkdf,
    rand::{SecureRandom, SystemRandom},
    signature::{self, KeyPair},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::OnceLock};

static WAKE: OnceLock<tokio::sync::Notify> = OnceLock::new();
pub fn wake() {
    wake_signal().notify_one();
}
/// The signal `wake` raises, for the delivery loop to wait on.
pub fn wake_signal() -> &'static tokio::sync::Notify {
    WAKE.get_or_init(Default::default)
}
pub async fn blocking<T: Send + 'static>(
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
    pub fn validate(&self) -> Result<()> {
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
pub struct Notice {
    pub title: String,
    pub body: String,
    pub session_id: Option<String>,
    pub url: String,
    pub tag: String,
    #[serde(default)]
    pub episode: Option<String>,
    #[serde(default)]
    pub observed_revision: u64,
}

pub struct Vapid(signature::EcdsaKeyPair);
impl Vapid {
    pub fn load() -> Result<Self> {
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
    pub fn public(&self) -> String {
        B64.encode(self.0.public_key().as_ref())
    }
    pub fn authorization(&self, endpoint: &str) -> Result<String> {
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
pub fn encrypt(subscription: &Subscription, payload: &[u8]) -> Result<Vec<u8>> {
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

#[derive(Debug, PartialEq, Eq)]
pub struct PushEntry<S> {
    pub session: S,
    pub completed_ordinal: Option<u64>,
    pub paused: bool,
    pub holder: S,
    pub kind: NotificationKind,
    pub text: Option<S>,
    pub message_id: Option<S>,
}

pub fn card_entries(runtime: &RuntimeProjection) -> Option<BTreeMap<String, PushEntry<&str>>> {
    let cards = runtime.metadata.notification_cards.as_ref()?;
    // Attention supplies only worker availability. Card publication owns
    // notification eligibility, including ignored sessions and managed children.
    let unavailable = |id: &str| {
        matches!(
            runtime.metadata.session_attention.get(id),
            Some(
                SessionAttention::Unknown
                    | SessionAttention::Unreachable
                    | SessionAttention::Failed
            )
        )
    };
    let mut entries: BTreeMap<String, PushEntry<&str>> = BTreeMap::new();
    for (session, card) in cards {
        let holder = card.item_destination(session, card.entries.len());
        for (item, entry) in card.entries.iter().enumerate() {
            let Some(identity) = card.item_identity(session, item) else {
                continue;
            };
            let message_id = match &entry.message {
                EntryMessage::Found(id) => Some(id.as_str()),
                _ => None,
            };
            // The same stable message can be classified in two completions.
            // The card's oldest-first order gives that identity its first position.
            entries.entry(identity).or_insert(PushEntry {
                session,
                completed_ordinal: Some(entry.completed_ordinal),
                paused: unavailable(session),
                holder: card.item_destination(session, 0),
                kind: entry.kind,
                text: None,
                message_id,
            });
        }
        if let Some(form) = &card.form
            && !runtime
                .records
                .get(holder)
                .is_some_and(|record| record.notifications_ignored)
        {
            let identity = card
                .item_identity(session, card.entries.len())
                .expect("form identity");
            entries.insert(
                identity,
                PushEntry {
                    session,
                    completed_ordinal: None,
                    paused: unavailable(session) || unavailable(holder),
                    holder,
                    kind: NotificationKind::NeedsInput,
                    text: Some(&form.message),
                    message_id: None,
                },
            );
        }
    }
    Some(entries)
}

impl PushEntry<&str> {
    pub fn owned(&self) -> PushEntry<String> {
        PushEntry {
            session: self.session.to_owned(),
            completed_ordinal: self.completed_ordinal,
            paused: self.paused,
            holder: self.holder.to_owned(),
            kind: self.kind,
            text: self.text.map(str::to_owned),
            message_id: self.message_id.map(str::to_owned),
        }
    }

    pub fn notice(
        &self,
        runtime: &RuntimeProjection,
        identity: String,
        message: Option<&str>,
    ) -> Notice {
        let fallback = match self.kind {
            NotificationKind::NeedsInput => "Needs your input",
            NotificationKind::Important => "Has a reply for you",
        };
        Notice {
            title: runtime
                .records
                .get(self.session)
                .map_or("Mjolnir", |record| record.listed_title())
                .to_owned(),
            body: phone_line(message.unwrap_or_default(), fallback),
            session_id: Some(self.session.to_owned()),
            url: format!("/#conversation/{}", self.holder),
            tag: self.session.to_owned(),
            episode: Some(identity),
            observed_revision: runtime.revision,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_client::runtime_feed::{CardForm, NotificationCard};

    #[test]
    fn each_card_item_push_targets_its_own_session_or_form_holder() {
        let mut runtime = RuntimeProjection::default();
        runtime.metadata.notification_cards = Some(mj_core::snapshot_map::SnapshotMap::from([(
            "parent".into(),
            NotificationCard {
                since_ms: 1,
                badge: NotificationKind::NeedsInput,
                entries: vec![mj_core::attention::NotificationStackEntry {
                    completed_at_ms: 1,
                    completed_ordinal: 4,
                    kind: NotificationKind::NeedsInput,
                    message: EntryMessage::None,
                }],
                dismissal: Some(mj_core::attention::NotificationDismissal::Stack { revision: 1 }),
                holder: Some("child".into()),
                form: Some(CardForm {
                    source: mj_client::runtime_feed::CardFormSource::Elicitation,
                    raised_at_ms: 2,
                    request_id: "r1".into(),
                    message: "form".into(),
                }),
            },
        )]));
        let entries = card_entries(&runtime).unwrap();
        let question = entries
            .values()
            .find(|entry| entry.completed_ordinal == Some(4))
            .unwrap();
        let form = entries
            .values()
            .find(|entry| entry.completed_ordinal.is_none())
            .unwrap();
        assert_eq!(question.holder, "parent");
        assert_eq!(
            question.notice(&runtime, "question".into(), None).url,
            "/#conversation/parent"
        );
        assert_eq!(form.holder, "child");
        assert_eq!(
            form.notice(&runtime, "form".into(), None).url,
            "/#conversation/child"
        );
    }

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
