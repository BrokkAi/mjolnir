use super::*;
use mj_core::targets::SshTrafficClass;

/// Reuse the same Git configuration resolver on a remote bare host.
pub struct RemoteGitExecutor<'a, E> {
    pub executor: &'a E,
    pub ssh: SshTarget,
}

impl<E: CommandExecutor> RemoteGitExecutor<'_, E> {
    fn remote(&self, command: &CommandSpec) -> CommandSpec {
        let mut arguments = vec!["env".to_owned()];
        arguments.extend(
            command
                .env
                .iter()
                .map(|(key, value)| format!("{key}={value}")),
        );
        arguments.push(command.program.clone());
        arguments.extend(command.args.clone());
        ssh_command(SshTrafficClass::Control, &self.ssh, arguments).purpose(&command.purpose)
    }
}

impl<E: CommandExecutor> CommandExecutor for RemoteGitExecutor<'_, E> {
    fn execute_to_file(
        &self,
        command: &CommandSpec,
        output: &std::fs::File,
    ) -> Result<CommandOutput> {
        self.executor.execute_to_file(&self.remote(command), output)
    }

    fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
        self.executor.pause_deadline()
    }

    fn cancellation_requested_without_deadline(&self) -> bool {
        self.executor.cancellation_requested_without_deadline()
    }

    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.executor.execute(&self.remote(command))
    }

    fn execute_without_deadline(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.executor
            .execute_without_deadline(&self.remote(command))
    }

    fn execute_with_stdin_without_deadline(
        &self,
        command: &CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> Result<CommandOutput> {
        // Like `execute_with_stdin`, streamed input is not supported here.
        self.execute_with_stdin(command, input)
    }

    fn cancellation_requested(&self) -> bool {
        self.executor.cancellation_requested()
    }
}
