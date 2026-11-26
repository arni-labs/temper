# Temper development checks

Shared workflow is owned by Stack. [REVIEW.md](../REVIEW.md) gives the repository review criteria; review and evidence are advisory and use plain language.

The active checks are:

- `.claude/settings.json`: spec verification, determinism and dependency checks while editing.
- `scripts/setup-hooks.sh`: local pre-commit spec/dependency/code checks and pre-push compile, lint and test checks.
- `ci/box/gate.sh`: build, lint, tests and the kernel's runtime invariants, run for each push to main on the warm Tensorlake box ci-temper (`ci/box/box.sh run` runs it by hand).

Run relevant checks for the changed behavior and report the results and any skipped coverage. There are no review markers, proof packets, planning-document checks or session-exit gates.
