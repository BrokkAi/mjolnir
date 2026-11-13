use super::*;

/// Process-local operator availability and refusal memory, retained across bridge restarts.
#[derive(Debug, Clone, Default)]
pub struct OperatorAway(Arc<Mutex<OperatorAwayState>>);

#[derive(Debug)]
struct OperatorAwayState {
    presence: mj_core::state::OperatorPresence,
    // The accepted answers are both the fallback identity and Jev's evidence.
    refused: BTreeMap<(String, AwayRequest), Vec<serde_json::Value>>,
    next_bridge: u64,
    bridges: BTreeSet<u64>,
    pending: Vec<PendingElicitation>,
    changed: tokio::sync::watch::Sender<()>,
}

impl Default for OperatorAwayState {
    fn default() -> Self {
        Self {
            presence: Default::default(),
            refused: Default::default(),
            next_bridge: 0,
            bridges: Default::default(),
            pending: Default::default(),
            changed: tokio::sync::watch::channel(()).0,
        }
    }
}

impl OperatorAwayState {
    fn answer_away(&mut self, entry: PendingElicitation) {
        let question =
            (entry.identity.1 == AwayRequest::Question).then(|| question_evidence(&entry.question));
        if entry.answer.send(PendingAnswer::OperatorAway).is_ok() {
            let history = self.refused.entry(entry.identity).or_default();
            if let Some(question) = question {
                history.push(question);
                self.changed.send_replace(());
            }
        }
    }
}

/// Retain answer constraints, excluding transient identity and secret defaults.
fn question_evidence(question: &ElicitationRequest) -> serde_json::Value {
    let mut evidence = serde_json::to_value(question).expect("elicitation serializes");
    evidence
        .as_object_mut()
        .expect("elicitation is an object")
        .remove("id");
    for (field, original) in evidence["fields"]
        .as_array_mut()
        .expect("elicitation fields are an array")
        .iter_mut()
        .zip(&question.fields)
    {
        if question.field_is_secret(original) {
            field
                .as_object_mut()
                .expect("elicitation field is an object")
                .remove("default");
            field["secret"] = true.into();
        }
    }
    evidence
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum AwayRequest {
    Question,
    Plan,
    Tool(String),
}

impl OperatorAway {
    // Only the Unix worker runtime receives SetOperatorAway. Other targets
    // carry the default state so ACP request handling has one type.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn set(&self, presence: mj_core::state::OperatorPresence) {
        let mut state = self.0.lock().expect("operator-away state lock poisoned");
        if presence.revision > state.presence.revision {
            state.presence = presence;
            state.refused.clear();
            if presence.away {
                // Vec order is registration order, including across bridge scopes.
                let pending = std::mem::take(&mut state.pending);
                for entry in pending {
                    if !state.refused.contains_key(&entry.identity) {
                        state.answer_away(entry);
                    } else {
                        state.pending.push(entry);
                    }
                }
            }
            state.changed.send_replace(());
        }
    }

    #[cfg(test)]
    pub(super) fn pending_for_bridge(&self) -> PendingElicitations {
        self.pending_for_bridge_with_source(
            Some(VerdictSource::Direct {
                key: String::new(),
                endpoint: String::new(),
            }),
            None,
        )
    }

    pub(super) fn pending_for_bridge_with_source(
        &self,
        source: Option<VerdictSource>,
        turn_context: Option<mj_transcript::turn_context::TurnContext>,
    ) -> PendingElicitations {
        let mut state = self.0.lock().expect("operator-away state lock poisoned");
        let id = state.next_bridge;
        state.next_bridge = id.checked_add(1).expect("ACP bridge identity exhausted");
        state.bridges.insert(id);
        PendingElicitations(Arc::new(PendingBridge {
            owner: self.clone(),
            id,
            source,
            turn_context,
        }))
    }
}

/// A bridge's live responders share the presence owner's serialized decisions.
#[derive(Clone)]
pub(super) struct PendingElicitations(Arc<PendingBridge>);

struct PendingBridge {
    owner: OperatorAway,
    id: u64,
    source: Option<VerdictSource>,
    /// Owns the session id and the decision log the AFK check records into.
    turn_context: Option<mj_transcript::turn_context::TurnContext>,
}

