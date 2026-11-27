use anyhow::{Context, Result};
use mj_core::project_memory::{
    ProjectMemorySnapshot, ProjectMemoryStore, TreeVersion, is_memory_document,
};
use mj_core::worker_launch::ProjectMemoryLaunchConfig;
use serde::{Deserialize, Serialize};
use std::io::Write;

const RECORD_FILE: &str = ".hel-memory-record.json";

fn retain_documents(tree: &mut ProjectMemorySnapshot) {
    tree.files.retain(|_, content| is_memory_document(content));
}

fn snapshot_documents(root: &std::path::Path) -> Result<ProjectMemorySnapshot> {
    let mut tree = ProjectMemoryStore::new(root).snapshot()?;
    retain_documents(&mut tree);
    Ok(tree)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Handout {
    id: String,
    tree: ProjectMemorySnapshot,
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    base: ProjectMemorySnapshot,
    base_version: Option<TreeVersion>,
    handout: Option<Handout>,
    incoming: Option<ProjectMemorySnapshot>,
}

impl Record {
    fn load(memory: &ProjectMemoryLaunchConfig) -> Result<Self> {
        let path = memory.baseline_root.join(RECORD_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let mut record: Self =
                    serde_json::from_slice(&bytes).context("decode project memory record")?;
                retain_documents(&mut record.base);
                if let Some(handout) = &mut record.handout {
                    retain_documents(&mut handout.tree);
                }
                if let Some(incoming) = &mut record.incoming {
                    retain_documents(incoming);
                }
                Ok(record)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let record = Self {
                    base: snapshot_documents(&memory.baseline_root)?,
                    base_version: None,
                    handout: None,
                    incoming: None,
                };
                record.save(memory)?;
                Ok(record)
            }
            Err(error) => Err(error).context("read project memory record"),
        }
    }

    fn prepare_save(&self, memory: &ProjectMemoryLaunchConfig) -> Result<tempfile::NamedTempFile> {
        std::fs::create_dir_all(&memory.baseline_root)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&memory.baseline_root)?;
        serde_json::to_writer(&mut temporary, self)?;
        temporary.flush()?;
        Ok(temporary)
    }

    fn save(&self, memory: &ProjectMemoryLaunchConfig) -> Result<()> {
        persist_record(self.prepare_save(memory)?, memory)
    }

    fn finish_incoming(&mut self, memory: &ProjectMemoryLaunchConfig) -> Result<()> {
        if let Some(tree) = self.incoming.clone() {
            let handout = self
                .handout
                .as_ref()
                .context("pending project-memory delivery has no handout")?;
            ProjectMemoryStore::new(&memory.root)
                .finish_interrupted_delivery(&handout.tree, &tree)?;
            self.base = tree;
            self.base_version = None;
            self.handout = None;
            self.incoming = None;
            self.save(memory)?;
        }
        Ok(())
    }
}

fn persist_record(
    temporary: tempfile::NamedTempFile,
    memory: &ProjectMemoryLaunchConfig,
) -> Result<()> {
    temporary
        .persist(memory.baseline_root.join(RECORD_FILE))
        .map_err(|error| error.error)?;
    Ok(())
}

pub(super) fn snapshot(
    memory: &ProjectMemoryLaunchConfig,
    absorbed: Option<&str>,
    head_version: &TreeVersion,
) -> Result<Option<(ProjectMemorySnapshot, String, ProjectMemorySnapshot)>> {
    let mut record = Record::load(memory)?;
    record.finish_incoming(memory)?;
    if record
        .handout
        .as_ref()
        .is_some_and(|handout| Some(handout.id.as_str()) == absorbed)
    {
        record.base = record.handout.take().expect("handout exists").tree;
        record.base_version = None;
        record.save(memory)?;
    }
    if let Some(handout) = &record.handout
        && handout.tree != record.base
    {
        return Ok(Some((
            record.base,
            handout.id.clone(),
            handout.tree.clone(),
        )));
    }
    let replica = snapshot_documents(&memory.root)?;
    if replica == record.base && record.base_version.as_ref() == Some(head_version) {
        return Ok(None);
    }
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|error| anyhow::anyhow!("generate project memory snapshot id: {error}"))?;
    let id = mj_core::hex::lower_hex(nonce);
    record.handout = Some(Handout {
        id: id.clone(),
        tree: replica.clone(),
    });
    record.save(memory)?;
    Ok(Some((record.base, id, replica)))
}

