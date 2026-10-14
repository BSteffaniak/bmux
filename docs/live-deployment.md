# Live production deployment

BMUX uses the existing `bmux.dev` Cloudflare zone. Production combines PWMTF's
local credential provisioner and separate approved infrastructure workflow with
WWMTF/Bcode's OpenTofu state on Cloudflare R2. No AWS account, IAM role, GitHub
OIDC customization, staging account, or staging deployment is required. The
R2 state operations use direct signed HTTPS requests from Python's standard library.
Ordinary local BMUX operation needs none of this infrastructure.

## One-time setup (one command)

From this repository, run:

```sh
bash scripts/setup-live.sh
```

The command enters the pinned Nix shell, reuses GitHub login (or opens login),
prompts privately for a Cloudflare provisioning token when needed, discovers the
account, and shows one combined plan for environments, backend and missing secrets.
Defaults are `BSteffaniak/bmux`, reviewer `BSteffaniak`, and bucket
`bmux-opentofu-state`; `--help` lists explicit overrides.

Approve the displayed changes/costs. For a new backend, supply and confirm a
high-entropy encryption passphrase from your password manager and confirm its
independent backup. There are no manual environment settings, exported secrets,
operation IDs, or JSON/checksum copying. The provisioning token is process-local,
not saved or installed in GitHub. Do not run with tracing or record secret input.

Rerun the **same command** to resume or verify. Existing secrets are skipped;
original generated credentials are retained in an owned 0600 `credentials.json`
inside the private bootstrap directory so interrupted secret uploads reuse them.
Protect/back up that directory and publisher key; neither is a public resume
artifact. An uncertain token creation is checkpointed and blocks automatic retry;
an ambiguous backend apply requires explicit reconciliation, never a replacement
backend or new credentials. The wrapper never adopts an existing bucket or changes
an operation's bound implementation/selection. Remote drift fails closed.

After success it saves private `configure-inputs.json` and prints a ready-to-run
`gh workflow run ... --json < ...` command. **It does not dispatch CI.** That input
file is for the first configure only; after canonical state exists, omit bootstrap
inputs on subsequent runs. A successful rerun verifies GitHub protections, secret
names and original local authority; it does not prove remote credential validity
or cloud deployment acceptance. Keep existing backend/adoption/recovery workflows
explicit rather than using setup to reset an existing installation.

## Interrupted bucket creation recovery

If bucket creation succeeded remotely but setup stopped with backend phase
`applying`, retain the original private bootstrap directory and passphrase. Do not
delete/recreate the bucket, start another operation, or edit its phase manually.
Use its successful Cloudflare account audit UUID and exact event timestamp:

```sh
bash scripts/setup-live.sh \
  --recover-creation-audit AUDIT_RECORD_UUID \
  --creation-time '2026-10-14T16:38:01-04:00'
```

Use the original timestamp, not the date you run recovery. The dashboard may omit
fractional seconds: if verification reports a mismatch, use the exact normalized
`API=` timestamp shown for the verified audit UUID. The check remains exact and
still requires the original operation window. The provisioning token
additionally needs **Account → Account Settings → Read** for the selected account.
Recovery reads the exact audit through `/accounts/ACCOUNT/audit_logs?id=UUID` and
reads live bucket metadata; it never writes to Cloudflare. It checks successful
API creation, account/resource identity, event time within five minutes after the
original durable applying checkpoint, and live bucket creation time within five
seconds of that event. Unsupported/missing fields and replacement buckets fail
closed. It displays evidence and requires explicit confirmation, then verifies
again before saving private evidence and an original checkpoint backup. Only then
does it acknowledge creation in the original operation; reviewed choices, binding,
operation ID and resource plan are unchanged. It never accepts a changed bound
implementation to get past an error. If those implementation files changed, restore
the original verified implementation or arrange explicit migration, never rebind.

Recovery resumes ordinary setup (with its separate setup approval) and produces the
original completion receipt without a second bucket creation request. Interrupted
local reconciliation can be repeated with the same arguments. Audit visibility
and timestamp correlation are required evidence, not a claim of atomic provider
transactions. CI's first-use admission still verifies the bucket is empty and
rejects consumed/changed authority before first state creation.

## Advanced setup / explicit phases

### 1. Authenticate to GitHub

Enter the locked development shell from the repository root:

```sh
nix develop --command bash
python3 --version
gh --version
tofu version
gh auth login
```

The shell supplies Python, GitHub CLI, and pinned OpenTofu
1.10.9. No AWS CLI, AWS SSO login, or AWS account is required.
Authenticate `gh` for `BSteffaniak/bmux` with repository administration and
Actions environment/secret write permissions.

No environment dashboard setup is needed. The helper's `--approval-reviewer`
selects a GitHub user and includes both environments in the same state operation:

