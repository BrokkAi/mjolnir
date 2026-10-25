use super::*;

impl SetupDialog {
    pub(crate) fn path_input_context(&self) -> String {
        format!(
            "settings:{}:{}:{:?}",
            self.generation,
            self.draft,
            self.focused_field_path().and_then(|path| self
                .fields
                .get(&path)
                .map(|field| (path, field.input.value())))
        )
    }

    pub(super) fn resolve_path_action(
        &mut self,
        path: &[String],
    ) -> Result<Option<DashboardAction>, String> {
        let Some(field) = self.fields.get(path) else {
            return Ok(None);
        };
        if schema::path_kind(path) != Some(schema::PathKind::Target)
            || !mj_core::path_input::needs_home(std::path::Path::new(field.input.value()))
                .map_err(|e| e.to_string())?
        {
            return Ok(None);
        }
        let machine = self
            .machine_for_path(path)
            .ok_or_else(|| "This setting has no machine to resolve the path on".to_owned())?;
        self.notice = Some("Resolving path…".into());
        Ok(Some(DashboardAction::ResolveSetupPath {
            generation: self.generation,
            draft: self.draft.clone(),
            path: path.to_vec(),
            value: field.input.value().to_owned(),
            machine: Box::new(machine),
        }))
    }

    /// The machine a draft path belongs to: the machine itself under
    /// `machines`, and the machine a runtime names under `targets`.
    pub(super) fn machine_for_path(&self, path: &[String]) -> Option<mj_core::config::Machine> {
        let machine_id = match path.first().map(String::as_str)? {
            "machines" => path.get(1)?.clone(),
            "targets" => self.draft["targets"][path.get(1)?]["machine"]
                .as_str()
                .unwrap_or(mj_core::config::LOCAL_MACHINE_ID)
                .to_owned(),
            _ => return None,
        };
        self.machine(&machine_id)
    }

    /// One machine from the draft. This machine is always available, whether
    /// or not the draft spells it out.
    pub(super) fn machine(&self, machine_id: &str) -> Option<mj_core::config::Machine> {
        match self.draft["machines"].get(machine_id) {
            Some(machine) => serde_json::from_value(machine.clone()).ok(),
            None if machine_id == mj_core::config::LOCAL_MACHINE_ID => {
                Some(mj_core::config::Machine::Local { build_cache: None })
            }
            None => None,
        }
    }
}
impl crate::wizards::CompletesPaths for SetupDialog {
    /// A local setting completes on the controller; a target setting
    /// completes on the machine it configures. A destination inside a
    /// container that does not exist yet completes nowhere.
    fn focused_path_input(
        &mut self,
        _dashboard: &DashboardState,
    ) -> Option<(
        &mut PathInput,
        mj_core::path_completion::CompletionHost,
        mj_core::path_completion::CompletionKind,
    )> {
        use mj_core::path_completion::{CompletionHost, CompletionKind};
        let path = self.focused_field_path()?;
        if !matches!(self.fields.get(&path)?.input, EditorInput::Path(_)) {
            return None;
        }
        let (host, kind) = match schema::path_kind(&path)? {
            schema::PathKind::Local => (CompletionHost::Local, schema::completion_kind(&path)),
            // The machine is read before the input is borrowed mutably.
            schema::PathKind::Target => (
                CompletionHost::Machine(Box::new(self.machine_for_path(&path)?)),
                CompletionKind::Directories,
            ),
            schema::PathKind::RelativeDestination => return None,
        };
        let EditorInput::Path(input) = &mut self.fields.get_mut(&path)?.input else {
            return None;
        };
        Some((input, host, kind))
    }

    fn dismiss_unfocused_completions(&mut self) {
        let focused = self.focused_field_path();
        for (path, field) in &mut self.fields {
            if focused.as_ref() != Some(path)
                && let EditorInput::Path(input) = &mut field.input
            {
                input.dismiss_completion();
            }
        }
    }
}
