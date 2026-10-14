# Live deployment

Live hosting is optional. No infrastructure, credential, or remote relay is needed for ordinary local BMUX operation. The HTTP compatibility boundary is [protocol v1](live-broadcast-protocol.md).

## Guided deployment from an installed binary

```sh
bmux broadcast deploy
bmux broadcast deploy --profile staging --setup
bmux broadcast deploy --profile staging --doctor
bmux broadcast deploy --profile staging --plan
bmux broadcast deploy --profile staging --review
bmux broadcast deploy --profile staging --apply OPERATION_ID
```

The no-argument wizard guides setup, optional read-only account/zone discovery,
resource import review, planning, and explicit approval of the exact operation
before apply. Deployment profiles are separate from publishing profiles created
by `bmux broadcast setup`. Existing accounts, registrar delegation, credentials,
and billing/SQLite Durable Object entitlement remain external prerequisites.

The installed binary embeds seven exact installer/infrastructure resources and
extracts them into a private, verified, content-addressed cache. No sidecar,
repository checkout, xtask changes, or implicit frontend build is required.
Python 3.12 or newer is selected from bounded absolute PATH entries, or explicitly
with `--python /absolute/path/to/python3`; its isolated version probe times out
after five seconds. OpenTofu 1.10.9, AWS CLI, and Node 22 are deployment tools;
the immutable candidate contains pinned Wrangler and provider locks.

Candidate authority, highest priority first:

1. Explicit `--artifact /absolute/immutable/candidate`; a missing path fails
   closed. Combining this with release URL/digest flags is rejected.
2. Explicit or saved approved `--candidate-url https://…` and independently
   trusted `--candidate-sha256 SHA256`. The wizard acquires and verifies this
   archive; it does not trust a digest obtained from an unsigned mutable response.
3. Optional `live-candidate` beside the selected executable, never the working
   directory or extracted installer cache.

`--doctor` uses the same authority without downloading or applying. Missing
candidates report the exact selected path and packaged-release setup remediation.
Candidate production remains the explicit CI/operator action
`scripts/build-live-artifact.sh`; it is not part of ordinary CLI builds.

Advanced workflows support `--import ADDRESS=ID --approve-import`, separate
`--bootstrap --accept-cost` plan/review/apply, and explicit
`--state-role ROLE --approve-policy-attachment`. Review the exact proposed
imports and cost before approval. Resume is bound to the artifact, plan hash,
profile, AWS account, and operation ID. Interrupted apply requires read-only
`--reconcile OPERATION_ID` or explicit `--abandon OPERATION_ID` and a fresh plan;
it never blindly repeats a mutation. Ambiguous Pages upload acknowledgement is
separate (`--ack-pages OPERATION_ID`). Profiles and operation ledgers contain no
reusable credentials; provide credentials through protected prompts/environment.

