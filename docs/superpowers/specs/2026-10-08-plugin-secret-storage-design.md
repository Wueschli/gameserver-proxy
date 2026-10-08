# Plugin secret storage (review for #235)

Status: design for review (2026-10-08). Decisions are marked **[decided]** where the earlier specs already fixed them; everything else is the recommendation the build plan follows unless the maintainer changes it. No code in this change.

This is the security review that the [automation hooks addendum](2026-10-07-plugin-automation-hooks-design.md) ("Secrets") and the [plugin system design](2026-10-07-plugin-system-design.md) ("Capabilities and trust") make a **hard gate**: no plugin that uses secrets ships, and the `http` import does not expand `${secret:NAME}`, until this design is implemented and its tests pass. It decides the five open items of #235: key rotation, how a joining node gets the key, a node without the key, log compaction, and redaction.

## Threat model

Who must not learn a secret, and what protects it:

| Reader | Has | Must learn secrets? | Protection |
|--------|-----|---------------------|------------|
| Holder of a Raft log, snapshot, `sled` directory or backup | ciphertext | no | AEAD encryption at rest, key never stored beside it |
| Admin API client | bearer token | no, but may set or delete one | no read path exists; only `set`, `delete` and a "is set" flag |
| A plugin (guest code) | its own memory, host imports | no | the host expands the secret into the request; the guest only sees `${secret:NAME}` and the response, scrubbed |
| An approved destination host | the request | yes, for its own slot only | slots are bound to hosts; a token is never sent to a second approved host |
| Someone with root on a controller node | process memory, key file | yes | out of scope; the key is on every node by design. Node compromise is key compromise |

Consequence worth stating plainly: **the cluster key is as sensitive as every secret together.** Compaction and rotation reduce what an old backup is worth, they do not make a stolen key harmless. If the key leaks, the operator must also rotate the upstream credentials (the panel token), not only the cluster key.

## Key model

- **[decided]** One cluster key, provisioned identically to every controller node out of band. It is never written to the replicated log, a snapshot, the `sled` database or an API response.
- Source: `--plugin-secret-key-file <path>` (preferred; refused when the file is readable by group or others on Unix) or the environment variable `WAYHOUSE_PLUGIN_SECRET_KEY`. Neither set means the node has **no key**: the plugin secrets API answers `409` ("no secret key configured") and plugins with secret slots are held (below). No default, no generated key: a key the controller invented would not survive a node replacement.
- Key material must come from a CSPRNG: the operator generates each key with `openssl rand -base64 32` (the same in fish). The key is used as raw AEAD key bytes with no password-based derivation, so a human-chosen or predictable value would let anyone holding a log, snapshot or backup guess it offline. The controller rejects a line that does not decode to exactly 32 bytes; it cannot check entropy, so the docs and the `--help` text say to use a generated key.
- The key file is a **keyring**: one 32-byte key per line, base64, in order of age; the **last line is the active key** that encrypts new writes, earlier lines only decrypt. A single line is the common case.
- Each key has an id, the first 8 bytes of `sha256("wayhouse-plugin-secret-key-id" || key)`, hex. The id is stored with each ciphertext and is not secret.
- Cipher: **XChaCha20-Poly1305** (the `chacha20poly1305` crate, already in `Cargo.lock`), random 192-bit nonce per encryption, so no nonce counter has to be coordinated across nodes. The associated data binds the ciphertext to its place: `install id || 0x00 || slot name || 0x00 || key id`. A ciphertext copied to another slot or install fails authentication.
- Plaintext lives only in a `zeroize` buffer for the duration of the host call that expands it. It is never stored in a plain `String` field, never in a `Debug` or `Display` impl (the type prints `<redacted>`), and never in guest memory.

## What is replicated

New Raft entries, gated for rolling upgrades like every plugin entry (addendum, "HA and state"):

- `PluginSecretSet { install_id, slot, key_id, nonce, ciphertext, updated_at, actor }`
- `PluginSecretDelete { install_id, slot, actor }`
- `PluginSecretRewrap { items: [{install_id, slot, key_id, nonce, ciphertext}] }`: used by rotation, applied atomically.

