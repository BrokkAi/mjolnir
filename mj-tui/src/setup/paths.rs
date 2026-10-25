use super::*;

impl SetupDialog {
    pub(crate) fn path_input_context(&self) -> String {
        format!(
            "settings:{}:{}:{:?}",
            self.generation,
            self.draft,
            self.editor
                .as_ref()
                .map(|editor| (&editor.path, editor.input.value()))
        )
    }

    pub(super) fn resolve_path_action(&mut self) -> Result<Option<DashboardAction>, String> {
        let Some(editor) = &self.editor else {
            return Ok(None);
        };
        if editor.adding
            || schema::path_kind(&editor.path) != Some(schema::PathKind::Target)
            || !mj_core::path_input::needs_home(std::path::Path::new(editor.input.value()))
                .map_err(|e| e.to_string())?
        {
            return Ok(None);
        }
        let machine = self
            .machine_for_path(&editor.path)
            .ok_or_else(|| "This setting has no machine to resolve the path on".to_owned())?;
        self.notice = Some("Resolving path…".into());
        Ok(Some(DashboardAction::ResolveSetupPath {
            generation: self.generation,
            draft: self.draft.clone(),
            path: editor.path.clone(),
            value: editor.input.value().to_owned(),
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
        if !self.form.borrow().is_focused(SetupControl::Field) {
            return None;
        }
        let editor = self.editor.as_ref()?;
        if editor.adding || !matches!(editor.input, EditorInput::Path(_)) {
            return None;
        }
        let (host, kind) = match schema::path_kind(&editor.path)? {
            schema::PathKind::Local => {
                (CompletionHost::Local, schema::completion_kind(&editor.path))
            }
            // The machine is read before the input is borrowed mutably.
            schema::PathKind::Target => (
                CompletionHost::Machine(Box::new(self.machine_for_path(&editor.path)?)),
                CompletionKind::Directories,
            ),
            schema::PathKind::RelativeDestination => return None,
        };
        let EditorInput::Path(input) = &mut self.editor.as_mut()?.input else {
            return None;
        };
        Some((input, host, kind))
    }

    fn dismiss_unfocused_completions(&mut self) {
        if self.form.borrow().is_focused(SetupControl::Field) {
            return;
        }
        if let Some(Editor {
            input: EditorInput::Path(input),
            ..
        }) = &mut self.editor
        {
            input.dismiss_completion();
        }
    }
}