pub(super) fn deliver(
    memory: &ProjectMemoryLaunchConfig,
    id: &str,
    tree: ProjectMemorySnapshot,
    version: TreeVersion,
    tools: &mj_core::activity::ToolsInFlight,
) -> Result<mj_core::relay::ProjectMemoryDeliveryOutcome> {
    deliver_with_render(memory, id, tree, version, tools, |root, prepared| {
        prepared.render(root, false)
    })
}

fn deliver_with_render(
    memory: &ProjectMemoryLaunchConfig,
    id: &str,
    tree: ProjectMemorySnapshot,
    version: TreeVersion,
    tools: &mj_core::activity::ToolsInFlight,
    render: impl FnOnce(&std::path::Path, mj_core::project_memory::PreparedMemoryTree<'_>) -> Result<()>,
) -> Result<mj_core::relay::ProjectMemoryDeliveryOutcome> {
    use mj_core::relay::ProjectMemoryDeliveryOutcome;
    let prepared = mj_core::project_memory::prepare_memory_tree(&tree)?;
    let mut record = Record::load(memory)?;
    record.finish_incoming(memory)?;
    let Some(handout) = record.handout.as_ref().filter(|handout| handout.id == id) else {
        return Ok(ProjectMemoryDeliveryOutcome::Stale);
    };
    let handout_tree = handout.tree.clone();
    record.incoming = Some(tree.clone());
    let pending = record.prepare_save(memory)?;
    if !tools.is_empty() {
        return Ok(ProjectMemoryDeliveryOutcome::Busy);
    }
    if snapshot_documents(&memory.root)? != handout_tree {
        record.base = handout_tree;
        record.base_version = None;
        record.handout = None;
        record.incoming = None;
        record.save(memory)?;
        return Ok(ProjectMemoryDeliveryOutcome::Dirty);
    }
    persist_record(pending, memory)?;
    render(&memory.root, prepared)?;
    record.base = tree;
    record.base_version = Some(version);
    record.handout = None;
    record.incoming = None;
    record.save(memory)?;
    Ok(ProjectMemoryDeliveryOutcome::Delivered)
}

/// The worker is the only seeder; a retained or restored replica always wins.
pub(super) fn ensure_project_memory_replica(memory: &ProjectMemoryLaunchConfig) -> Result<()> {
    let seed = mj_core::worker_launch::project_memory_seed_root(&memory.baseline_root);
    if !memory.root.exists() {
        anyhow::ensure!(
            seed.is_dir(),
            "project memory seed is missing: {}",
            seed.display()
        );
        if memory.baseline_root.exists() {
            std::fs::remove_dir_all(&memory.baseline_root)?;
        }
        let parent = memory
            .root
            .parent()
            .context("project memory root has no parent")?;
        std::fs::create_dir_all(parent)?;
        let temporary = tempfile::tempdir_in(parent)?;
        let baseline = tempfile::tempdir_in(parent)?;
        super::reload_commit::copy_overlay(&seed, temporary.path())?;
        super::reload_commit::copy_overlay(&seed, baseline.path())?;
        std::fs::rename(baseline.path(), &memory.baseline_root)?;
        std::fs::rename(temporary.path(), &memory.root)?;
    } else if !memory.baseline_root.exists() {
        let parent = memory
            .baseline_root
            .parent()
            .context("project memory baseline has no parent")?;
        std::fs::create_dir_all(parent)?;
        let baseline = tempfile::tempdir_in(parent)?;
        super::reload_commit::copy_overlay(&memory.root, baseline.path())?;
        std::fs::rename(baseline.path(), &memory.baseline_root)?;
    }
    if seed.exists() {
        std::fs::remove_dir_all(&seed)?;
    }
    Ok(())
}

/// A pending delivery is recovered before the worker accepts any requests.
pub(super) fn recover_project_memory(memory: &ProjectMemoryLaunchConfig) -> Result<()> {
    Record::load(memory)?.finish_incoming(memory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::project_memory::{
        SwapOutcome, commit_canonical, merge_trees, read_canonical_state,
    };
    use mj_core::relay::ProjectMemoryDeliveryOutcome;
    use std::path::Path;

    fn setup(root: &Path) -> (ProjectMemoryLaunchConfig, std::path::PathBuf) {
        let memory = ProjectMemoryLaunchConfig {
            project_key: "test".into(),
            root: root.join("replica"),
            baseline_root: root.join("baseline"),
            repository_roots: Default::default(),
            mcp_delivery: Default::default(),
            history_socket: None,
        };
        let canonical = root.join("canonical/memory");
        for dir in [&memory.root, &memory.baseline_root, &canonical] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("note.md"), "initial\n").unwrap();
        }
        recover_project_memory(&memory).unwrap();
        (memory, canonical)
    }

    fn handout(
        memory: &ProjectMemoryLaunchConfig,
        canonical: &Path,
    ) -> (ProjectMemorySnapshot, String, ProjectMemorySnapshot) {
        let state = read_canonical_state(canonical).unwrap();
        snapshot(
            memory,
            state.absorbed.get("session").map(String::as_str),
            &state.version,
        )
        .unwrap()
        .unwrap()
    }

    fn commit(
        canonical: &Path,
        base: &ProjectMemorySnapshot,
        id: &str,
        replica: &ProjectMemorySnapshot,
    ) -> ProjectMemorySnapshot {
        let state = read_canonical_state(canonical).unwrap();
        let merged = merge_trees(base, &state.tree, replica).tree;
        if base != replica {
            assert_eq!(
                commit_canonical(canonical, &state, "session", &merged, Some(id)).unwrap(),
                SwapOutcome::Swapped
            );
        }
        merged
    }

    fn apply(
        memory: &ProjectMemoryLaunchConfig,
        id: &str,
        tree: ProjectMemorySnapshot,
    ) -> ProjectMemoryDeliveryOutcome {
        let version = tree.version();
        deliver(memory, id, tree, version, &Default::default()).unwrap()
    }

    #[test]
    fn dirty_delivery_merges_edits_with_foreign_changes_on_next_round() {
        let temp = tempfile::tempdir().unwrap();
        let (memory, canonical) = setup(temp.path());
        std::fs::write(memory.root.join("local.md"), "first").unwrap();
        let (base, id, replica) = handout(&memory, &canonical);
        let mut state = read_canonical_state(&canonical).unwrap();
        let mut foreign = state.tree.clone();
        foreign
            .files
            .insert("/foreign.md".into(), "other session".into());
        assert_eq!(
            commit_canonical(&canonical, &state, "other", &foreign, Some("foreign")).unwrap(),
            SwapOutcome::Swapped
        );
        let merged = commit(&canonical, &base, &id, &replica);
        std::fs::write(memory.root.join("later.md"), "second").unwrap();
        assert_eq!(
            apply(&memory, &id, merged),
            ProjectMemoryDeliveryOutcome::Dirty
        );
        let (base, id, replica) = handout(&memory, &canonical);
        let merged = commit(&canonical, &base, &id, &replica);
        assert_eq!(
            apply(&memory, &id, merged.clone()),
            ProjectMemoryDeliveryOutcome::Delivered
        );
        state = read_canonical_state(&canonical).unwrap();
        assert_eq!(state.tree, merged);
        assert_eq!(
            ProjectMemoryStore::new(&memory.root).snapshot().unwrap(),
            merged
        );
        assert!(
            merged.files.contains_key("/local.md")
                && merged.files.contains_key("/later.md")
                && merged.files.contains_key("/foreign.md")
        );
    }

    #[test]
    fn lost_delivery_promotes_the_absorbed_handout() {
        let temp = tempfile::tempdir().unwrap();
        let (memory, canonical) = setup(temp.path());
        std::fs::write(memory.root.join("note.md"), "edited\n").unwrap();
        let (base, id, replica) = handout(&memory, &canonical);
        commit(&canonical, &base, &id, &replica);
        let (new_base, new_id, new_replica) = handout(&memory, &canonical);
        assert_eq!(new_base, replica);
        assert_eq!(new_replica, replica);
        assert_ne!(new_id, id);
        assert_eq!(
            apply(&memory, &new_id, new_replica),
            ProjectMemoryDeliveryOutcome::Delivered
        );
    }

    #[test]
    fn interrupted_canonical_commit_reuses_handout_without_duplicate_edits() {
        let temp = tempfile::tempdir().unwrap();
        let (memory, canonical) = setup(temp.path());
        std::fs::write(memory.root.join("note.md"), "initial\nnew line\n").unwrap();
        let (base, id, replica) = handout(&memory, &canonical);
        let before = read_canonical_state(&canonical).unwrap();
        let abandoned = merge_trees(&base, &before.tree, &replica).tree;
        // A crash before rename leaves the canonical state and handout untouched.
        let (retry_base, retry_id, retry_replica) = handout(&memory, &canonical);
        assert_eq!(retry_id, id);
        let merged = commit(&canonical, &retry_base, &retry_id, &retry_replica);
        assert_eq!(merged, abandoned);
        assert_eq!(merged.files["/note.md"], "initial\nnew line\n");
    }

    #[test]
    fn blank_replica_documents_are_deletions_in_snapshot_and_delivery() {
        let temp = tempfile::tempdir().unwrap();
        let (memory, canonical) = setup(temp.path());
        std::fs::write(memory.baseline_root.join("legacy-empty.md"), "  \n").unwrap();
        std::fs::write(memory.root.join("note.md"), " \n\t").unwrap();
        std::fs::write(memory.root.join("new-empty.md"), "").unwrap();
        // Start with a legacy directory baseline so its blank documents are
        // subject to the same rule as files in the live replica.
        std::fs::remove_file(memory.baseline_root.join(RECORD_FILE)).unwrap();
        let (base, id, replica) = handout(&memory, &canonical);
        assert!(!base.files.contains_key("/legacy-empty.md"));
        assert!(!replica.files.contains_key("/note.md"));
        assert!(!replica.files.contains_key("/new-empty.md"));
        let merged = commit(&canonical, &base, &id, &replica);
        assert!(
            merged
                .files
                .values()
                .all(|content| is_memory_document(content))
        );
        assert_eq!(
            apply(&memory, &id, merged.clone()),
            ProjectMemoryDeliveryOutcome::Delivered
        );
        assert_eq!(
            ProjectMemoryStore::new(&memory.root).snapshot().unwrap(),
            merged
        );
        assert!(!memory.root.join("note.md").exists());
    }

    #[test]
    fn startup_finishes_interrupted_delivery() {
        let temp = tempfile::tempdir().unwrap();
        let (memory, canonical) = setup(temp.path());
        let (_, id, mut replica) = handout(&memory, &canonical);
        replica.files.insert("/new.md".into(), "foreign".into());
        let mut record = Record::load(&memory).unwrap();
        record.incoming = Some(replica.clone());
        record.save(&memory).unwrap();
        std::fs::remove_file(memory.root.join("note.md")).unwrap();
        recover_project_memory(&memory).unwrap();
        assert_eq!(
            ProjectMemoryStore::new(&memory.root).snapshot().unwrap(),
            replica
        );
        let record = Record::load(&memory).unwrap();
        assert_eq!(record.base, replica);
        assert!(record.handout.is_none() && record.incoming.is_none());
        assert_eq!(
            apply(&memory, &id, record.base),
            ProjectMemoryDeliveryOutcome::Stale
        );
    }

    #[test]
    fn failed_incoming_render_is_retried_before_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let (memory, canonical) = setup(temp.path());
        let (_, _, mut tree) = handout(&memory, &canonical);
        tree.files.insert("/foreign.md".into(), "foreign".into());
        let mut record = Record::load(&memory).unwrap();
        record.incoming = Some(tree.clone());
        record.save(&memory).unwrap();
        std::fs::remove_dir_all(&memory.root).unwrap();
        std::fs::write(&memory.root, "blocks directory creation").unwrap();
        let state = read_canonical_state(&canonical).unwrap();
        assert!(snapshot(&memory, None, &state.version).is_err());
        assert_eq!(Record::load(&memory).unwrap().incoming, Some(tree.clone()));
        std::fs::remove_file(&memory.root).unwrap();
        let (base, _, replica) = handout(&memory, &canonical);
        assert_eq!(base, tree);
        assert_eq!(replica, tree);
        assert!(Record::load(&memory).unwrap().incoming.is_none());
    }

    // Hard-won: 844612f9f: retrying a failed delivery overwrote subsequent agent edits.
    #[test]
    fn failed_delivery_recovery_preserves_later_agent_edits() {
        let temp = tempfile::tempdir().unwrap();
        let (memory, canonical) = setup(temp.path());
        std::fs::write(memory.root.join("remaining.md"), "local old").unwrap();
        std::fs::write(memory.root.join("remove.md"), "remove old").unwrap();
        std::fs::write(memory.root.join("to-directory"), "old file").unwrap();
        std::fs::create_dir(memory.root.join("to-file")).unwrap();
        std::fs::write(memory.root.join("to-file/note.md"), "old child").unwrap();
        let (base, id, replica) = handout(&memory, &canonical);
        let mut incoming = replica.clone();
        incoming
            .files
            .insert("/note.md".into(), "remote note".into());
        incoming
            .files
            .insert("/remaining.md".into(), "remote remaining".into());
        incoming
            .files
            .insert("/foreign.md".into(), "foreign addition".into());
        incoming.files.remove("/remove.md");
        incoming.files.remove("/to-directory");
        incoming
            .files
            .insert("/to-directory/note.md".into(), "new child".into());
        incoming.files.remove("/to-file/note.md");
        incoming.files.insert("/to-file".into(), "new file".into());
        let state = read_canonical_state(&canonical).unwrap();
        assert_eq!(
            commit_canonical(&canonical, &state, "session", &incoming, Some(&id)).unwrap(),
            SwapOutcome::Swapped
        );
        let tools = Default::default();
        let failed = deliver_with_render(
            &memory,
            &id,
            incoming.clone(),
            incoming.version(),
            &tools,
            |root, _prepared| {
                std::fs::write(root.join("note.md"), "remote note")?;
                std::fs::remove_file(root.join("remaining.md"))?;
                std::fs::remove_file(root.join("remove.md"))?;
                std::fs::remove_file(root.join("to-directory"))?;
                std::fs::remove_file(root.join("to-file/note.md"))?;
                anyhow::bail!("render failed after partial application")
            },
        );
        assert!(failed.is_err());
        let pending = Record::load(&memory).unwrap();
        assert_eq!(pending.incoming.as_ref(), Some(&incoming));
        assert_eq!(pending.handout.as_ref().unwrap().id, id);
        std::fs::write(memory.root.join("note.md"), "agent edit after failure").unwrap();
        std::fs::write(memory.root.join("agent-added.md"), "agent addition").unwrap();

        let state = read_canonical_state(&canonical).unwrap();
        let (recovered_base, _, recovered_replica) = snapshot(
            &memory,
            state.absorbed.get("session").map(String::as_str),
            &state.version,
        )
        .unwrap()
        .unwrap();
        assert_eq!(recovered_base, incoming);
        assert_eq!(
            recovered_replica.files["/note.md"],
            "agent edit after failure"
        );
        assert_eq!(recovered_replica.files["/agent-added.md"], "agent addition");
        assert_eq!(recovered_replica.files["/remaining.md"], "remote remaining");
        assert_eq!(recovered_replica.files["/foreign.md"], "foreign addition");
        assert_eq!(
            recovered_replica.files["/to-directory/note.md"],
            "new child"
        );
        assert_eq!(recovered_replica.files["/to-file"], "new file");
        assert!(!recovered_replica.files.contains_key("/remove.md"));
        let record = Record::load(&memory).unwrap();
        assert!(record.incoming.is_none());
        assert_eq!(record.base, incoming);
        assert_eq!(record.handout.unwrap().tree, recovered_replica);
        assert_eq!(base.files["/note.md"], "initial\n");
    }

    // Hard-won: 844612f9f: a failed final record save caused recovery to overwrite later edits.
    #[test]
    fn recovery_after_record_save_failure_preserves_later_agent_edits() {
        let temp = tempfile::tempdir().unwrap();
        let (memory, canonical) = setup(temp.path());
        let (_, id, mut incoming) = handout(&memory, &canonical);
        incoming.files.insert("/note.md".into(), "delivered".into());
        incoming
            .files
            .insert("/foreign.md".into(), "foreign".into());
        let mut record = Record::load(&memory).unwrap();
        record.incoming = Some(incoming.clone());
        record.save(&memory).unwrap();
        mj_core::project_memory::render_memory_tree(&memory.root, &incoming, false).unwrap();
        // The final record save fails: the persisted record still has the
        // handout and incoming, although rendering completed.
        std::fs::write(memory.root.join("note.md"), "later edit").unwrap();
        std::fs::write(memory.root.join("new.md"), "later addition").unwrap();
        let state = read_canonical_state(&canonical).unwrap();
        let (base, _, replica) = snapshot(&memory, Some(&id), &state.version)
            .unwrap()
            .unwrap();
        assert_eq!(base, incoming);
        assert_eq!(replica.files["/note.md"], "later edit");
        assert_eq!(replica.files["/new.md"], "later addition");
        assert_eq!(replica.files["/foreign.md"], "foreign");
        assert!(Record::load(&memory).unwrap().incoming.is_none());
    }

    // Hard-won: 844612f9f: Busy was checked before delivery validation and record preparation.
    #[test]
    fn delivery_validates_before_reporting_a_busy_tool() {
        let temp = tempfile::tempdir().unwrap();
        let (memory, canonical) = setup(temp.path());
        let (_, id, replica) = handout(&memory, &canonical);
        let tools = mj_core::activity::ToolsInFlight::default();
        let call: agent_client_protocol::v1::schema::SessionUpdate = serde_json::from_value(serde_json::json!({"sessionUpdate":"tool_call", "toolCallId":"live", "title":"write", "kind":"execute", "status":"in_progress"})).unwrap();
        tools.observe_for_owner(&call, 1, mj_core::activity::ForegroundOwner::Unowned);
        let invalid = ProjectMemorySnapshot {
            files: [("/invalid/".into(), "content".into())].into(),
        };
        assert!(deliver(&memory, &id, invalid.clone(), invalid.version(), &tools).is_err());
        let record_path = memory.baseline_root.join(RECORD_FILE);
        let record_bytes = std::fs::read(&record_path).unwrap();
        std::fs::write(&record_path, "invalid record").unwrap();
        assert!(deliver(&memory, &id, replica.clone(), replica.version(), &tools).is_err());
        std::fs::write(&record_path, record_bytes).unwrap();
        assert_eq!(
            deliver(&memory, &id, replica.clone(), replica.version(), &tools).unwrap(),
            ProjectMemoryDeliveryOutcome::Busy
        );
        assert_eq!(
            ProjectMemoryStore::new(&memory.root).snapshot().unwrap(),
            replica
        );
        assert!(Record::load(&memory).unwrap().incoming.is_none());
    }

    #[test]
    fn foreign_deletion_does_not_resurrect_a_previously_pushed_edit() {
        let temp = tempfile::tempdir().unwrap();
        let (memory, canonical) = setup(temp.path());
        std::fs::write(memory.root.join("note.md"), "edited\n").unwrap();
        let (base, id, replica) = handout(&memory, &canonical);
        let merged = commit(&canonical, &base, &id, &replica);
        assert_eq!(
            apply(&memory, &id, merged),
            ProjectMemoryDeliveryOutcome::Delivered
        );
        let state = read_canonical_state(&canonical).unwrap();
        let mut deleted = state.tree.clone();
        deleted.files.remove("/note.md");
        assert_eq!(
            commit_canonical(&canonical, &state, "other", &deleted, Some("delete")).unwrap(),
            SwapOutcome::Swapped
        );
        let (base, id, replica) = handout(&memory, &canonical);
        let merged = commit(&canonical, &base, &id, &replica);
        assert_eq!(
            apply(&memory, &id, merged),
            ProjectMemoryDeliveryOutcome::Delivered
        );
        assert!(!memory.root.join("note.md").exists());
    }

    #[tokio::test]
    async fn checkpoint_preempts_turn_sync_and_finishes_its_committed_handout() {
        let temp = tempfile::tempdir().unwrap();
        let (memory, canonical) = setup(temp.path());
        std::fs::write(memory.root.join("note.md"), "edited\n").unwrap();
        let slot = std::sync::Arc::new(mj_core::recovery_gate::ProjectMemorySyncSlot::default());
        let (guard, cancel) = slot.start_turn().unwrap();
        let (ready, started) = tokio::sync::oneshot::channel();
        let turn_memory = memory.clone();
        let turn_canonical = canonical.clone();
        let turn = mj_core::runtime::spawn(async move {
            let _guard = guard;
            let (base, id, replica) = handout(&turn_memory, &turn_canonical);
            commit(&turn_canonical, &base, &id, &replica);
            ready.send(()).unwrap();
            cancel.cancelled().await;
        });
        started.await.unwrap();
        let _checkpoint = slot.checkpoint().await;
        turn.await.unwrap();
        let (base, id, replica) = handout(&memory, &canonical);
        let merged = commit(&canonical, &base, &id, &replica);
        assert_eq!(
            apply(&memory, &id, merged.clone()),
            ProjectMemoryDeliveryOutcome::Delivered
        );
        assert_eq!(read_canonical_state(&canonical).unwrap().tree, merged);
        assert_eq!(
            ProjectMemoryStore::new(&memory.root).snapshot().unwrap(),
            merged
        );
        assert_eq!(Record::load(&memory).unwrap().base, merged);
    }
}
