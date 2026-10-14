# Live production deployment

BMUX uses the existing `bmux.dev` Cloudflare zone. Production follows the PWMTF
pattern: one local credential provisioner, a separate infrastructure workflow,
and immutable CI releases with a `production-approval` gate. No staging account,
hostname, or deployment is required. Ordinary local BMUX operation needs none
of this infrastructure.

## One-time setup

### 1. Authenticate and protect GitHub environments

Authenticate `gh` for `BSteffaniak/bmux` and use your existing AWS SSO/bootstrap
identity locally. Never use AWS root credentials. Supply
`CLOUDFLARE_PROVISION_TOKEN` through a credential manager or protected environment,
not command arguments. This is a **provisioning** token with API Tokens Read/Edit,
read access to `bmux.dev`, and authority to grant the required account/zone
permissions. It is never installed in GitHub.

Create these repository environments, restricted to branch `master` only:

* `Production`: deployment secrets; no required reviewers here (the build/deploy
  job needs the credentials after the separate gate).
* `production-approval`: configure a required reviewer, following PWMTF. The
  repository owner may approve their own run; self-review need not be disabled.
  Disable administrator bypass for this gate.

The provisioner verifies protections and never silently replaces them. Existing
credentials are skipped before provider access; replacement requires `--rotate`
and can be limited with `--only NAME`.

### 2. Run the provisioner once

Create private directories for the local publisher key and bootstrap ledger.
Replace the public AWS account/bucket choices below with your account and a
unique bucket name; reuse an existing selected backend instead of creating a
competing one.

```sh
python3 scripts/bootstrap-live-secrets.py --repo BSteffaniak/bmux \
  --publisher-key-file /ABSOLUTE/PRIVATE/publisher-key \
  --bootstrap-state --configure-oidc \
  --state-account-id YOUR_12_DIGIT_AWS_ACCOUNT \
  --state-region us-east-1 \
  --state-bucket YOUR_UNIQUE_PRODUCTION_STATE_BUCKET \
  --state-role bmux-live-production \
  --state-dir /ABSOLUTE/PRIVATE/bmux-state-bootstrap
```

The provisioner discovers the existing zone/account, defaults to hostname
`bmux.dev` and Pages project `bmux`, generates a publisher key only at the explicit
private path if absent, and installs **only its digest** in GitHub. It creates a
BMUX-specific Cloudflare deployment token with separately scoped account
Workers/Pages permissions and zone DNS/routes permissions. PWMTF's project token
is not reused. Token values go to GitHub on stdin, never in logs or argv. After
rotation, verify deployment before manually retiring old credentials.

The optional AWS companion guides a pinned OpenTofu plan, review, explicit cost
acceptance and exact operation-ID approval. It provisions a production-only
private/versioned/encrypted/TLS-only S3 backend and scoped GitHub OIDC role, then
installs public backend/role metadata automatically. **No long-lived AWS access
keys are needed in the normal path.** Retain the private local bootstrap state;
it is the root of ownership, not disposable cache. Existing resource imports
require explicit approval. Planning and ambiguous apply recovery never blindly
replay a mutation.

`--configure-oidc` explicitly approves a repository-wide subject-template change;
review other OIDC consumers first. The template is `[repo,context,ref]`, so AWS
trust matches exactly
`repo:BSteffaniak/bmux:environment:Production:ref:refs/heads/master`, audience
`sts.amazonaws.com`. An incompatible existing custom template is rejected, not
overwritten. Production branch restrictions and the separate approval gate
remain required.

Use `--dry-run` to see intent with **zero GitHub, provider, or child provisioning
calls**. Advanced noninteractive backend operation uses `--state-mode
plan|review|apply|reconcile` and `--state-operation ID`; only complete backend
results are eligible for installing secrets. `--input-env` retains explicit
legacy supplied-value/static-key support. See `--help` for selected-secret and
recovery controls. Local setup needs Python 3.12+, AWS CLI and OpenTofu 1.10.9;
CI installs its pinned tools.

### 3. Configure infrastructure in GitHub Actions

After the changes are on `master`, run **Configure Production Infrastructure**.
CI builds and verifies an immutable candidate, waits for `production-approval`,
assumes the exact production OIDC role, and configures infrastructure under the
production backend lock.

For existing Pages/domain/DNS/route resources, the first discovery reports a
validated public `imports` map and fingerprint, then stops without adoption.
Confirm those exact resources are no longer managed by another state/automation;
rerun with the exact JSON map in `reviewed_imports` and `approve_import=true`.
The approval attests ownership transfer. Unexpected Worker/version/Durable Object
ownership or ambiguous matches require reconciliation; names alone never prove
ownership. Do not delete/recreate the existing website to make deployment pass.

The workflow owns the Pages project/domain, proxied DNS record, Worker,
immutable Worker version/deployment, SQLite Durable Object migration/binding,
and exact `bmux.dev/api/live/*` route. OpenTofu is the only Worker owner; pinned
Wrangler is used only for Pages static uploads. All approved infrastructure
changes and deployment uploads share the same non-cancelling CI concurrency
lock. S3 conditional state locking is always enabled; R2/custom S3 endpoints are
not interchangeable without independent locking proof.

## Subsequent releases

Docs pushes on `master` call **Live production release**, or dispatch that
workflow manually. CI builds once without deployment secrets, verifies the
artifact checksum and revision, waits for production approval, updates Worker
code/version deployment and uploads the matching website/viewer assets. It
**rejects infrastructure creation, imports, DNS/project changes, and destructive
changes**; run Configure Production Infrastructure for deliberate topology
changes. Immutable Worker-version replacement is the expected release exception.
No local frontend build, candidate upload, or staging promotion is required.

The normal native installer also supports `bmux broadcast deploy` for separately
hosted installations; that is not the normal bmux.dev CI deployment path. Seven
installer/provider assets are embedded and verified in a private cache, not
installed as mutable sidecars. Candidate precedence is explicit absolute
`--artifact`, then approved saved/explicit release URL plus independently trusted
archive digest, then optional `live-candidate` beside the selected executable.
Missing explicit candidates fail closed; doctor never downloads.

## Publish and verify

Keep the private publisher-key file locally. Configure a BMUX publishing profile
with `bmux broadcast setup`, the `https://bmux.dev` endpoint, channel `main`, and
that absolute credential-file path; then use `bmux broadcast start`. Audience
URL: `https://bmux.dev/live?channel=main`.

HTTP protocol v1 negotiates renderer `bmwp-3` / `BMWP\x03`; publishers, relay and
viewer assets must upgrade together. See [the protocol contract](live-broadcast-protocol.md).
Verify terminal/presenter output, pause/resume/stop, offline state, reconnects,
and audience privacy after deployment. Public read-only smoke does not prove
native/Canvas visual correctness or private-note exclusion by itself.

## Recovery and validation boundaries

Do not cancel an active apply, disable backend locking, force-unlock, or restore
old state over active work. Reconcile actual resources and state lineage before
explicit recovery. Bootstrap state/ledgers contain ownership metadata, never
reusable credentials. Keep provider diagnostics, Terraform plans and state
private; CI suppresses provider output.

Offline mocks, policy tests, formatting and OpenTofu validation establish local
contracts, not actual account entitlement, provider import endpoint behavior,
S3 concurrent lock exclusion, billing, SQLite migration, DNS/TLS activation, or
real cloud deployment success. Those remain explicit authorized production
acceptance checks. Infrastructure, token creation and secret writes are not run
by coding-agent tests.
