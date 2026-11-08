# ADR-0075: Tenant Secrets Key Management

- Status: Accepted
- Date: 2026-04-29
- Deciders: Temper core maintainers
- Related:
  - ADR-0004: Cedar Authorization for Agents
  - ADR-0044: Platform Secrets Untangle Default Tenant
  - ADR-0065: Postgres Platform Store and Canonical Schema
  - `crates/temper-server/src/secrets/`
  - `crates/temper-store-postgres/src/platform.rs`

## Context

Postgres parity includes tenant secret storage. Moving secrets from Turso to Postgres must not weaken the existing "encrypted before persistence, filtered before WASM injection" contract.

## Decision

Tenant secrets are encrypted in application code before database persistence. Postgres stores only:

- `tenant`
- `key_name`
- AES-256-GCM ciphertext bytes
- nonce bytes
- timestamps

The master key is provided through `TEMPER_VAULT_KEY` as base64-encoded 32 bytes in production. If absent, local development may generate an ephemeral in-memory key, but that mode is not valid for production because persisted secrets cannot be decrypted across restarts.

Secret injection into WASM remains filtered by authorization before constructing the guest environment. Storage migration does not change the Cedar surface.

## Readiness Gates

- Production has `TEMPER_VAULT_KEY` set before any Postgres cutover that migrates secrets.
- A staging cutover verifies one migrated secret can be decrypted after service restart.
- WASM secret filtering tests pass for allowed and denied modules.
- Migration manifests count tenant secrets without logging plaintext.

## Consequences

### Positive

- Database administrators and backups do not receive plaintext secrets.
- The same vault boundary works for Turso and Postgres-backed storage.

### Negative

- Losing `TEMPER_VAULT_KEY` makes persisted secrets unrecoverable.
- Rotating the key needs both keys present for one boot (see Key Rotation).

### DST Compliance

Secret encryption uses production randomness and is outside deterministic simulation. Sim tests should use fixed fixture values or mocked vault behavior.

## Key Rotation (2026-11-07)

`TEMPER_VAULT_KEY_PREVIOUS` (optional, same format) holds the key being replaced. At boot each tenant secret row is opened with the current key first, then the previous one:

- Current key opens it: kept as stored.
- Only the previous key opens it: sealed again under the current key and written back on the condition that its version id is unchanged since it was read. A row changed meanwhile is read again and loaded as it now is. A re-seal that cannot be written (no version id, a failed write, or a row rewritten still under the previous key) is deferred: its value is loaded and the next boot retries it. A re-sealed row opens with the current key, so a retry or second boot changes nothing.
- Neither opens it: logged as an error per row and not loaded.

The boot prints a line for the tenants restored before serving, and the restore walk prints the final line, covering every tenant, when it ends: rows kept, re-sealed, deferred and unreadable, and tenants unchecked (unavailable, or never restored). The previous key never seals anything. Setting it without `TEMPER_VAULT_KEY` stops the boot, since rows would be re-sealed under a throwaway key. Without it, loading is unchanged except that an unreadable row is an error, not a warning.

To rotate: set the new key as `TEMPER_VAULT_KEY` and the old one as `TEMPER_VAULT_KEY_PREVIOUS`, and boot until the walk's final line shows 0 re-sealed, 0 deferred and 0 tenants unchecked; only then remove the previous key. Run one release at a time during a rotation: a release older than secret version ids writing during the overlap can lose its write to a re-seal. Once a row is re-sealed only the new key opens it, so a rollback, including to a release without this variable, keeps the new key as `TEMPER_VAULT_KEY`.

## Rollback Policy

Before cutover, continue serving secrets from Turso. After cutover, rollback requires the same `TEMPER_VAULT_KEY` to remain available so the Postgres target can be inspected or re-migrated safely.