HTTP protocol v1 negotiates renderer `bmwp-3` and `BMWP\x03` frames, not a new
HTTP protocol version. See the [coordinated upgrade contract](live-broadcast-protocol.md#coordinated-renderer-upgrade).
Offline tests and built-executable help/doctor checks do not establish real
provider import, billing, Durable Object migration, DNS/TLS, or cloud deployment
acceptance. Those require separately authorized staging operations.

## Ownership and topology

`infra/live-terraform/main.tf` owns, per environment:

* a direct-upload Pages project and custom domain;
* its proxied CNAME DNS record;
* one Worker, immutable Worker version, 100% deployment, and only `<hostname>/api/live/*` route;
* SQLite Durable Object migration `v1`, class `LiveChannel`, binding `LIVE_CHANNELS`;
* channel `main` and **publisher SHA-256 digest only**.

Worker entry is `infra/live-relay/src/worker.js`; all `.js` modules below `src` are shipped. No Wrangler Worker deploy is allowed: OpenTofu exclusively owns Worker code, bindings, migrations and routes. Pinned Wrangler is used **only for Pages static uploads**. Worker observability/logpush/previews are disabled to avoid content or credentials entering logs. Future DO migrations require a new explicit tag and reviewed migration; never rename/delete the authoritative class casually.

Staging and production must have different Pages projects, hostnames, Workers, publisher credentials and state identities. `environment` accepts only `staging|production`; state keys are `bmux/live/<environment>/terraform.tfstate`, with matching `.tflock` files. Use separate buckets too (bootstrap below).

## External prerequisites and first bootstrap

These commands are operator actions, **not performed by the implementation agent**:

1. A Cloudflare account with Workers and SQLite Durable Objects enabled; an active Cloudflare-managed zone delegated at your registrar. Zone/account identity and billing acceptance cannot be invented offline.
2. AWS account/region and a separate bootstrap IAM identity authorized to create S3 buckets, their policies and scoped IAM policies. In a secure directory:

   ```sh
   tofu -chdir=infra/live-terraform/bootstrap init -lockfile=readonly
   tofu -chdir=infra/live-terraform/bootstrap plan \
     -var='region=us-east-1' -var='bucket_prefix=YOUR-GLOBALLY-UNIQUE-bmux-live-state' -out=bootstrap.plan
   tofu -chdir=infra/live-terraform/bootstrap apply bootstrap.plan
   ```

   This declares private, versioned, encrypted, TLS-only **separate staging/production AWS S3 buckets** and narrowly scoped state policies. Securely back up its local state; it is the bootstrap root, not either live state. Attach each output IAM policy to its own managed state identity; provide credentials privately. Access keys are deliberately not generated into OpenTofu state. S3 conditional locking is native OpenTofu `use_lockfile=true`; **R2/custom S3 endpoints are not supported** without independent conditional-Put proof. No credential in backend files.
3. Create GitHub environments named exactly `staging` and `production`. Permit only branch `master`, no tags/wildcards. Production requires reviewers, prevent self-review, and no administrator bypass. Environment protection availability depends on repository visibility/plan; do not weaken it to make deployment run. The bootstrap verifies these policies and never writes/replaces them.
4. Issue a Cloudflare token restricted to the target account with `Workers Scripts Write` and `Pages Write`, and the target zone with `Workers Routes Write` and `DNS Write`. Account permissions and zone permissions are separate policy resources, not interchangeable. No all-accounts/all-zones wildcard. Use separate environment tokens where feasible. The script exposes a tested `token_policy` helper for provisioning automation but intentionally does **not** create/revoke tokens remotely here.
5. Generate one publisher key per environment in a **private local file only**:

   ```sh
   umask 077
   python3 -c 'import secrets; print(secrets.token_urlsafe(48))' > /PRIVATE/PATH/publisher-key
   ```

   Keep publisher keys out of git, GitHub Actions and OpenTofu. Never reuse deployment or state credentials as publisher keys.
6. Supply each environment's values via private environment variables (`CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`, `CLOUDFLARE_ZONE_ID`, `LIVE_HOSTNAME`, `LIVE_PAGES_PROJECT`, `LIVE_STATE_BUCKET`, `LIVE_STATE_REGION`, `LIVE_STATE_ACCESS_KEY_ID`, `LIVE_STATE_SECRET_ACCESS_KEY`). Use a new staging project e.g. `bmux-live-staging`; for production `bmux` may already exist, see import below. Plan without requests:

   ```sh
   python3 scripts/bootstrap-live-secrets.py --repo BSteffaniak/bmux --environment staging --dry-run
   ```

   After explicit authorization, set missing secrets:

   ```sh
   python3 scripts/bootstrap-live-secrets.py --repo BSteffaniak/bmux --environment staging \
     --publisher-key-file /PRIVATE/PATH/staging-publisher-key
   # Repeat with --environment production and a separate key/values.
   ```

   Existing names are skipped without reading replacement values. Secret values pass to `gh secret set` on **stdin**, not argv, and all provider failures are redacted. Missing inputs are detected before any writes. Partial remote writes cannot be atomic; re-list and retry (skip-existing) after ambiguous failures. No credentials are logged. No gh calls occur in dry-run.

## Existing production import: no silent adoption

Before the first production promotion, inspect ownership with the existing infrastructure owner. Never run a new apply against a duplicate production Pages project/domain/DNS record. If already owned elsewhere, explicitly transfer ownership and remove the old owner first, or use an entirely new project/hostname. This is an architectural/operator decision, not automatic import.

With reviewed production backend configuration and `TF_VAR_*` set as in `scripts/live-deploy.sh`, initialize the remote backend, then explicitly import existing resources (replace IDs):

```sh
tofu -chdir=infra/live-terraform import cloudflare_pages_project.live ACCOUNT/PROJECT
tofu -chdir=infra/live-terraform import cloudflare_pages_domain.live ACCOUNT/PROJECT/HOSTNAME
tofu -chdir=infra/live-terraform import cloudflare_dns_record.live ZONE/RECORD_ID
```

Review the full plan for changes, including `production_branch=master`, before applying. `prevent_destroy` guards Pages, DNS/domain and Worker resources. The workflow cannot auto-import unknown ownership. A Git-integrated Pages project needs an explicit reviewed conversion/ownership plan; this configuration is direct upload, not a second Git deployment owner.

## Immutable build and protected promotion

`.github/workflows/live-promote.yml` performs an uncredentialed build using flake-owned Rust: offline Python tests, relay Node tests, clippy **before** nextest, machete, native release build, real WASM build, copy generated viewer files into `packages/docs-site/public/live-assets/` **before docs gen**, then packages native+viewer+relay+site+infrastructure+locked Pages tooling with `REVISION` and `SHA256SUMS` into one tar artifact. Actions, Node (22.16.0), OpenTofu (1.10.9), Cloudflare provider (5.17.0), AWS provider (5.100.0), and Wrangler (4.56.0) are pinned/locked. Bootstrap and live provider lockfiles are checked in.

Untrusted PR builds never receive deployment/state/publisher secrets. No `pull_request_target`. Only `refs/heads/master` may enter deploy jobs. Staging verifies checksums and applies locked infrastructure, uploads Pages, then checks public protocol/status/viewer/WASM. Protected production, dependent on staging success, downloads the **same artifact**, not a rebuild. Checksums and revision are checked again. Environment-specific config is expected; code/asset identity must stay identical.

`.github/workflows/docs-deploy.yml` now calls this one owner instead of independently overwriting Pages. **First deployment needs the above environment bootstrap; docs pushes now request production approval.** This migration intentionally trades immediate unprotected docs updates for consistent viewer/site/relay promotion. It is not a local BMUX startup dependency.

Mutations share workflow concurrency `bmux-live-promotion`, `cancel-in-progress: false`; do not manually cancel during apply. Backend locks have a five-minute acquisition timeout, never disabled. Wrangler deploy logs are suppressed. Public smoke does not use publisher credentials or mutate a channel. For full native/WASM visual evidence use the isolated local instructions in [live broadcasting](live-broadcasting.md); public smoke alone is not that proof.

## Recovery, rotation and deployment gates

* A failed apply may have partially committed provider state. Preserve authoritative remote state, inspect, and run a fresh locked plan; do not blindly reapply an old plan. No automatic rollback of DO state or deletion.
* A stuck lock requires proving the owning CI/process is dead, inspecting lock identity, backing up current state/version, and an explicitly approved operator unlock. Never use automatic force-unlock or turn locking off.
* To recover state, choose a known S3 object version, back up the current state and lock metadata, prove no mutation is active, and compare lineage/serial with actual provider resources before any explicit restoration. Do not restore stale state over active work. Bootstrap bucket deletion is guarded.
* Rotate deployment/state credentials with explicit `--rotate --only NAME` selections. Install replacement first, verify a protected staging/prod deployment, then manually revoke the prior token/identity when no in-flight job can use it. The script never revokes credentials automatically.
* Rotate publisher keys separately; install the new digest through protected promotion, coordinate local publisher restart/re-authentication, then retire the old private key. No plaintext key is a CI secret.

**Tested offline:** provider signature/lock acquisition, `init -backend=false`, `fmt`, `validate` for live and bootstrap modules; mocked GitHub bootstrap protections/skip/rotation/stdin/redaction/scopes; backend environment separation and mandatory locking config. **Not proven by offline validation:** actual S3 concurrent lock exclusion/recovery, Cloudflare SQLite migration first deploy/redeploy, DNS/TLS activation and `/api/live/*` precedence alongside Pages, protected GitHub approval behavior, and native/browser end-to-end publishing. These are explicit staging acceptance gates requiring separately authorized infrastructure/build/browser execution. Public smoke catches protocol/route/asset failures but cannot replace those gates.
