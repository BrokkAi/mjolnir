use super::*;
use mj_core::config::DEFAULT_WORKSPACE_PREFIX;

fn ssh_machine(host: &str) -> Machine {
    Machine::Ssh {
        ssh: SshConnection {
            host: host.to_owned(),
            user: Some("dev".to_owned()),
            identity_file: None,
            extra_args: vec![],
        },
        workspace_prefix: DEFAULT_WORKSPACE_PREFIX.into(),
        build_cache: None,
    }
}

fn found(kind: RuntimeKind, status: CheckStatus) -> RuntimeProbe {
    RuntimeProbe {
        kind,
        status,
        detail: String::new(),
        remediation: None,
    }
}

/// What a probe of `machine` found: a worker platform and Docker, no Podman.
fn answered(machine_id: &str, machine: &Machine) -> MachineProbe {
    MachineProbe {
        machine_id: machine_id.to_owned(),
        machine: machine.clone(),
        outcome: MachineProbeOutcome::Answered(vec![
            found(RuntimeKind::Bare, CheckStatus::Ready),
            found(RuntimeKind::Podman, CheckStatus::Fixable),
            found(RuntimeKind::Docker, CheckStatus::Ready),
        ]),
    }
}

/// The probe runs outside the configuration lock and the decision inside it,
/// so what a probe found reaches the file only for a machine that still has
/// no runtime and is still the host that was probed, and only once however
/// many clients probe it.
#[test]
fn probed_runtimes_are_added_once_and_only_to_the_machine_that_was_probed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let builder = ssh_machine("builder.example.test");
    let moved = ssh_machine("moved.example.test");
    let busy = ssh_machine("busy.example.test");
    Config::update_to(&path, |config| {
        config.machines.insert("builder".into(), builder.clone());
        config.machines.insert("moved".into(), moved.clone());
        config.machines.insert("busy".into(), busy.clone());
        // An existing runtime takes the id the bare runtime would like.
        config
            .targets
            .insert("builder".into(), TargetTemplate::LocalBare);
        Ok(())
    })
    .unwrap();
    let probes = [
        answered("builder", &builder),
        answered("moved", &moved),
        answered("busy", &busy),
    ];

    // While they were probed, one machine moved to another host and another
    // gained a runtime from a different client.
    Config::update_to(&path, |config| {
        config
            .machines
            .insert("moved".into(), ssh_machine("elsewhere.example.test"));
        let (id, podman) = machine_runtime("busy", &busy, RuntimeKind::Podman).unwrap();
        config.targets.insert(id, podman);
        Ok(())
    })
    .unwrap();

    let added = add_machine_runtimes(&path, &probes, &|| false).unwrap();
    assert_eq!(
        added,
        BTreeMap::from([(
            "builder".to_owned(),
            vec!["builder-2".to_owned(), "builder-docker".to_owned()]
        )])
    );
    let text = std::fs::read_to_string(&path).unwrap();
    for (id, kind) in [("builder-2", "bare"), ("builder-docker", "docker")] {
        let table = text
            .split(&format!("[targets.{id}]\n"))
            .nth(1)
            .unwrap_or_else(|| panic!("{id} is written:\n{text}"));
        let table = table.split("\n[").next().unwrap();
        assert!(table.contains(&format!("kind = \"{kind}\"")), "{table}");
        assert!(table.contains("machine = \"builder\""), "{table}");
    }
    let config = Config::load_from(&path).unwrap();
    assert_eq!(config.machine_runtimes("moved").count(), 0);
    assert_eq!(
        config
            .machine_runtimes("busy")
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        ["busy-podman"]
    );

    // A second client's probe of the same machine adds nothing and leaves
    // the file as it is.
    assert!(
        add_machine_runtimes(&path, &probes, &|| false)
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
}