* `Production`: deployment secrets; no required reviewers.
* `production-approval`: the selected required reviewer, self-review allowed.
* Both: exactly branch `master` (no tags/wildcards); administrator bypass disabled.

Plan/review only inspect remote configuration. Apply creates missing environments,
leaves matching ones untouched, and verifies each change before proceeding to
backend provisioning or secret writes. Existing conflicting protections are shown
in the review and require `--approve-environment-changes` on apply. Existing wait
timers are preserved; unknown/custom protection integrations are rejected rather
than overwritten. API/permission failures never mean an environment is missing.
Without `--approval-reviewer`, the helper retains its verification-only behavior.

Existing credentials are skipped; replacement requires `--rotate` with explicit
`--only NAME` selections. Keep the state encryption passphrase backed up securely;
losing it prevents decrypting the state and its archives.

### 2. Provision Cloudflare and R2 credentials

Supply `CLOUDFLARE_PROVISION_TOKEN` through a credential manager or protected
environment, not command arguments. This provisioning token needs API Tokens
Read/Edit, read access to `bmux.dev`, and authority to grant the required scoped
Workers/Pages, DNS/routes, and R2 permissions. It is never installed in GitHub.
Use BMUX-specific credentials, not PWMTF/WWMTF/Bcode deployment tokens.

For Bash, securely enter the provisioning token and create private directories:

```sh
read -r -s -p 'Cloudflare provisioning token: ' CLOUDFLARE_PROVISION_TOKEN
printf '\n'
export CLOUDFLARE_PROVISION_TOKEN
umask 077
mkdir -p "$HOME/.config/bmux/live" "$HOME/.local/state/bmux/live-bootstrap"
chmod 700 "$HOME/.config/bmux/live" "$HOME/.local/state/bmux/live-bootstrap"
```

Do not enable shell tracing or print secret environment variables. Use the
provider-free `--dry-run` before performing provisioning. The helper discovers
the existing zone/account, defaults to hostname `bmux.dev` and Pages project
`bmux`, and generates a publisher key only at its explicit private path if absent.
Only the publisher key digest is installed in GitHub.

Use the helper in explicit plan/review/apply phases. The plan prints a public
operation ID; substitute it for `OPERATION_ID` below. Do not change checkout or
backend choices between these phases, since the operation is implementation-bound.

```sh
bootstrap_live() {
  python3 scripts/bootstrap-live-secrets.py --repo BSteffaniak/bmux \
    --approval-reviewer BSteffaniak \
    --publisher-key-file "$HOME/.config/bmux/live/publisher-key" \
    --bootstrap-state --state-dir "$HOME/.local/state/bmux/live-bootstrap" "$@"
}
bootstrap_live --dry-run
bootstrap_live --state-mode plan
bootstrap_live --state-mode review --state-operation OPERATION_ID
```

If a bucket is already designated for this backend, set `LIVE_STATE_BUCKET`
before planning; adopting it additionally requires `--approve-import` and its
original encryption material. The default for a new backend is
`bmux-opentofu-state`. Existing/adopted buckets cannot use new-bucket admission.
Missing state there is a recovery error, never permission to reset it.

Before apply, generate a high-entropy encryption passphrase in your credential
manager and back it up independently. Load it securely without shell history:

```sh
read -r -s -p 'State encryption passphrase: ' TOFU_STATE_ENCRYPTION_PASSPHRASE
printf '\n'
export TOFU_STATE_ENCRYPTION_PASSPHRASE
bootstrap_live --state-mode apply --state-operation OPERATION_ID --accept-cost \
  --state-completion-file "$HOME/.local/state/bmux/live-bootstrap/ready.json"
```

If review reports conflicting existing GitHub environment settings, inspect the
printed before/desired policies and explicitly add `--approve-environment-changes`
to that apply command. It never grants approval to unplanned drift. Keep the same
reviewer selection in every phase. Environment writes have private durable
checkpoints: after an ambiguous response, explicitly rerun the **same** reviewed
apply to accept only the exact expected effect or retry a confirmed absent effect.
Unexpected remote changes require reconciliation, not a silent overwrite. GitHub
does not offer atomic compare-and-swap for environment settings, so avoid concurrent
administration during bootstrap; the helper rechecks before and after each write.

The helper requires the original passphrase before bucket mutation; it never
silently generates or replaces encryption material. For a confirmed new bucket,
it writes a private completion receipt for first-deployment admission. It does
not upload synthetic empty state. The first actual OpenTofu infrastructure apply
creates the canonical encrypted state. Keep the receipt and independent passphrase
backup; neither receipt nor operation metadata contains credentials.
After success, verify names only and remove the bootstrap token from this shell:

```sh
gh secret list --repo BSteffaniak/bmux --env Production
unset CLOUDFLARE_PROVISION_TOKEN
```

