"""Claude Code tool policy for the pilot's mock engine."""

ENGINE_TOOLS = frozenset({"Read", "Grep", "Glob", "Write", "Edit"})

# Claude Code 2.1.326 SDK built-ins, legacy aliases and the MCP namespace.
# Frozen mj cannot pass a session-specific SDK tools array.
ENGINE_SETTINGS = {
    # No hook (including the frozen worker's PostToolBatch mailbox hook)
    # can run a shell on behalf of a model tool.
    "disableAllHooks": True,
    "disableClaudeAiConnectors": True,
    "autoMemoryEnabled": False,
    "env": {"ENABLE_TOOL_SEARCH": "false"},
    "permissions": {
        "deny": sorted({
            "Agent", "Artifact", "AskUserQuestion", "Bash", "ClaudeDesign",
            "CronCreate", "CronDelete", "CronList", "EndConversation", "EnterPlanMode",
            "EnterWorktree", "ExitPlanMode", "ExitWorktree", "KillBash", "KillShell",
            "ListAgents", "ListMcpResourcesTool", "LSP", "Monitor", "NotebookEdit",
            "OfferChromeSetup", "PowerShell", "Projects", "propose_skills",
            "PushNotification", "ReadMcpResourceDirTool", "ReadMcpResourceTool",
            "ReadNotifications", "RemoteTrigger", "ReportFindings", "ScheduleWakeup",
            "SendFeedback", "SendMessage", "SendUserFile", "SendUserMessage", "SetGoal",
            "ShareOnboardingGuide", "ShowOnboardingRolePicker", "Skill", "SubagentHandback",
            "Task", "TaskCreate", "TaskGet", "TaskList", "TaskOutput", "TaskStop",
            "TaskUpdate", "TodoWrite", "ToolSearch", "WaitForMcpServers", "WebFetch",
            "WebSearch", "Workflow", "mcp__*",
        }),
        "blockReadsOutsideWorkingDirectories": True,
        "blockWritesOutsideWorkingDirectories": True,
    },
}
