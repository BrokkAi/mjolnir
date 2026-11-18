use super::*;

impl Controller {
    pub(super) fn subagent_parent_worktree(
        &self,
        id: &str,
    ) -> Option<&mj_core::state::ManagedWorktree> {
        match self.state.checkout(id).ok()?.effective() {
            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => Some(worktree),
            mj_core::state::Checkout::Attached { .. }
            | mj_core::state::Checkout::ManagedWorkspace
            | mj_core::state::Checkout::Borrowed { .. } => None,
        }
    }
}
