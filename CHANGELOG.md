# Changelog — k9o (Rust crate)

## 1.5.0 (unreleased)

### Added

- `approvals().comments()`: `add(pending_id, AddApprovalCommentInput { body, visibility })` (`POST /@<slug>/pending-approvals/<id>/comments`) and `list(pending_id, ListApprovalCommentsOptions { limit, cursor })` (`GET` on the same path, oldest first, `ApprovalCommentPage { comments, next_cursor }`), both returning `ApprovalComment` (`id`, `case_id`, `author_id`, `visibility`, `body`, `created_at`). `body` is 1 to 4000 characters and `visibility` is `everyone` or `approvers_only`; a bad value is refused with `K9O_INVALID_FIELD_VALUE` before sending. Commenting needs a session. 403 `K9O_APPROVAL_COMMENT_NOT_ELIGIBLE` and 409 `K9O_APPROVAL_COMMENT_CLOSED` come back as the `KError`.
- `realtime().replay(topic, ReplayOptions)`: `POST /@<slug>/realtime/replay` for any realtime topic. `topic` is required; `from_sequence`, `until_sequence`, `limit` (1 to 10000) and `after_audit_event_id` are optional. It returns a `RealtimeReplay` (`events` as `RealtimeEvent`, `next_cursor`, `has_more`, `resync`). `RealtimeEvent` fields other than `payload` now default when a frame omits them. `audit().replay` is unchanged.

### Fixed

- `realtime().subscribe`, `metadata().subscribe` and `rollups().subscribe` no longer reconnect in a loop without delivering anything. The gateway opens a connection that sent no `Last-Event-ID` with an informational `resync` frame (the live head), then streams events on the same connection. That opening `resync` is now ignored: the stream continues, `on_resync` is not called and there is no reconnect. A `resync` after an event was delivered, or on a connection that sent a `Last-Event-ID` or a `fromSequence` start position, is still a lost position and calls `on_resync` as before.

### Changed

- Audit tail and `audit().replay` decode the gateway's real frames. The five fields are read from the frame's `payload` (not from the top of the data object), duplicates are dropped by the frame's `eventId` and by `auditEventId`, and a reconnect resumes with the frame's SSE `id` (which is not the audit event id). `AuditTailEvent` gains `event_id` and `sequence`; `outcome` is `Option<String>` (the gateway sends null). `AuditReplay` is a page: `events`, `next_cursor`, `has_more`. A `replay-ledger` resync follows the replay pages while `has_more`; it now reconnects fresh first and replays while that connection's frames are buffered, so the catch-up is gapless and each event is delivered once, a new connection starts at the live head and may re-send the last few seconds (the informational head `resync` on a fresh connection is ignored), and `audit().replay` sends `limit: 1000` unless you pass one (1 to 10000).
- `rollups().subscribe` yields `RollupCell` (`event_id`, `cell_key`, `bucket`, `dims`, `is_final`, `measures`, and `status` / `reason` with `is_suppressed()` for a cohort-suppressed cell) read from the frame's `payload`; resume uses the frame's SSE `id`, repeats are dropped by `eventId`, and the `reread-source` resync reaches `on_resync`.
- `keys().rotate(key_id, RotateOptions)`: a `step_up` hook receives the challenge (`StepUpChallenge`) and the request's method and URL and returns a `StepUpProof { method, proof }`; the SDK retries once with the `x-k9o-step-up-proof` header. Without a hook the 401 `K9O_STEP_UP_REQUIRED` carries the challenge (`StepUpChallenge::from_error`). `KeyList.next_cursor` is removed: `keys().list` is not paged.
- `rampart().classify` refuses a non-object `input` before sending (`K9O_INVALID_FIELD_VALUE`).

### Added