impl Drop for PendingBridge {
    fn drop(&mut self) {
        let mut state = self
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        state.bridges.remove(&self.id);
        state.pending.retain(|entry| entry.bridge != self.id);
    }
}

impl PendingElicitations {
    /// Both immediate and later AFK decisions reach the same responder task.
    /// It publishes Requested before receiving this answer, so AFK cannot
    /// resolve a form before the relay has observed the question.
    pub(super) fn register(
        &self,
        question: ElicitationRequest,
        agent: String,
        request: AwayRequest,
    ) -> oneshot::Receiver<PendingAnswer> {
        let (answer, received) = oneshot::channel();
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        if !state.bridges.contains(&self.0.id) {
            return received;
        }
        let identity = (agent, request);
        if state.presence.away && !state.refused.contains_key(&identity) {
            state.answer_away(PendingElicitation {
                bridge: self.0.id,
                question,
                identity,
                answer,
            });
        } else {
            state.pending.push(PendingElicitation {
                bridge: self.0.id,
                question,
                identity,
                answer,
            });
        }
        received
    }

    /// The responder owns this future. Human answers and cancellation drop its
    /// HTTP work; only the presence owner may accept the resulting answer.
    pub(super) async fn refine_question(&self, id: &str) {
        let mut changed = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned")
            .changed
            .subscribe();
        let mut client = None;
        loop {
            changed.borrow_and_update();
            let snapshot = {
                let state = self
                    .0
                    .owner
                    .0
                    .lock()
                    .expect("operator-away state lock poisoned");
                let Some(entry) = state
                    .pending
                    .iter()
                    .find(|entry| entry.bridge == self.0.id && entry.question.id == id)
                else {
                    return;
                };
                if entry.identity.1 != AwayRequest::Question {
                    return;
                }
                state.presence.away.then(|| {
                    (
                        state.presence.revision,
                        entry.identity.0.clone(),
                        question_evidence(&entry.question),
                        state
                            .refused
                            .get(&entry.identity)
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
            };
            if let Some((revision, agent, question, history)) = snapshot {
                let classify = async {
                    if client.is_none() {
                        client =
                            verdict_client::VerdictClient::resolve(self.0.source.as_ref()).await;
                    }
                    // Read at classification time: the log opens after the bridge starts.
                    let (session, log) = self.0.turn_context.as_ref().map_or_else(
                        || (String::new(), None),
                        |context| (context.session_id(), context.decision_log()),
                    );
                    match &client {
                        Some(client) => {
                            client
                                .clone()
                                .with_log(log)
                                .same_afk_question(&session, &question, &history)
                                .await
                        }
                        None => Err(anyhow!("Jev is disabled or unavailable")),
                    }
                };
                let verdict = tokio::select! {
                    result = classify => result,
                    result = changed.changed() => { if result.is_err() { return; } continue; }
                };
                match verdict {
                    Ok(false) => {
                        let mut state = self
                            .0
                            .owner
                            .0
                            .lock()
                            .expect("operator-away state lock poisoned");
                        // Compare and consume under the same owner lock. Concurrent
                        // new questions must reclassify against the accepted history.
                        if state.presence.away
                            && state.presence.revision == revision
                            && state
                                .refused
                                .get(&(agent, AwayRequest::Question))
                                .is_some_and(|current| *current == history)
                            && let Some(index) = state.pending.iter().position(|entry| {
                                entry.bridge == self.0.id && entry.question.id == id
                            })
                        {
                            let entry = state.pending.remove(index);
                            state.answer_away(entry);
                            tracing::info!(target: "mj_jev", %id, revision, "Jev identified a new AFK question");
                            return;
                        }
                        continue;
                    }
                    Ok(true) => {
                        tracing::info!(target: "mj_jev", %id, revision, "Jev identified a repeated AFK question")
                    }
                    Err(error) => {
                        tracing::warn!(target: "mj_jev", %id, %error, "AFK question classification unavailable; retaining existing behavior")
                    }
                }
            }
            if changed.changed().await.is_err() {
                return;
            }
        }
    }

    pub(super) fn remove(&self, id: &str) -> Option<PendingElicitation> {
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        let index = state
            .pending
            .iter()
            .position(|entry| entry.bridge == self.0.id && entry.question.id == id)?;
        let entry = state.pending.remove(index);
        Some(entry)
    }

    /// Claim Cancel before stopping the turn, so a resolved form cannot stop it.
    /// The caller keeps the sender until after sending session/cancel.
    pub(super) fn take_turn_cancel(
        &self,
        id: &str,
        response: &ElicitationResponse,
    ) -> Option<PendingElicitation> {
        if *response != ElicitationResponse::Cancel || !id.starts_with(TOOL_PERMISSION_ID_PREFIX) {
            return None;
        }
        self.remove(id)
    }

    pub(super) fn is_empty(&self) -> bool {
        let state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        !state.pending.iter().any(|entry| entry.bridge == self.0.id)
    }

    pub(super) fn close(&self) {
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        state.bridges.remove(&self.0.id);
        state.pending.retain(|entry| entry.bridge != self.0.id);
    }

    pub(super) fn retain(&self, keep: impl Fn(&str) -> bool) {
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        state
            .pending
            .retain(|entry| entry.bridge != self.0.id || keep(&entry.question.id));
    }

    pub(super) fn resolve(
        &self,
        id: &str,
        response: ElicitationResponse,
    ) -> std::result::Result<(), String> {
        let mut state = self
            .0
            .owner
            .0
            .lock()
            .expect("operator-away state lock poisoned");
        let index = state
            .pending
            .iter()
            .position(|entry| entry.bridge == self.0.id && entry.question.id == id)
            .ok_or_else(|| format!("elicitation {id:?} is no longer pending"))?;
        let response = state.pending[index].question.harness_response(response)?;
        let entry = state.pending.remove(index);
        entry
            .answer
            .send(PendingAnswer::Human(response))
            .map_err(|_| format!("elicitation {id:?} was cancelled before it was answered"))
    }
}

#[cfg(test)]
impl Default for PendingElicitations {
    fn default() -> Self {
        OperatorAway::default().pending_for_bridge()
    }
}

#[derive(Debug)]
pub(super) struct PendingElicitation {
    bridge: u64,
    question: ElicitationRequest,
    identity: (String, AwayRequest),
    answer: oneshot::Sender<PendingAnswer>,
}

#[derive(Debug)]
pub(super) enum PendingAnswer {
    Human(ElicitationResponse),
    OperatorAway,
}

impl PendingAnswer {
    pub(super) fn action_name(&self) -> &str {
        match self {
            Self::Human(response) => response.action_name(),
            Self::OperatorAway => mj_core::elicitation::OPERATOR_AWAY_ACTION,
        }
    }

    pub(super) fn human(&self) -> Option<&ElicitationResponse> {
        match self {
            Self::Human(response) => Some(response),
            Self::OperatorAway => None,
        }
    }

    pub(super) fn harness_response(self) -> ElicitationResponse {
        match self {
            Self::Human(response) => response,
            Self::OperatorAway => ElicitationResponse::Decline,
        }
    }
}

pub(super) fn away_permission_identity(
    request: &RequestPermissionRequest,
    input: Option<serde_json::Value>,
) -> AwayRequest {
    let mut identity = serde_json::json!({
        "kind": request.tool_call.fields.kind,
        "title": request.tool_call.fields.title,
        "input": input,
    });
    // ACP metadata may enable serde_json's preserve_order feature elsewhere.
    identity.sort_all_objects();
    AwayRequest::Tool(identity.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    struct Classifier {
        listener: TcpListener,
        source: VerdictSource,
    }

    impl Classifier {
        async fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/", listener.local_addr().unwrap());
            Self {
                listener,
                source: VerdictSource::Hosted { endpoint },
            }
        }

        async fn request(&self) -> (TcpStream, Value) {
            let (stream, _) = self.listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut headers = String::new();
            let mut length = None;
            loop {
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                assert!(
                    !line.is_empty(),
                    "classifier client disconnected before headers"
                );
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
                headers.push_str(&line);
            }
            let mut body = vec![0; length.unwrap()];
            stream.read_exact(&mut body).await.unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert!(headers.starts_with("POST /v1/afk-question-glide "));
            // The hosted proxy owns the question: the body is the state alone.
            assert_eq!(body.as_object().unwrap().len(), 2, "{body}");
            (stream.into_inner(), body)
        }

        async fn respond(&self, mut stream: TcpStream, choice: &str) {
            let (same, different) = if choice == "same" {
                (0.99, 0.01)
            } else {
                (0.01, 0.99)
            };
            let body = json!({"probabilities":{"same":same,"different":different}}).to_string();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
    }

    fn question(id: &str, text: &str) -> ElicitationRequest {
        ElicitationRequest {
            id: id.into(),
            message: text.into(),
            title: None,
            description: None,
            fields: vec![],
            tool_permission: None,
        }
    }

    fn ask(
        pending: &PendingElicitations,
        id: &str,
        text: &str,
    ) -> tokio::task::JoinHandle<Option<PendingAnswer>> {
        ask_form(pending, question(id, text))
    }

    fn ask_form(
        pending: &PendingElicitations,
        question: ElicitationRequest,
    ) -> tokio::task::JoinHandle<Option<PendingAnswer>> {
        let id = question.id.clone();
        let answer = pending.register(question, String::new(), AwayRequest::Question);
        let pending = pending.clone();
        tokio::spawn(async move {
            await_elicitation_response(&pending, &id, answer, std::future::pending()).await
        })
    }

    // Hard-won: #1489: new questions shared the generic Question refusal identity.
    #[tokio::test]
    async fn afk_question_protocol_distinguishes_mixed_forms_and_preserves_unavailable_fallback() {
        let classifier = Classifier::new().await;
        let owner = OperatorAway::default();
        let pending = owner.pending_for_bridge_with_source(Some(classifier.source.clone()), None);
        owner.set(mj_core::state::OperatorPresence {
            away: true,
            revision: 1,
        });
        assert!(matches!(
            ask(&pending, "first", "Who owns #10941?").await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let new = ask(&pending, "mixed", "Who owns #11482, and who owns #10941?");
        let (stream, evidence) = classifier.request().await;
        assert_eq!(
            evidence,
            json!({"current_question":{"message":"Who owns #11482, and who owns #10941?", "fields":[]}, "previous_questions":[{"message":"Who owns #10941?", "fields":[]}]})
        );
        classifier.respond(stream, "different").await;
        assert!(matches!(
            new.await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));

        let repeated = ask(&pending, "repeat", "Please assign #10941.");
        let (stream, evidence) = classifier.request().await;
        assert_eq!(
            evidence["previous_questions"],
            json!([{"message":"Who owns #10941?", "fields":[]}, {"message":"Who owns #11482, and who owns #10941?", "fields":[]}])
        );
        classifier.respond(stream, "same").await;
        pending
            .resolve("repeat", ElicitationResponse::Decline)
            .unwrap();
        assert!(matches!(
            repeated.await.unwrap(),
            Some(PendingAnswer::Human(_))
        ));

        for body in [
            "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        ] {
            let fallback = ask(&pending, "fallback", "Who owns #12000?");
            let (mut stream, _) = classifier.request().await;
            stream.write_all(body.as_bytes()).await.unwrap();
            // Wait until the network attempt has consumed the response before
            // answering, so this proves fallback rather than winning a race.
            let mut eof = Vec::new();
            stream.read_to_end(&mut eof).await.unwrap();
            pending
                .resolve("fallback", ElicitationResponse::Decline)
                .unwrap();
            assert!(matches!(
                fallback.await.unwrap(),
                Some(PendingAnswer::Human(_))
            ));
        }
    }

    // Hard-won: #1489 review: evidence lost constraints and leaked secret defaults/choices.
    #[tokio::test]
    async fn afk_question_evidence_preserves_changed_selection_and_required_constraints() {
        let classifier = Classifier::new().await;
        let owner = OperatorAway::default();
        let pending = owner.pending_for_bridge_with_source(Some(classifier.source.clone()), None);
        owner.set(mj_core::state::OperatorPresence {
            away: true,
            revision: 1,
        });
        let first = ElicitationRequest::from_acp_params(
            "first",
            json!({
                "mode":"form", "sessionId":"test", "message":"Choose a destination",
            "requestedSchema":{"type":"object", "properties":{
                "destination":{"type":"string", "title":"Destination", "enum":["one","two"]},
                "password":{"type":"string", "title":"Password", "enum":["P!n12","n3w?Pin"], "default":"P!n12", "_meta":{"codex":{"isSecret":true}}},
                "password__other":{"type":"string", "title":"Other password", "default":"77!Z", "_meta":{"codex":{"questionId":"password", "role":"user_note"}}}
            }}
            }),
        )
        .unwrap();
        let second = ElicitationRequest::from_acp_params("second", json!({
            "mode":"form", "sessionId":"test", "message":"Choose a destination",
            "requestedSchema":{"type":"object", "required":["destination"], "properties":{
                "destination":{"type":"array", "title":"Destination", "items":{"type":"string", "enum":["one","two"]}, "minItems":1},
                "password":{"type":"string", "title":"Password", "enum":["P!n12","n3w?Pin"], "default":"n3w?Pin", "_meta":{"codex":{"isSecret":true}}},
                "password__other":{"type":"string", "title":"Other password", "default":"55?X", "_meta":{"codex":{"questionId":"password", "role":"user_note"}}}
            }}
        })).unwrap();
        assert_eq!(
            first.question_text(),
            second.question_text(),
            "the old evidence lost the change"
        );
        assert!(matches!(
            ask_form(&pending, first).await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let second = ask_form(&pending, second);
        let (stream, evidence) = classifier.request().await;
        assert!(!evidence.to_string().contains("P!n12"));
        assert!(!evidence.to_string().contains("n3w?Pin"));
        assert!(!evidence.to_string().contains("77!Z"));
        assert!(!evidence.to_string().contains("55?X"));
        assert!(
            evidence["current_question"]["fields"][2]
                .get("default")
                .is_none()
        );
        assert!(
            evidence["previous_questions"][0]["fields"][2]
                .get("default")
                .is_none()
        );
        assert_eq!(evidence["current_question"]["fields"][2]["secret"], true);
        assert_eq!(
            evidence["current_question"]["fields"][1]["options"],
            evidence["previous_questions"][0]["fields"][1]["options"]
        );
        assert_ne!(
            evidence["current_question"]["fields"][1]["options"][0]["value"],
            evidence["current_question"]["fields"][1]["options"][1]["value"]
        );
        assert!(
            evidence["current_question"]["fields"][1]
                .get("default")
                .is_none()
        );
        assert!(
            evidence["previous_questions"][0]["fields"][1]
                .get("default")
                .is_none()
        );
        assert_eq!(evidence["current_question"]["fields"][1]["secret"], true);
        assert_eq!(
            evidence["current_question"]["fields"][0]["kind"],
            "multi_select"
        );
        assert_eq!(evidence["current_question"]["fields"][0]["required"], true);
        assert_eq!(evidence["current_question"]["fields"][0]["min_items"], 1);
        assert_eq!(
            evidence["previous_questions"][0]["fields"][0]["kind"],
            "single_select"
        );
        assert_eq!(
            evidence["previous_questions"][0]["fields"][0]["required"],
            false
        );
        classifier.respond(stream, "different").await;
        assert!(matches!(
            second.await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
    }

    // Hard-won: #1489 review: redaction changed ID identity across escapes and punctuation.
    #[tokio::test]
    async fn afk_question_protocol_preserves_redacted_identifier_equality() {
        let classifier = Classifier::new().await;
        let owner = OperatorAway::default();
        let pending = owner.pending_for_bridge_with_source(Some(classifier.source.clone()), None);
        owner.set(mj_core::state::OperatorPresence {
            away: true,
            revision: 1,
        });
        let first_id = "a".repeat(40);
        let second_id = "b".repeat(40);
        assert!(matches!(
            ask(&pending, "first", &format!("Merge commit {first_id}?"))
                .await
                .unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let second = ask(&pending, "second", &format!("Merge commit\n{second_id}?"));
        let (stream, evidence) = classifier.request().await;
        let serialized = evidence.to_string();
        assert!(!serialized.contains(&first_id) && !serialized.contains(&second_id));
        assert_eq!(
            evidence["current_question"]["message"],
            "Merge commit\n[redacted token 0]?"
        );
        assert_eq!(
            evidence["previous_questions"][0]["message"],
            "Merge commit [redacted token 1]?"
        );
        classifier.respond(stream, "different").await;
        assert!(matches!(
            second.await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let repeated = ask(&pending, "repeated", &format!("Merge commit\t{first_id}."));
        let (stream, evidence) = classifier.request().await;
        assert_eq!(
            evidence["current_question"]["message"],
            "Merge commit\t[redacted token 0]."
        );
        assert_eq!(
            evidence["previous_questions"][0]["message"],
            "Merge commit [redacted token 0]?"
        );
        assert_ne!(
            evidence["current_question"],
            evidence["previous_questions"][1]
        );
        classifier.respond(stream, "same").await;
        pending
            .resolve("repeated", ElicitationResponse::Decline)
            .unwrap();
        assert!(matches!(
            repeated.await.unwrap(),
            Some(PendingAnswer::Human(_))
        ));
        // A serialized newline plus a 23-character run used to cross the
        // 24-character redaction boundary and produce an invalid JSON escape.
        let short_message = format!("Merge commit\n{}?", "c".repeat(23));
        let short = ask(&pending, "short", &short_message);
        let (stream, evidence) = classifier.request().await;
        assert_eq!(evidence["current_question"]["message"], short_message);
        classifier.respond(stream, "different").await;
        assert!(matches!(
            short.await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
    }

    #[tokio::test]
    async fn afk_question_decisions_are_recorded_with_the_redacted_request_and_outcome() {
        let classifier = Classifier::new().await;
        let directory = tempfile::tempdir().unwrap();
        let context = mj_transcript::turn_context::TurnContext::default();
        context.set_session_id("afk-session");
        context.set_decision_log(mj_core::jev::DecisionLog::open(directory.path().into()).unwrap());
        let owner = OperatorAway::default();
        let pending =
            owner.pending_for_bridge_with_source(Some(classifier.source.clone()), Some(context));
        owner.set(mj_core::state::OperatorPresence {
            away: true,
            revision: 1,
        });
        const TOKEN: &str = "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789";
        assert!(matches!(
            ask(&pending, "first", &format!("Use {TOKEN}?"))
                .await
                .unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let new = ask(&pending, "new", "Who owns #11482?");
        let (stream, _) = classifier.request().await;
        classifier.respond(stream, "different").await;
        assert!(matches!(
            new.await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let repeat = ask(&pending, "repeat", "Please assign #11482.");
        let (stream, _) = classifier.request().await;
        classifier.respond(stream, "same").await;
        let failed = ask(&pending, "failed", "Another decision?");
        let (mut stream, _) = classifier.request().await;
        stream
            .write_all(
                b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut eof = Vec::new();
        stream.read_to_end(&mut eof).await.unwrap();
        // "repeat" was held and still waits; resolve both so the tasks end.
        for id in ["repeat", "failed"] {
            pending.resolve(id, ElicitationResponse::Decline).unwrap();
        }
        repeat.await.unwrap();
        failed.await.unwrap();
        let mut page = mj_core::jev::read(directory.path(), "afk-session", None).unwrap();
        page.decisions
            .sort_by_key(|d| (d.started_at_ms, d.id.clone()));
        let outcomes: Vec<_> = page.decisions.iter().map(|d| d.status.as_str()).collect();
        assert_eq!(outcomes, ["declined_new", "held_repeat", "failed"]);
        // The list omits technical details; read each record by id.
        let mut records = Vec::new();
        for summary in &page.decisions {
            let detail =
                mj_core::jev::read(directory.path(), "afk-session", Some(&summary.id)).unwrap();
            records.push(detail.decisions.into_iter().next().unwrap());
        }
        for record in &records {
            assert_eq!(record.kind, "activity");
            assert_eq!(
                record.checked,
                "Does this question repeat one already declined while away?"
            );
            let technical = record.technical.as_ref().unwrap();
            assert_eq!(technical["contract"], "afk-question-glide-v1");
            assert!(!technical.to_string().contains(TOKEN), "{technical}");
            assert!(
                technical["request"]["previous_questions"][0]["message"]
                    .as_str()
                    .unwrap()
                    .contains("[redacted token"),
                "{technical}"
            );
        }
        assert_eq!(
            records[0].technical.as_ref().unwrap()["probabilities"],
            json!({"same":0.01,"different":0.99})
        );
        assert!(
            records[2].technical.as_ref().unwrap()["error"]
                .as_str()
                .unwrap()
                .contains("413")
        );
    }

    #[tokio::test]
    async fn afk_question_classification_cannot_outlive_human_cancel_return_or_bridge_close() {
        for action in ["human", "cancel", "return", "close"] {
            let classifier = Classifier::new().await;
            let owner = OperatorAway::default();
            let pending =
                owner.pending_for_bridge_with_source(Some(classifier.source.clone()), None);
            owner.set(mj_core::state::OperatorPresence {
                away: true,
                revision: 1,
            });
            assert!(matches!(
                ask(&pending, "first", "First decision?").await.unwrap(),
                Some(PendingAnswer::OperatorAway)
            ));
            let answer = pending.register(
                question("racing", "Another decision?"),
                String::new(),
                AwayRequest::Question,
            );
            let (cancel, cancelled) = oneshot::channel();
            let response = {
                let pending = pending.clone();
                tokio::spawn(async move {
                    await_elicitation_response(&pending, "racing", answer, async {
                        let _ = cancelled.await;
                    })
                    .await
                })
            };
            let (mut stream, _) = classifier.request().await;
            match action {
                "human" => pending
                    .resolve("racing", ElicitationResponse::Decline)
                    .unwrap(),
                "cancel" => cancel.send(()).unwrap(),
                "return" => owner.set(mj_core::state::OperatorPresence {
                    away: false,
                    revision: 2,
                }),
                "close" => pending.close(),
                _ => unreachable!(),
            }
            // Cancellation/return drops the in-flight HTTP request, rather than
            // waiting for Jev to release its response.
            let mut eof = Vec::new();
            stream.read_to_end(&mut eof).await.unwrap();
            if action == "return" {
                pending
                    .resolve("racing", ElicitationResponse::Decline)
                    .unwrap();
            }
            let outcome = response.await.unwrap();
            assert!(
                if matches!(action, "human" | "return") {
                    matches!(outcome, Some(PendingAnswer::Human(_)))
                } else {
                    outcome.is_none()
                },
                "{action}: {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn concurrent_afk_questions_reclassify_after_another_question_is_answered() {
        let classifier = Classifier::new().await;
        let owner = OperatorAway::default();
        let pending = owner.pending_for_bridge_with_source(Some(classifier.source.clone()), None);
        owner.set(mj_core::state::OperatorPresence {
            away: true,
            revision: 1,
        });
        assert!(matches!(
            ask(&pending, "first", "First decision?").await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let one = ask(&pending, "one", "Who owns #11482?");
        let (stream_one, _) = classifier.request().await;
        let two = ask(&pending, "two", "Assign #11482 to someone?");
        let (mut stream_two, _) = classifier.request().await;
        classifier.respond(stream_one, "different").await;
        assert!(matches!(
            one.await.unwrap(),
            Some(PendingAnswer::OperatorAway)
        ));
        let mut eof = Vec::new();
        stream_two.read_to_end(&mut eof).await.unwrap();
        let (stream_two, evidence) = classifier.request().await;
        assert_eq!(
            evidence["previous_questions"],
            json!([{"message":"First decision?", "fields":[]}, {"message":"Who owns #11482?", "fields":[]}])
        );
        classifier.respond(stream_two, "same").await;
        pending
            .resolve("two", ElicitationResponse::Decline)
            .unwrap();
        assert!(matches!(two.await.unwrap(), Some(PendingAnswer::Human(_))));
    }
}
