# ADR-0185: Installing an App Keeps Its Installed Dependencies

- Status: Accepted
- Date: 2026-11-17
- Deciders: Temper core maintainers
- Related:
  - ADR-0173: Genesis install is verified-or-reverted
  - `crates/temper-platform/src/genesis_install/closure_install.rs` (`uninstalled_apps`)
  - `crates/temper-platform/src/genesis_install.rs` (`GenesisClosureAdmission`, the closure walks)
  - TemperPaw `crates/temperpaw/src/startup.rs` (`classify_bootstrap_action`)

## Context

An install fetches its app's whole dependency closure, each dependency at the version the closure
pins, and installed every app in it. Installing one app therefore replaced every dependency it
shared with other installed apps, often with an older version. On 17 Nov 2026 a pinned install of
one app replaced an installed dependency with an older copy, and every release of another app that
needed the newer one stopped. Callers worked around it by rewriting each app's pins to whatever the
place already ran.

## Decision

### Sub-Decision 1: an install installs the named app, and a dependency only when none is installed

Installing an app puts that app in place, and each dependency of which no version is installed. An
installed dependency is never replaced or downgraded by installing something else; it is upgraded
by installing it. A dependency's pin names the version to bring when none is installed.

Installed means the tenant has an install record for the app's name, whatever its source, version
or status. This matches TemperPaw's boot, which keeps any recorded bootstrap app: the record is
what boot restores and what a failed install rolls back to. A dependency left out resolves from the
tenant's catalog at its installed version, and the install still reconciles and verifies it with the
root, so the rollback of ADR-0173 is unchanged.

The rule is one function, `uninstalled_apps`, applied where every install chooses the apps it puts
in place: the shared closure install (registry and pushed installs and their checks), the Genesis
`App.Install` hook, and the boot restore of `App.Install` caches.

**Why this approach**: other installed apps run on a shared dependency, so only an install of that
dependency may change it. Filtering the closure before anything is registered keeps the install path
otherwise as it was, with no setting.

### Sub-Decision 2: differing pins inside one closure are not an error

A closure refused two pins of one dependency at different versions. Under Sub-Decision 1 a pin no
longer decides what runs, so that refusal blocked installs for no reason. A closure still holds one
version of each app (its apps share one cache directory, by name); the closure walks are
breadth-first and the pin nearest the root is taken. Two different owners' apps of one name still
collide and are refused.

## Consequences

- Installing an app never changes another app's dependency; release lines install apps in
  dependency order to upgrade a dependency.
- Pins no longer need rewriting to match what a place runs.
- An install whose dependency has a record but whose files the tenant cannot resolve is refused
  rather than installing the pinned copy over it.
