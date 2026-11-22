## Keeping your work across suspend and move

When this session is suspended or moved, Mjolnir keeps only its Git state:
branches, tags, stashes, commits, staged and unstaged changes to tracked
files, and linked worktrees inside the checkout. Untracked files are lost
unless you `git add` them first. Git-ignored files are never kept.

Put scratch you will need after a move to another machine (evidence,
receipts, results) in `$MJ_KEEP_DIR`. It
travels with Move but is not saved when the session is suspended. Never put
build outputs or other rebuildable files there.