Install records (#220 slice 3) never contain secret material. The install's approved capabilities already name the slots and their bound hosts.

**Encryption happens on the node that receives the API call, before anything is proposed.** A follower that gets a `PUT` encrypts with its own key and forwards the ciphertext entry to the leader; plaintext never crosses the peer link and never reaches a Raft log. Because every node holds the same keyring, any node can do this and any node can decrypt what another wrote. Standalone controllers use the same code path with a local store.

## Admin API

All routes sit behind the existing admin bearer layer (no new auth scheme) and answer `501` with a reason on slave tiers and while HA replication of installs is not built.

- `PUT /plugins/{id}/secrets/{slot}`: body `{"value": "..."}`, at most 4 KiB. The slot must be one of the install's **approved** slots (`404` otherwise); an empty value is `400`. Returns `{slot, set: true, key_id, updated_at}`.
- `DELETE /plugins/{id}/secrets/{slot}`: `204`.
- `GET /plugins/{id}/secrets`: per slot `{slot, description, bound_hosts, set, key_id, updated_at}`. **There is no endpoint that returns a value, and `GET /plugins/{id}` and the install list never include secret fields.** The UI shows "set / not set" and a replace field.
- Secret values are accepted only over the admin listener's TLS or a loopback bind; the controller refuses to start `--plugins` with a secret key on a plain-HTTP non-loopback listener unless `--allow-insecure-secrets` is given (loud warning). Request bodies of these routes are never logged: the request-logging layer skips them, and the value type has no `Debug`.
- Deleting an install deletes its secrets in the same entry.
- Every set, delete and rewrap writes an audit log line (`install`, `slot`, `key_id`, `actor`, never the value).

## Binding and expansion

Unchanged from the system design, made precise:

- A slot is bound to a list of hosts in the module's embedded capabilities and approved with them. A module update that adds a slot or widens a binding needs re-approval (existing rule).
- Expansion happens in the `http` host import, in **header values only**, for the exact token `${secret:NAME}` (no partial or computed names; query strings and bodies are not expanded). A token for an unknown, unset or not-approved slot fails the call; it is never sent literally.
- The check runs against the **connection's** host, per hop, including every redirect hop the `http` import follows: every redirect re-evaluates the binding against the new host. A target that is approved and also bound to the slot may receive the expanded secret; a target that is approved but **not** bound to it fails the call (the secret is never expanded for it, and the header is never sent literally). Redirects to unapproved hosts are already refused.

## Redaction

- Response headers, body and error text returned to the guest are scrubbed: any occurrence of an expanded secret value, or its base64 or percent-encoded form, is replaced by `[redacted]` before the guest sees it. This blocks the simplest echo attacks (a destination that reflects the `Authorization` header); it is not a defense against a hostile destination, which already holds the secret by design.
- Secrets shorter than 8 bytes are refused at `PUT` (`400`), because scrubbing a 3-character value would mangle ordinary text and gives no protection.
- Host-side logging of a request prints header names, never values, for slots that expanded. Plugin `log()` lines are scrubbed with the same function before they are emitted. The scrub list is built per call from the slots the call used.
- Error responses of the secrets API never echo the submitted value.

## Missing key: held, not run

Per node, per install, the status is one of: `active`, `pending` (module blob not yet here), `held: no secret key`, `held: key <id> missing`, `held: secret unset`.

- A node evaluates this when it decodes the ciphertexts at install activation and again on every key reload. An install that uses secret slots **never ticks, handles a webhook or receives an event** on a node whose keyring lacks the key id of any secret it needs, or that lacks a value for a declared slot. It is held, not run without its secret, and the status is visible in the plugin API and UI.
- Only the leader runs plugins. A node that becomes leader without the key therefore holds every secret-using plugin, raises the same visible alert as a missing module blob (addendum, "Module bytes in HA"), and the other nodes' plugin status shows which node lacks which key id. Plugins that declare no secret slots are unaffected.
- A missing or unreadable key is **not** an error that stops the controller: config distribution, the registries and the tunnel keep working. Only plugin secrets are affected.

## A joining node

The key is **not** distributed through the cluster or the peer protocol: that would put the key one authenticated request away from any peer and defeat out-of-band provisioning. A node added with `--ha-join` (or replaced after a disk loss) must be started with the same key source as the others, exactly as it must carry the same `--ha-token` today. Without it the node joins, replicates ciphertext and runs held; supplying the key later and sending `SIGHUP` (or restarting) activates the plugins. `GET /admin/ha/members` shows each node's key ids so a missing key is visible before a failover needs it. Losing every copy of the key loses every secret: the operator re-enters them, and the docs say so next to the backup instructions.

## Rotation

Rotation replaces the active key; it is an operator action in three steps and never needs downtime.

1. **Add.** Append a new key line to the keyring on every node and reload (`SIGHUP`). New writes use the new key; old ciphertexts still decrypt with the old line. Nodes that have not yet reloaded can still read the new ciphertext only after they get the new line, so the operator adds the line everywhere before any write; the members view lists key ids per node to confirm.
2. **Rewrap.** `POST /admin/plugins/secrets/rewrap` (leader-side): decrypts every secret and proposes one `PluginSecretRewrap` entry that re-encrypts them under the active key. It is idempotent, and refuses to run while any node lacks the active key (it reports which).
3. **Retire.** Once `GET /plugins/{id}/secrets` shows the new `key_id` everywhere (and the old key is no longer used by any replicated ciphertext), the operator removes the old line from the keyring. Retiring early is safe by construction: a secret still under a removed key just reads as `held: key <id> missing` until the operator re-enters or restores the key.

A leaked key is handled as: rotate the upstream credentials first, then rotate the cluster key. Rewrapping does not make old log segments or backups that were encrypted under the leaked key safe.

## Log compaction

The Raft log keeps entries until a snapshot lets it purge (`raft_config`, currently the last 1000 entries after each snapshot; `crates/wayhouse-controller/src/ha/mod.rs`), and `sled` does not overwrite freed pages. A superseded or deleted `PluginSecretSet` therefore stays readable as ciphertext in the log, in the `sled` file and in every backup of either.

**This is acceptable because it is ciphertext**, and the design does not rely on erasure for confidentiality. It still limits how long and how widely superseded ciphertext lingers:

- A `PluginSecretSet` replaces the previous entry for the slot, so a snapshot only ever contains the current ciphertext per slot.
- After a secret delete or rewrap commits, the leader requests a snapshot and a log purge up to that entry (`Raft::trigger().snapshot()` and `purge_log`), best effort and visible in the audit log. Followers purge when their own snapshot policy runs; they do not need a secret-specific trigger.
- Old ciphertext in a backup, in `sled` free pages or on a node that was offline is explicitly **not** erased, and the docs say so. Rotating to a new key makes a stolen old *log* useless only if the old key is also gone from every place it lived; the operator should treat "old backup plus old key" as holding the old secrets.

Rejected alternative: per-secret data keys with crypto-shredding (delete the data key to erase the ciphertext). It buys real erasure at the price of a second replicated key table and its own compaction story, for a handful of secrets per cluster. Revisit if plugins ever hold many secrets.

## Alternatives considered

- **Per-node secrets (not replicated).** The operator would enter each secret on every node, and a failover would silently lose them. Rejected: replication with ciphertext gives the same confidentiality with working HA.
- **Key derived from the HA token.** Couples two credentials with different lifecycles, and the token already travels the peer protocol. Rejected.
- **An external KMS or Vault.** Likely later as another key source behind the same keyring interface; the file or environment source ships first.
- **Plaintext secrets in a sealed file next to the database.** Not replicable, no AAD binding, and not safer than the keyring.

## Build plan (follows this review)

Slices, in order, each its own PR; none of them ships a secret-expanding plugin until the last:

1. Keyring loader, AEAD wrapper with AAD, `zeroize`/redacting types, key ids, `--plugin-secret-key-file` (standalone controller, no HA).
2. Secrets store trees, admin API and audit lines on the standalone controller; install deletion removes secrets.
3. Replicated entries, receiving-node encryption, forwarding of ciphertext, members view, held/pending status, rotation endpoint and log purge request.
4. `http` import expansion, redaction and per-hop binding checks (with the `http` capability slice, #220).
5. UI: slot list with set/not set and replace, key id display, held status.

Tests the gate requires before slice 4 merges:

- The AAD binding: a ciphertext moved to another slot or install fails to decrypt.
- **A raw-bytes scan**: after setting a secret with a sentinel value, neither the Raft log tree, a built snapshot, the `sled` directory nor any API response contains the sentinel or its base64 form.
- A follower-received `PUT` produces a log entry holding only ciphertext.
- A node without the key holds the plugin and the other plugins still run; a leader without the key raises the alert.
- Rotation: add key, rewrap, retire key, old ciphertexts read before and after; retiring early yields `held`, not a crash.
- Redaction: a destination that echoes the header gives the guest `[redacted]`; a redirect to an approved host that is not bound to the slot fails the call and never receives the secret.
- Request bodies of the secrets routes do not appear in logs at `trace` level.

## Open items (not blocking this review)

- Whether the secrets routes should require a stronger credential than the admin bearer token. They use the same token as the rest of the admin API until the project has a second credential class.
- A "reveal once at creation" flow for plugin-generated secrets is not offered; if a plugin ever needs to create credentials, that is a new capability with its own review.