The R2 backend contract is:

* `LIVE_STATE_BUCKET`: the explicitly selected R2 bucket.
* `LIVE_STATE_REGION`: `auto`.
* `CLOUDFLARE_ACCOUNT_ID`: selects the canonical
  `https://ACCOUNT.r2.cloudflarestorage.com` endpoint.
* `R2_STATE_ACCESS_KEY_ID` and `R2_STATE_SECRET_ACCESS_KEY`: dedicated bucket-scoped
  S3-protocol credentials, not AWS IAM keys.
* `TOFU_STATE_ENCRYPTION_PASSPHRASE`: high-entropy private encryption material,
  mapped to `TF_VAR_state_encryption_passphrase` only for OpenTofu.

Endpoint overrides, old AWS role/backend choices, and old AWS operation ledgers
are rejected rather than silently reinterpreted as R2. Existing bucket selection
must be explicit and consistent; never initialize a competing backend to bypass
an error. Bootstrap metadata and operation reviews must not contain credentials.

### Validation versus deployment

`Live Validation` runs offline infrastructure/setup tests, relay tests, and native
runtime/renderer acceptance on pull requests and relevant master pushes. General
Rust tests and linting remain in `Build and Test`. These are separate from the
configure/release workflow: deployment builds/packages the candidate without
rerunning test/lint suites. `test-live-deployment-boundary.py` guards that separation.

Artifact inventory/checksums, exact revision, production approval, encrypted backend
locking, bootstrap admission/import ownership checks, plan policy and public
post-deploy health checks remain part of deployment. They are operational safeguards,
not duplicate unit tests. Separate validation workflows are not implicitly a deploy
dependency; use repository branch protection to require their results before merge.

### 3. Configure production infrastructure in CI

Merge the implementation/workflows into `master`, then run **Configure Production
Infrastructure**. CI builds and verifies one immutable candidate, waits for
`production-approval`, and configures infrastructure under the selected R2
backend lock. For the first configure only, supply the exact `ready.json` contents
in `bootstrap_completion` and its file SHA-256 in `bootstrap_completion_sha256`:

```sh
sha256sum "$HOME/.local/state/bmux/live-bootstrap/ready.json"
```

The checksum covers the exact file bytes, including any trailing newline; do not
reformat the JSON. The workflow writes a private local receipt and passes
`--bootstrap-completion` to configure. Admission verifies the exact backend and
remote bucket creation identity and emptiness, then consumes a conditional remote
marker before effects. An interrupted or ambiguous admission/apply requires
explicit recovery; replaying the receipt must not create another state lineage.
Subsequent configure/release runs leave both inputs empty and require existing
canonical encrypted state. OpenTofu state and saved plans use enforced client-side AES-GCM
encryption, matching WWMTF; native S3-protocol lockfiles remain enabled.

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
lock. State archives preserve encrypted remote bytes, not plaintext state dumps.

## Subsequent releases

Docs pushes on `master` call **Live production release**, or dispatch that
workflow manually. CI builds once without deployment secrets, verifies the
artifact checksum and revision, waits for production approval, updates Worker
code/version deployment and uploads matching website/viewer assets. It rejects
infrastructure creation, imports, DNS/project changes, and destructive changes;
run Configure Production Infrastructure for deliberate topology changes.
Immutable Worker-version replacement is the expected release exception.
No local frontend build, candidate upload, or staging promotion is required.

The normal native installer also supports `bmux broadcast deploy` for separately
hosted installations; that is not the normal bmux.dev CI deployment path.
Installer/provider assets are embedded and verified in a private cache, not
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

## Recovery and migration

Do not cancel an active apply, disable backend locking, force-unlock, or restore
old state over active work. Reconcile actual resources and state lineage before
explicit recovery. Keep diagnostics, plans, state, credentials and encryption
material private. CI suppresses provider output.

If an earlier AWS backend was actually deployed, this change does not migrate it
or destroy AWS resources. Stop all writers; preserve the original state and
credentials, establish its lineage/serial, and arrange a separately reviewed
OpenTofu backend/encryption migration with recoverable backups before enabling
R2 writers. Do not merely remove the old environment variables and initialize
empty R2 state. Resume descriptors from AWS operations are not R2 authority.
Likewise, changing the R2 account, bucket, state key, or encryption passphrase is
an explicit migration, not ordinary credential rotation.

Offline mocks, policy tests, formatting and OpenTofu validation establish local
contracts, not actual account entitlement, provider import behavior, live R2
concurrent lock exclusion, retention enforcement, billing, SQLite migration,
DNS/TLS activation, or real cloud deployment success. These remain explicit
authorized production acceptance checks. Infrastructure, token creation and
secret writes are not run by coding-agent tests.
