# ADR-0011: Config Keys for the AGENTS.md Cap and Queue Ticket TTL; OS Machine-Id; `.venv` Policy

- **Status:** Accepted (accepted by Lucas Pan, 2026-11-03; `SPEC.md` §6, §11, §12, §13 amended; implementation `T-052`)
- **Date:** 2026-11-03
- **Author:** Claude Opus 5.5 (Agent)
- **Authority:** `SPEC.md` §6 (config; `[machine] id` "auto-derived from OS machine-id if absent"), §11 (`.venv` "recreated from lock"), §12 (ticket TTL), §13 (cap "10 lines by default (configurable)"), §16 (`MT-050`); `AGENTS.md` Standing Order #1

## Context

Four SPEC statements have no implementation path because §6 defines no key or the policy is ambiguous. Each was noted in handoffs (`T-026`, `T-040`, `T-005`/`T-016`, 2026-10-30 audit) and never tasked:

1. §13: the `AGENTS.md` block is "capped at 10 lines by default (configurable)"; there is no key, so `DEFAULT_LINE_CAP` is hard-coded.
2. §12: slots are held "until the command exits or the ticket TTL lapses"; there is no key, so `T-040` uses `lease.ttl_secs` (900 s) counted from the grant.
3. §6: the machine id is "auto-derived from OS machine-id if absent"; the code uses `mc-<hostname>`, which collides across machines with the same host name and changes when a host is renamed.
4. §11: `.venv` is "recreated from lock", but MeshTree has no step that runs a recreate command and §6 defines none.

## Decision

1. **`[agents] max_lines`** (integer ≥ 1, default 10): cap for the generated block. Invalid ⇒ `MT-050`.
2. **`[queue.<name>] ticket_ttl_secs`** (integer ≥ 1, default `lease.ttl_secs`): how long a granted slot may be held. Invalid ⇒ `MT-050`.
3. **OS machine-id derivation** when `[machine] id` is absent: `mc-` + first 16 hex digits of SHA-1 over the OS machine identifier — Linux `/etc/machine-id` (fallback `/var/lib/dbus/machine-id`), macOS `IOPlatformUUID` (`ioreg -rd1 -c IOPlatformExpertDevice`), Windows `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`. If none can be read, fall back to today's `mc-<hostname>` with a one-time note. Hashing keeps the raw OS identifier out of the shared coordination ref.
4. **`.venv`:** MeshTree never shares or recreates it. §11's "recreated from lock" means by the project's own tooling (e.g. a `validate` step or the agent), following the recipes in `docs/compatibility.md`. `strategy = "none"` is the only `.venv` behavior.

## SPEC amendments on acceptance

- §6 example config gains `[agents] max_lines = 10` and `ticket_ttl_secs` under one `[queue.*]` example; the `[machine]` comment names the derivation and its fallback.
- §11 table: `.venv` default policy "not shared (recreate with project tooling)".
- §12 and §13: reference the new keys.

## Consequences

- Pre-1.0 change of default machine ids: hosts without `[machine] id` change from `mc-<hostname>` to `mc-<hash>`. Leases held under the old id expire normally (TTL); no migration.
- Implementation tracked as a new task (`T-052`).
