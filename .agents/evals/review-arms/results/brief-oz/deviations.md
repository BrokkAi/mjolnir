# Wizard-of-Oz arm: deviations from brief/PLAN-oz.md

1. Codex home: option A, chosen by Jonathan ("just drop it in normal codex ... clean up afterwards").
   Four overlay files review_arms_oz.config.toml in ~/.codex, .codex2, .codex3 and .codex4. To be
   removed after the arms.
2. One QT job (metadata-checkpoint-overwrites-checkout) ended with a reviewer without output. It
   was rerun once; the first attempt is in failed-attempts/oz-run1/.
3. Round I judges QT,QS,DT,DS,FT,FS and no-brief on all 10 diffs. Arm W is not re-judged in round
   I, because it has no sessions for symbol-change-keeps-cached-transcript (that brief was refused
   by the mj API at 413), and brief_score refuses an incomplete sample. W's labels come from round
   H (the same judge model, on its 9 diffs); Q - W is reported on those 9 diffs, with this caveat.
4. The smoke run (QT held-prompt-ignores-interrupt, before launch) is not counted; it is in smoke/.