- `KError.request_id` is now filled from the gateway's `x-k9o-request-id` response header on every non-2xx answer (requests, downloads and stream opens), when the error body carries no `requestId` of its own; a body value still wins. Log it and give it to support.
- `k9o-kibo-core` (the `kibo-core` directory): a separate crate with `KiboCore`, a ready `AppCore` over the Kibo core. It builds on the core's Rust crate, `kibo-kms-rest-ffi` 0.4.5 (Kibo core 2.16.5) or later. It converts the SDK's structs to the core's and gives the core's errors as `"<CODE>: <message>"`. It is a separate crate so a normal build of `k9o` does not resolve or compile the core.
- `gate().sign_in` first asks the project which region holds the email (`POST /@<slug>/gate/locate`, the email only, no credential), then sends the password to the region host it names, but only if that host is `https` (or `http` on a loopback base and a loopback host), has no userinfo, port, path, query or fragment, and is your own organization, gateway family and apex with the region code added or swapped in (the code must equal the answer's `region`); otherwise the base URL is used. The answer is not cached; `refresh_token` and `sign_out` use the host that issued the session (a bounded in-memory hint, removed on sign-out). A 429 `K9O_RATE_LIMITED` or 503 `K9O_GATE_REGION_UNAVAILABLE` from that step is returned as the `KError`, the password is not sent, and nothing is retried; a 2xx with a malformed body uses the base. Sign-in now makes two requests. If the project's locate step answers 404, the client uses the base address.
- `KiboClientCore` matches the configured key against the call's `expected_key_refs` before it fetches a federated token or exchanges a credential; a key outside the call's pin gets `K9O_CRYPTO_UNEXPECTED_KEY` on every item, with no token fetch and no seal or open.
- The custody check before each `encrypt` names the crypt (`GET /crypto/mode?crypt=<crypt>`), so a personal access token or workload policy scoped to named crypts can encrypt; `crypto().mode()` still sends no query.
- `crypto().mode_for(crypt)` asks the custody mode about one crypt (`?crypt=<crypt>`); a bad crypt is refused with `K9O_INVALID_FIELD_VALUE` before sending. `crypto().mode()` is unchanged.
- `crypto().encrypt` follows the `route` in the project's `GET /@<slug>/crypto/mode` answer (`CryptoMode.route`, kept as sent). For BYOK and HYOK only an explicit `"server"` sends plaintext to the platform (it needs a non-empty epoch, else `K9O_CRYPTO_CUSTODY_UNKNOWN`); a missing route or `"client"` uses your client core. A route that is not `"server"` or `"client"` is refused with `K9O_CRYPTO_CUSTODY_UNKNOWN` (context `custodyMode`, and `route` when it is a string) and nothing is sent, for every custody mode. The mode is read before every encrypt, never cached.
- `crypto().crypts()`: `list`, `create(name)` and `retire(name)` for the crypt registry (`GET` / `POST /@<slug>/crypto/crypts`, `POST .../crypts/<name>/retire`), returning `Crypt` (`name`, `status`, `created_at`, `retired_at`). A bad name is refused with `K9O_INVALID_FIELD_VALUE` (field `name`) before anything is sent; `create` sends only `name`; a malformed gateway answer is `K9O_INTERNAL_ERROR`. 409 `K9O_CRYPT_ALREADY_EXISTS`, 409 `K9O_CRYPT_LIMIT_EXCEEDED`, 409 `K9O_CRYPT_RETIRED`, 404 `K9O_CRYPT_NOT_FOUND` and access refusals come back as the `KError`. Retiring only makes `encrypt` refuse the name; `decrypt` and `rewrap` never read the registry. Needs `crypto:manage_crypts` (PROJECT_ADMIN by default); a personal access token is refused on retire.
- `KiboClientCore`: a client core for `crypto()` on a BYOK or HYOK project that opens and seals on this machine against your own KMS or HSM. Sequences the federated token, the credential exchange on your cloud and the seal or open, over the new `AppCore` trait (the crate does not depend on the Kibo core). `FederationSpec::Vault` takes `jwt_auth_mount`, the JWT auth mount (`jwt` for `auth/jwt`, not the transit mount). Requires a non-empty `expected_key_refs` containing the configured key; checks the core declares `expected_key_refs` before any token is fetched (`K9O_CRYPTO_KEY_PIN_UNENFORCED`); key, provider, region, role and endpoint come only from your configuration; a token response naming another provider or project is refused; the exchange has a deadline (default 30 s, `KIBO_EXCHANGE_TIMEOUT`); the credential is cached per crypt, operation and subject until 60 s before expiry; one result or code per item, with `UPSTREAM_KMS_ERROR` meaning your own KMS failed. `CryptoFederatedToken` gains optional `provider` and `project_id`.
- `crypto().federated_token(crypt, op)` asks for a short-lived token (15 minutes at most) that your own cloud KMS or HSM can trust (`POST /@<slug>/crypto/federated-token`) and returns `CryptoFederatedToken` (`token`, `expires_at`, `issuer`, `subject`, `audience`, `kid`). The crypt and `op` (`encrypt`, `decrypt`, `rewrap`) are checked before sending; the call is never retried, and `Debug` redacts the token.
- `crypto()`: `encrypt`, `decrypt` and `rewrap` for server-side encryption under the project's platform-managed key (`POST /crypto/encrypt | decrypt | rewrap`). Checks the crypt (a registered name, not `actions`), the item count and the sizes before sending; one result or code per item, in order; never retried. For service tokens and personal access tokens only (extensions, end users and mobile cannot call it; IAM on the crypt decides, default floor PROJECT_EDITOR). Custody-aware: `encrypt` checks the project's custody mode on every call (`crypto().mode()`, never cached) and sends the `custodyEpoch`; BYOK and HYOK keys go to a registered `ClientCore` (`crypto().use_client_core`), and without one the call raises `K9O_ZK_CLIENT_CORE_UNAVAILABLE` and sends no plaintext; an unknown or unreadable mode fails closed. `decrypt` and `rewrap` try the platform first and reroute client-side on 409 `K9O_ZK_STORE_FLOW_REQUIRED` with route client. The ciphertext follows the project key's lifecycle only, not per-subject erasure; it is a different construction from client-side Kibo and the extension route. `encrypt` can answer 404 `K9O_CRYPT_NOT_FOUND` (name not registered) or 409 `K9O_CRYPT_RETIRED` (name retired; `decrypt` and `rewrap` are not refused for either); both pass through as `KError` (constants `CODE_CRYPT_NOT_FOUND`, `CODE_CRYPT_RETIRED`). Not live on every gateway yet (a 404 on `/crypto/*` is expected).
- `ClientOpts.expected_key_refs` (`Option<Vec<String>>`) and `key_ref_matches(KeyRefProvider, key_ref, &expected)`: for BYOK and HYOK, set the keys you expect from your own cloud configuration. `crypto()` passes them to the registered `ClientCore` on every call in a new `CoreOptions` argument (`ClientCore::encrypt`, `decrypt` and `rewrap` gain `options: &CoreOptions<'_>`), and the core refuses any other key (version ignored) before it uses a token or makes a network call. A core declares that it does with `ClientCore::enforces_expected_key_refs` returning `true` (default `false`); with the option set and no declaration the call is refused with `K9O_CRYPTO_KEY_PIN_UNENFORCED` before the core is called. The helper compares by the provider's rule (AWS whole reference; GCP without the version; Azure the first five parts in lower case; Vault `<path>/<key>` without `:<version>`); the provider (`Aws`, `Gcp`, `Azure` or `Vault`) is required and never read from the reference. `Client::new` rejects an empty list, or an entry that is blank, has whitespace or contains `*`, with `K9O_INVALID_FIELD_VALUE`.
- `KError.retry_after_sec` (`Option<f64>`): the `retryAfterSec` number from the gateway's `recommendation` object when it is finite and not negative. It is informational; retry behaviour is unchanged.
  `KError` has public fields, so code that builds it as a struct literal must add `retry_after_sec: None`. `KError::new` is unchanged.
- `approvals().list` pages: `ListApprovalsOptions` gains `cursor`, and `meta.cursor.next` of the answer is the cursor of the next page (`None` on the last page). An invalid cursor fails with `K9O_INVALID_CURSOR` (HTTP 400). Code that builds `ListApprovalsOptions` as a struct literal adds `..Default::default()`.
- `approvals().bulk` reports `failed`: `BulkApprovalsResult.failed` lists the rows that matched but could not be decided (`BulkApprovalFailure`: `id`, `code`, `reason`); it is empty when none failed. The call answers HTTP 200 even when rows fail.
- `require_approval()` on single-record writes: `from(c).insert_builder(&record)`, `update(..).eq("id", ..)` and `delete().eq("id", ..)` queue the write for approval (HTTP 202) instead of applying it.
  `execute` returns a `WriteOutcome`: `Queued` with the `QueuedChange`, or `Applied` with the normal result when an `auto_approve` policy applies the write at once. `insert` is unchanged; upserts and bulk writes do not take the modifier.
- `functions().triggers()`: `list`, `create`, `update` and `delete` for a function's event triggers (`collection`, `events`, `filter`, `enabled`, `log_ok_invocations`). `delete` answers 204, so `data` is `()`.
  The SDK checks `events` (1 to 4 of `insert`, `update`, `delete`, `upsert`) and the collection name before sending.
- `schedules()`: `list`, `get`, `create`, `update`, `delete`, `runs` and `run_now` for scheduled triggers. `list` takes `status`, `function_name`, `limit` and `cursor`.
  An interval under 5 minutes, or `catchup_policy` `run_all`, can be refused with 403 `K9O_TIER_FEATURE_GATED`.
- `end_users()`: `list` and `get` for the project's end users, and `role_grants()` with `list`, `grant` and `revoke` for the roles they hold.
  `role` is any string; `list` returns the grants as a `Vec`; revoking an unknown or already revoked grant answers `revoked: false` rather than an error.
- `vectors()`: `nearest(collection, &query_vector, NearestOptions { top_k, min_score, metric, filters })` and `upsert(collection, &rows)` for collections of your own embeddings.
  The vector is sent in the request body (up to 16000 numbers, 1 to 1000 rows per upsert), for a collection of your own embeddings. A collection name must start with `_`.
  The SDK checks the name, the vector length and numbers, `top_k`, `metric`, the filter values and the row count before sending.
- `metadata()`: `filter`, `set`, `search`, `vectors().nearest`, `subscribe`, and `index().get` / `index().declare` for the Metadata API.
  Column names are sent as `metadata.<name>` (the prefix is added when missing); the SDK checks surfaces, operators, limits, the 64-column declaration, `metric`, the embedding and cursor use before sending.
  `filter` pages with `meta.cursor.next`. `search` has no cursor input, `nearest` returns no score and may return at most 50 rows, and `subscribe` events carry only `{ collection, id }`.
  `subscribe` is the change feed of `realtime().subscribe`: it reconnects with `Last-Event-ID`, drops duplicates and supports `on_resync`; `from_sequence` applies to the first connection only.
  Expect `K9O_METADATA_FIELD_NOT_INDEXED`, `K9O_UNKNOWN_SURFACE`, `K9O_QUERY_RESULT_TOO_LARGE` (413), `K9O_NOT_IMPLEMENTED` (501 for `storage:` surfaces), `K9O_ENCRYPTED_COLUMN_OPERATION_UNSUPPORTED` and `K9O_EXTENSION_PERMISSION_DENIED`.
- `custody().status()`: the project's custody mode, attached key provider, last custody job and, for a hardware module, its vendor, cluster and region (`CustodyStatus`). Read-only.
- `rampart()`: `classify(&input, label)`, `jwks()` and `verify_receipt(&verdict or receipt, &input, &VerifyOptions)`. `classify` returns the verdict, the signed receipt exactly as received and the `project_id`; Rampart is advisory and a classifier outage is an error (503 `K9O_RAMPART_CLASSIFIER_UNAVAILABLE`), never a pass. A long input is scored in overlapping windows and the highest injection score decides; past 16 windows (about 6,000 tokens) the answer is 413 `K9O_RAMPART_INPUT_TOO_LONG` with no verdict. 503 `K9O_RAMPART_CLASSIFIER_UNAVAILABLE` still means the classifier could not answer. The thresholds (calibration `rampart-launch-v1`) are a starting calibration and may move; a change is a new `calibration_version`.
  `verify_receipt` fetches the key feed (`GET /rampart/jwks`) itself, caches it for the life of the client and refetches once on an unknown `kid`. It checks `kid`, `alg`, `typ`, the signature, `iss`, `project_id`, the `iat`/`exp` window and the input digest, and refuses with a `ReceiptRefusal` (`malformed`, `key_unknown`, `alg`, `typ`, `signature`, `issuer`, `project`, `project_unknown`, `expired`, `not_yet_valid`, `input_hash`, `key_feed`).
  The expected issuer is the `issuer` the key feed advertises (`jwks()` returns it next to `keys`; the `issuer` option overrides it); a feed with no issuer and no option is refused as `issuer`. The expected project is the `project_id` option, else the project id of the `Classification` passed; a bare receipt string with neither is refused as `project_unknown`.
  Ed25519, SHA-256, SHA-512 and the platform's canonical JSON form (`jcs_canonicalize`: keys sorted by UTF-8 byte order, integers only up to 2^53 - 1, so an input with a fractional number cannot be classified or verified) are implemented in the crate; no dependency was added.
- `analytics()` and `rollups()`: datasets. `analytics()` has `list`, `query`, `create`, `get`, `delete`, `materialize`, `read`, `write`, `upload` and `download_original` (the stored original file as bytes), plus `rollups()` and `schedules()` with `create`, `list`, `delete` and `reattest`; `rollups()` has `list`, `read` and `subscribe` (a feed of closed cells with the change feed's reconnect, duplicate drop and `on_resync`).
  `reattest` is for a person only (a service, workload or extension token gets 403 `K9O_RBAC_DENY`, `reason` `human_only`); a rollup or schedule that is not re-attested within its 7 to 365 days pauses.
- `keys()`: `list` (`KeyStatusFilter::All` adds rotating and revoked versions), `get` and `rotate` for the project's encryption keys. Metadata only. `rotate` needs project admin and a step-up proof (401 `K9O_STEP_UP_REQUIRED` without one), is limited to 3 per rolling 24 hours (429 `K9O_RATE_LIMITED`) and answers 409 `K9O_STATE_CONFLICT` while another rotation runs.
- `audit().tail(AuditTailOptions)`: the live audit feed (`audit.project.*`, or one `audit.project.<type>`) with `types`, `on_resync`, `reconnect_delay` and `idle_timeout`. Events carry `audit_event_id`, `event_type`, `actor_type`, `occurred_at` and `outcome`. A `replay-ledger` resync is caught up through the replay endpoint by the tail itself. `audit().replay(AuditReplayOptions)` wraps `POST /@<slug>/realtime/replay`.
- `VerifiedReceipt.windows`: the receipt's `windows` claim, the number of windows scored (1 when the input fits one window, and for a receipt without the claim). Code that builds `VerifiedReceipt` as a struct literal adds `windows`. Unknown claims are ignored.
- 413 `K9O_RAMPART_INPUT_TOO_LONG` from `rampart().classify` is a typed `KError`, never a verdict.
- `KError.retry_after_sec` is also read from a `Retry-After` response header when the body gives none.
- `Moat` (local, no network, no client): pins the system prompt, refuses role and system markers in untrusted input, enforces an input and an output JSON Schema subset, runs an optional PII filter and gives `prompt_sha256()`. Refusals are `MoatRefusal` values (`role_marker`, `input_schema`, `output_schema`, `pii`).
  Moat finds role markers on a normalised copy of the text (NFKC, combining marks ignored, ignorable characters removed, a small set of Cyrillic and Greek look-alikes of the marker letters read as Latin, case-folded, horizontal whitespace collapsed; not general homoglyph detection), line by line; adds the `unicode-normalization` 0.1.26 dependency.
- `vector_search` on the select builder sends the embedding in the request body.
- `Meta.approval` (`MetaApproval`): the approval status the gateway reports on a queued write.
- README sections for writes that wait for approval, function triggers, schedules, vector search and end-user role grants.
- Realtime change feed resumes by itself: `realtime().subscribe` reconnects with `Last-Event-ID` after the server closes the stream (at most 300 seconds) or the network fails, drops events it already delivered by `event_id`, ignores `:hb` heartbeats, abandons a connection that is silent for 45 seconds, and stops on `close()`. A 4xx answer on connect is returned once; a 5xx answer is retried with a capped backoff (up to 30 seconds).
- `RealtimeEvent.event_id`. Code that builds `RealtimeEvent` as a struct literal adds `event_id`.
- `realtime().subscribe_with(topic, SubscribeOptions)` with `filter`, `on_resync` (async, receives `Resync { reason, action }`), `reconnect_delay` and `idle_timeout`. A `resync` frame is no longer returned as an event: `on_resync` runs and the subscription continues on a fresh connection without `Last-Event-ID`.
- `Subscription::close()`.
- README: what an event-trigger function receives (the JSON payload, redaction, `triggeredAt`, and deduping on `eventId`).

### Fixed

- A gateway error whose `recommendation` is an object, for example `{"action": "..."}` or `{"retryAfterSec": 30, "note": "..."}`, is no longer turned into `K9O_INTERNAL_ERROR` with no status. The error keeps its `code`, `message`, `status` and `context`. `recommendation` stays `Option<String>`: a string is kept, an object gives its `action` text, else its `note`, and any other JSON type gives `None`.
- `compute().llms().list` and `read` no longer fail with "missing field tierGate". `LlmModel` now matches what the gateway sends: `id`, `provider`, `provider_class`, `modality`, `min_tier` and `invocable` are required; `reason`, `price_in`, `price_out` and `price_unit` are optional. The fields the gateway never sent (`tier_gate`, `context_window`, `capabilities`, `cost_per_million_input_tokens`, `cost_per_million_output_tokens`) are removed.

## 1.4.0 — 2026-10-26

### Added

- `query`: run one read-only SQL statement with positional or `:name` parameters.
- `search`: full-text search over one column of a collection.
- `text_search` and `vector_search` on the select builder, for full-text and nearest-neighbour search.
- `ai().generate`: run a prompt through an LLM, with an optional `model`. Same result as `compute().llms().invoke`.
- `streams()`: `create`, `list`, `get`, `delete`, `publish`, `replay` and `subscribe` for user data streams. `subscribe` reconnects from the last sequence.
- `approvals()`: `list`, `get`, `approve`, `reject` and `bulk` for the pending-change queue.
- `approval_policies()`: `list`, `create` and `delete` for the rules that decide which writes are queued.
- `schema().collections().list`: the project's collections by name, with cursor paging.

### Changed

- README description.
- Function deploy examples use the sandbox tier.

## 1.3.0 — 2026-10-26

Every method now matches the gateway and is checked by a contract test.

### Changed

- `projects().create` takes `display_name` (required) and optional `slug`, `region`, `custody_mode`. `get` and `update` use the projects routes.
- `functions().deploy` takes `name`, `runtime`, `tier` (`gateway` or `sandbox`), `artifact_key`, `version` and `description`. Inline code is gone.
- `functions().invoke` sends `{payload}` and accepts `InvokeOptions`. `functions().list` accepts `limit`, `cursor`, `status`, `source`, `tier`.
- Policies `create`, `update` and `validate` take a `policy` document. `policies().list` takes only `status`.
- `audit().query` filters are `event_type`, `actor_id`, `after`, `before`, `limit`, `cursor`.
- Role grants use `/role-grants` and identity providers use `/identity-providers`, with typed create and update inputs.
- `default_llm_get` and `default_llm_set` return records. `smtp().set` takes `password_ref` and `from_address`; `test` takes `to` and `subject`.
- `api_tokens().create` takes `label`, `scope_manifest` and `ttl_seconds`. `sessions().revoke` and `revoke_all` use the session routes.
- `compute().llms().invoke` parameters are flat and the result has `content` and `usage`. Stream chunks are OpenAI-style.
- `workflows().trigger` sends `{triggerContext}`. `gate().sign_out` requires the refresh token.
- `kibo().vault().secrets().get` looks up a secret by exact name; the region is optional.
- The API host is read from the base URL: any `<org>.api.<apex>` host works and no domain is built in.

### Removed

- `gate().verify_session`, `gate().hosted`, `realtime().replay` and the polling transport.
- `me().notifications`, `me().audit_log`, `me().data_export`.
- `org().groups().create`, `org().groups().delete`, `org().members().get`, `org().smtp().get`.
- The `modality` filter of `compute().llms().list` and the `triggerType` filter of `workflows().list`.

### Added

- `functions().upload_artifact(bytes, "wasm" | "tar")` returns the `artifact_key` for `deploy`.

## 1.2.0 — 2026-10-25

- The README now introduces the Kilimanjaro building blocks (Keep, Pulse, Gate, Kibo). No change to the public API.

## 1.1.1 — 2026-10-25

- Documentation links corrected: the documentation hub is https://docs.kilimanjaro.dev, with the website, platform API and console listed in a Links section. Package metadata points at the same places.

## 1.1.0 — 2026-10-25

- Documentation rewritten with runnable examples and expected output; internal cross-references removed from the published sources. No change to the public API.
- Error text for `update()` / `delete()` without an `id` filter no longer mentions a version or a roadmap.

## 1.0.0 — 2026-10-23

First public release. See the README for the full API overview.
