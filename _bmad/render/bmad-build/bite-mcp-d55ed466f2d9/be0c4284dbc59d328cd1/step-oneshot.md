# Step One-Shot: Implement, Review, Present

You reach this step from step 2, or from step 1 when resuming a plan whose `route` is `oneshot`. `{plan_file}` already exists.

## RULES

- Do not push to a remote unless the user asks.
- Do not edit anything inside `<frozen-after-approval>` in `{plan_file}`.
- Review subagents must use the same model level as this session.
- Start all review subagents in this turn and wait for all of them to finish. Do not run them in the background or end your turn before they return.

## INSTRUCTIONS

### Implement

If intent gaps remain, present each as a numbered question with its options and what each option means, HALT for the human's answers, and fold the answers into the Intent.

Capture `baseline_revision` (current HEAD, or `NO_VCS` if version control is unavailable) into `{plan_file}` frontmatter before making any changes. If the frontmatter already contains `baseline_revision` (resumed run), preserve the existing value.

Build the change from `{plan_file}`. The Intent section is what you implement. As you work, add notes to `## Implementation Notes`: decisions you made, files you changed, surprises.

**When to stop.** Stop coding if the request left out something the user would notice in the result. Write the gap in `## Implementation Notes`, then ask the human — do not guess.

### Review

Write `review: 'none'`, `review_source: 'pinned'`, and `lenses_ran: []` to `{plan_file}` frontmatter.

### Finalize Plan

Update `{plan_file}`:

1. Set `status: 'built'` in the frontmatter.
2. If review found anything, add `## Review Triage Log` with one line per finding: verdict and evidence. For `false`, the disproof. For `maybe-false`, what would settle it. For rejected `low`, why it was not worth fixing.

### Commit

If git is available and there are uncommitted changes, commit with a conventional message based on the Intent. If git is not available, skip.

### Present



Give the user a short summary — one or two sentences:

- What changed.
- Review result, including anything deferred.
- Commit hash, if you made one.

Do not list files, repeat the plan, or walk through what you did unless asked.

Offer next steps in one line: create a PR (push first if needed) when git and a remote exist; use `bmad-walkthrough`; or make another change.

Stop and wait for the user.

Workflow complete.

## On Complete

If anything appears below, do it before exiting. Otherwise exit.


