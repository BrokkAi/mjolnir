use agent_client_protocol::schema::v1::ContentBlock;
use base64::Engine as _;
use mj_core::attachment::*;
use std::fs;
fn photo() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.resize(MAX_IMAGE_BYTES, 7);
    bytes
}
#[test]
fn photo_queue_survives_restart_and_archive_transfer() {
    use crate::relay::test_support::{SESSION, relay_request, submit_relay};
    use crate::relay::{
        DurableRelay, RELAY_STATE_FILE, RelayCommand, RelayRequest, RelayResponseBody,
    };
    let source = tempfile::tempdir().unwrap();
    let store = AttachmentStore::worker(source.path());
    let mut prompt = Vec::new();
    for index in 0..10 {
        let mut bytes = photo();
        bytes[100] = index;
        let reference = AttachmentRef::new(&bytes, "image/png".into(), 100, 100).unwrap();
        store.install(&reference, &bytes).unwrap();
        prompt.push(reference.content_block());
    }
    let command = RelayCommand::Prompt {
        prompt: prompt.clone(),
    };
    let mut relay = DurableRelay::open(source.path(), SESSION, "1.0.0").unwrap();
    let ordinal = submit_relay(&mut relay, "photos-command-1", command.clone());
    assert_eq!(
        submit_relay(&mut relay, "photos-command-1", command.clone()),
        ordinal
    );
    assert!(
        fs::metadata(source.path().join(RELAY_STATE_FILE))
            .unwrap()
            .len()
            < 64 * 1024
    );
    drop(relay);
    let mut relay = DurableRelay::open(source.path(), SESSION, "1.0.0").unwrap();
    assert_eq!(
        submit_relay(&mut relay, "photos-command-1", command.clone()),
        ordinal
    );
    let target = tempfile::tempdir().unwrap();
    let restored = AttachmentStore::worker(target.path());
    for artifact in store.archive_artifacts().unwrap() {
        assert!(
            restored
                .restore_artifact(&artifact.relative_path, &artifact.data)
                .unwrap()
        );
    }
    restored.resolve(&mut prompt).unwrap();
    for (index, block) in prompt.iter().enumerate() {
        let ContentBlock::Image(image) = block else {
            panic!()
        };
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(&image.data)
                .unwrap()[100],
            index as u8
        );
    }
    let missing = tempfile::tempdir().unwrap();
    let mut relay = DurableRelay::open(missing.path(), SESSION, "1.0.0").unwrap();
    let response = relay.handle(relay_request(
        "missing-photos",
        RelayRequest::Submit {
            command_id: "photos-command-2".into(),
            command,
        },
    ));
    assert!(matches!(response.body, RelayResponseBody::Error { .. }));
    assert_eq!(relay.latest_ordinal(), 0);
}
