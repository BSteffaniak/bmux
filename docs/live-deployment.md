# Live production deployment

## Steady-state architecture

Pages serves the complete documentation site and content-addressed live viewer at
`bmux.dev`. One Rust `bmux-live-relay` process runs on Fly at `live.bmux.dev`.
Local BMUX operation does not depend on either service. Backend and site have
separate immutable artifacts, publishers, and production concurrency groups.

The one-time Worker-to-Fly cutover and app-taint repair workflows have been
retired. Their remote completion markers remain durable replay fences, not
infrastructure-state authorities. Do not delete or reset those records.

## Normal releases

`live-promote.yml` builds the backend candidate and complete site candidate from
the same revision without deployment credentials. `live-validation.yml` runs
independent offline and runtime acceptance checks; there is no migration bypass.
The release then previews the ordinary image-only infrastructure plan, obtains
`production-approval`, deploys the backend, checks backend health, and invokes the
sole Pages publisher with the same-run, same-SHA site candidate.

The approved apply replans under the normal OpenTofu lock and revalidates the
image-only policy; the preview is not an authorization to apply arbitrary drift.
Only the relay image may change during an ordinary release. Creation, deletion,
replacement, imports and changes to guest size, environment, mounts, ports or
region require separate reviewed configuration. Unexpected drift stops release.

Each candidate is inventory verified; archive checksums and revision must match
before deployment. Image transfer preserves the OCI digest. Deployment never
recompiles the backend or rebuilds the site with production secrets.

The `Production` environment supplies credentials; `production-approval` remains
the explicit approval gate. Infrastructure uses `bmux-production-infrastructure`
concurrency with cancellation disabled; Pages uses `bmux-production-pages`.
Do not add caller locks around reusable workflows that already own their lock.

`docs-deploy.yml` also supports independent site publication. Its
`publish_candidate` input selects the verified same-run artifact, not an artifact
from a hard-coded historical run. If only site publication fails, retry the site
publisher rather than repeating backend setup or recovery.

## Initial installation and explicit configuration

First installation is a different operation from normal release. The guided
`scripts/setup-live.sh` / `setup-live.py`, Fly setup, state provisioning and
bootstrap tools remain available. Read their help and inspect the proposed
resources and ownership before approval. `configure-production-infra.yml` invokes
an explicit non-destructive configure path with reviewed imports and optional
new-bucket bootstrap completion. Never use it to bypass an interrupted cutover.

Configure may create approved known infrastructure; it does not authorize
resource deletion or replacement. The Fly app stage precedes registry upload.
Adoption requires exact independently reviewed provider IDs; list failures are
not an empty installation. Bootstrap receipt/admission is only for a genuinely
new installation, not an existing production backend.

## Credentials and ownership

Inspect `infra/live-terraform/*.tf` and the workflow environment bindings for the
current resource and credential contract. Use scoped Cloudflare/Fly deployment
credentials, R2 state credentials and the independently backed-up OpenTofu state
encryption passphrase. Publisher and metrics keys are separate; infrastructure
receives only their SHA-256 digests. Do not publish credentials, plaintext state,
private plans, terminal content or provider response bodies.

Infrastructure owns the relay app, machine, encrypted authority volume,
networking, certificates/DNS, selected Pages resources and state retention.
Pages uploads are performed only by the site publisher. State-history retention
is permanent infrastructure, not a temporary cutover override.

## Canonical state and recovery

The existing encrypted R2 state bucket/key remain the sole infrastructure-state
authority. `live-state.py`, `archive-live-state.py`, `live-state-recovery.py` and
OpenTofu encryption remain in place. Preserve signed conditional writes,
OpenTofu lockfiles, encryption, immutable history retention and non-cancelling
production concurrency. Do not substitute local state or change backend identity.

Normal deployment performs a read-only `release-check` against canonical state
and any retained cutover marker. Applying, malformed or changing markers stop
release. Absent markers are allowed for new non-migration installations with
valid canonical state. Completed markers are read but never changed. Checks do
not grant permission to resume or replay a migration.

Encrypted remote bytes are archived before apply and after each successful
stage. Preview and infrastructure checks do not archive, upload images or apply.
`archive-live-state.py` retains `check`, `release-check`, `archive`, `list`, and
`restore`, plus first-install admission operations. Restore requires exact
archive/current ETags, verifies encrypted content and preserves the prior state
under a cooperating lock. Unknown/corrupt/missing state fails closed. Never
force-unlock, overwrite state or automatically retry an ambiguous durable write.

An incomplete historical cutover needs an explicitly reviewed recovery design
based on canonical state, history and retained fences. It is not a reason to
restore deleted one-shot workflows or rebootstrap production blindly.

## Validation and operational checks

Read `AGENTS.md` for required validation, including clippy before full nextest
and dependency checks for code changes. `live-validation.yml` retains complete
browser/native acceptance independently of deployment approvals and credentials.

Public read-only smoke:

```sh
python3 scripts/smoke-live-broadcast.py --url https://bmux.dev --relay-url https://live.bmux.dev
```

This checks protocol identity and immutable site bundle integrity. It does not
prove visual rendering, historical error-free operation or volume durability.
`live-smoke.yml` remains the public smoke workflow. Private `/metrics` requires
its separate bearer credential; monitoring is not provisioned by deployment.

## Deployment verification

Protected configuration run `39168519882` completed backend deployment and public
read-only smoke successfully. Configuration mode intentionally skips the static
site publisher. Verify the next approved ordinary backend-plus-site release;
backend health alone does not establish end-to-end browser rendering.
