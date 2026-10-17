# Live production deployment

## Recover an inspected partial cutover

Run `gh workflow run live-recover.yml --repo BSteffaniak/bmux --ref master`
after pushing the recovery workflow. It reuses both pinned artifacts without
compilation and previews missing relay resources. Approve `production-approval`
only after reviewing that preview. Recovery requires an existing managed Fly app,
no remaining Worker resources, and the original `applying` migration marker.
The approval digest binds current encrypted state, marker identity/ETag, candidate
inventory, configuration and plan. Recovery permits only missing relay-resource
creation; no imports, app recreation, deletion, state reset or marker clearing.

After applying the saved plan under normal locking, backend health must pass
before the original marker is conditionally completed. The site publishes only
after recovery succeeds. Apply/health failures retain the applying marker; inspect
state before another explicit recovery preview. Completed migrations cannot use
recovery. If only site publication fails after completion, retry the site workflow,
not backend recovery. `live-inspect.yml` remains the read-only inspection command.


## End state and source-ready status

The backend is one event-driven Rust `bmux-live-relay` process on Fly, not a
Cloudflare Worker or polling backend. Pages remains at `bmux.dev`; the relay
origin is `https://live.bmux.dev`. Local BMUX needs neither service.

This is a **source-ready migration, not a verified production deployment**.
No live state, credentials, resources or DNS were changed during implementation.
Production remains blocked until an operator supplies Fly credentials/variables,
reviews existing ownership, approves retirement, and validates the packaged image
and runtime against the real services. Downtime is intentionally accepted.

## Guided automation (recommended)

After these changes are merged to `master`, run:

```sh
bash scripts/setup-live.sh --fly --repo BSteffaniak/bmux
bash scripts/setup-live.sh --fly --repo BSteffaniak/bmux --approve
```

The first command lists missing configuration names without writing anything.
The approved run discovers Fly organizations and current non-deprecated regions,
asks for missing public settings, checks app-name availability, and installs only
missing GitHub values. Existing values are preserved. You supply the Fly token
privately and keep the separate metrics bearer key backed up independently.
Account creation, billing authorization and token scope/expiry remain operator
responsibilities. Installation requires explicit billing and key-backup
acknowledgments. Do not run concurrent installers: GitHub variable/secret writes
have no atomic create-only API.

In GitHub Actions, dispatch **Live production release** on `master` with
`migrate=true`. This is the one-time cutover, not the ordinary release button.
The one-time `migrate=true` recovery path skips runtime acceptance so browser
startup or unrelated terminal compatibility failures cannot block restoring live
service. Offline infrastructure/safety checks remain required, as do the exact
preview digest, state locks, replay fence and production approval. Standalone
Live Validation and ordinary releases still run the complete acceptance suite.
This deliberately accepts unverified runtime risk during the cutover; verify a
real stream immediately afterward.

For temporary migration debugging, dispatch with `migrate=true` and
`reuse_backend=true` to reuse the backend candidate from run `39048863096`.
This skips Rust, bundled-plugin and OCI image compilation. The archive checksums
and full source inventory are verified; product/dependency changes since that
build reject reuse. Current infrastructure is packaged with the original image
and native binary, and `BACKEND_SOURCE.json` records their original revision and
archive hash. The image remains digest-pinned and is published only during the
approved deployment. The same option now reuses the site/viewer artifact from
that run, with `SITE_SOURCE.json` recording its original provenance. Site source
and build-script changes reject reuse. Both jobs still package/upload candidates
but skip compilation. Reuse is opt-in, migration-only,
and depends on that run's artifact retention; ordinary releases still rebuild.

Migration runs first perform an authenticated, read-only infrastructure plan
before compiling backend or site candidates. This check uses a synthetic image
reference only for planning; it never uploads an image, emits an approval digest,
or applies a plan. Failed OpenTofu commands identify the operation and report
bounded, redacted structured errors, not raw provider logs. The later preview
still binds approval to the actual built candidate.

The workflow builds same-revision backend/site candidates and runs the selected
validation, then shows a read-only migration preview before approval. Review its
resource identities/actions, downtime and paid resources. Approval is bound to
the candidate inventory, encrypted canonical state, configuration and full private
plan; any mismatch stops before resource effects. No plaintext plan is uploaded.
The approved backend stages retirement, Fly app creation, image publication and
remaining resources under the existing infrastructure lock. The separate Pages
publisher then uploads the matching site and public smoke checks report results.
Ordinary releases also validate and publish the matching backend/site revision.
Standalone Docs push runs build only, preventing a premature viewer cutover;
explicit Docs dispatch remains available for a reviewed site-only recovery.
GitHub environment protections still apply: if `Production` itself requires
approval, its read-only preview job may need that additional approval before
`production-approval` displays the migration gate.

A durable conditional-write marker under `migration/<canonical-state-key>.json`
fences the one-way operation. It is an operational replay guard, **not another
infrastructure state authority**. Completed migration cannot be rerun. If a phase
fails or its acknowledgement is lost, the workflow stops without automatic retry
or rollback. Preserve the marker, encrypted canonical state and archive history;
inspect actual resources under the same infrastructure lock before a separately
reviewed recovery. Do not delete markers, force-unlock, choose another app name,
rebootstrap, or repeat migration to bypass an ambiguous result. A Pages failure
after backend completion needs only the matching site publisher/review, not
another infrastructure migration.

The lower-level configure and retirement commands below remain available for
explicitly reviewed recovery; they are not alternate automatic resume paths.
Public smoke checks do not prove production interactive broadcasting, volume
persistence, metrics retention or successful native installation. Install the
matching native BMUX version and perform a deliberate publisher/viewer check
before production acceptance. Monitoring destination/retention/alerts remain
unprovisioned rather than silently choosing another paid service.

## Declarative ownership

OpenTofu 1.10.9 owns the existing Cloudflare Pages project/domain/DNS, R2 state
bucket policies, and new Fly resources. `registry.terraform.io/ampbase-io/fly`
0.3.2 is a maintained community Machines API provider (release 2026-10-16).
The OpenTofu registry did not mirror it at inspection time; use the explicit
Terraform registry source. Signed release checksums are committed for Linux
amd64 and macOS arm64. `fly-apps/fly` is archived/unmaintained; `matifali/fly`
is deprecated; neither is used.

Verified provider schema and tagged source support app, IP, machine, encrypted
volume, certificate and certificate-validation resources. Machine updates use
the Machines update API; region/app/volume changes require replacement and are
blocked by plan policy. There is no local-exec resource emulation. Provider
service settings are not fully refreshed from the remote API: provider docs
explicitly describe this drift-detection limitation. Operator-side changes are
not a supported deployment path.

Exactly one Machine has 1 shared CPU, 1024MB RAM, configurable region (default
`iad`), restart always, autostop off, and HTTPS port 443 forwarded to container
8080. No autoscaler, second Machine, or create-before-destroy is declared.
`/health` is readiness. `/metrics` is content-free and authenticated; Fly's
unauthenticated metrics scraper is deliberately not configured. No authenticated
collector, durable metrics retention, dashboard or alerting is provisioned either.
The endpoint alone does not establish monitoring: counters are process-local and
lost on restart. A separately reviewed private collector and retention policy
are required before claiming durable observability.

An encrypted 1GiB `fly_volume.authority` mounts at `/data`, with
`prevent_destroy`; only `/data/relay/authority.json` authority metadata is durable.
No frame/history storage is intended. Volume deletion, replacement or relocation
requires an explicit recovery/migration design, never ordinary release approval.
One Machine/volume is not HA. Region changes are not a routine release.

The relay receives only SHA-256 digests, never publisher/metrics bearer keys:
`PUBLISHER_KEY_SHA256`, independent `METRICS_KEY_SHA256`, and
`ALLOWED_ORIGINS=https://bmux.dev,https://www.bmux.dev`. The two keys must differ.
TLS certificate creation is asynchronous: DNS-only relay and ACME CNAME records
precede the genuine `fly_cert_validation` gate. Cloudflare proxying is disabled
for the relay origin.

## Credentials and setup (operator action only)

Do **not** rebootstrap or recreate the canonical R2 backend for this migration.
The existing `bash scripts/setup-live.sh` owns Cloudflare/R2 setup and recovery,
but does not provision Fly credentials. Its successful readiness message alone
is not Fly deployment readiness.

A separate, additive installer accepts **existing** Fly organization credentials
and an independent backed-up metrics key through private terminal prompts:

```sh
bash scripts/setup-live.sh --fly --repo BSteffaniak/bmux
# After reviewing missing secret names only:
bash scripts/setup-live.sh --fly --repo BSteffaniak/bmux --approve
```

It installs only missing `Production` environment secrets `FLY_API_TOKEN` and
`LIVE_METRICS_KEY_SHA256`, never replaces existing values or creates Fly resources.
Token values travel through stdin, not argv, Terraform, files or logs. Keep the
original metrics bearer key in an independent password manager for authorized
private scraping. Do not record prompts or run with shell tracing.

Set repository **variables** `LIVE_FLY_APP` (globally unique and available before
build), `LIVE_FLY_ORG`, and optionally `LIVE_FLY_REGION`. Existing `Production`
Cloudflare/R2/publisher-digest secrets remain unchanged. The first configure
requires an organization-scoped Fly token capable of app creation, public IPs,
volumes, certificates, Machines and registry upload; an app-only deployment token
cannot bootstrap the app. Review least privilege and expiration with Fly before
installation; actual token capabilities were not tested against production.
There is no automated token creation/rotation. The `--fly` setup route is explicit
and additive; it does not repeat the original Cloudflare/R2 bootstrap. Missing
values fail closed. The legacy local `live-install.py` deployment path rejects
Fly candidates; use the protected configure/release workflow.

## Immutable build and approved deployment

`Live Validation` remains separate from release; deployment does not run tests.
`Live production release` builds a candidate without deployment credentials,
then requires `production-approval`, then runs in `Production` under the existing
non-cancelling `bmux-production-infrastructure` concurrency lock. Configure calls
the same owner through `Configure Production Infrastructure`. Pages publishing
remains a separate workflow and publisher.

`build-live-artifact.sh` uses the pinned Nix compiler and target directory:
`cargo build --locked --release -p bmux-live-relay`, `cargo metadata` to resolve
the explicit executable path, then `packages/live-relay/package-rootfs.sh` to
package the complete Nix closure under `artifacts/live-relay-rootfs`. The owned
`packages/live-relay/Dockerfile` builds the scratch image from that rootfs. Startup
initializes the mounted `/data/relay` directory, then drops privileges before
serving; the relay must not run permanently as root. Metadata remains exclusively
at `/data/relay/authority.json`.
Buildx produces an OCI archive and exact `registry.fly.io/<app>@sha256:…`
reference. Both join the candidate SHA256 inventory, revision, Terraform files
and provider lock. No floating Rust toolchain or deployment-time image build.

Configure preflights app-name collisions and refuses implicit adoption. It first
applies only `fly_app.live`, archives encrypted state, then uploads the prebuilt
OCI image with `skopeo --preserve-digests` and applies the full reviewed saved
plan. This ordering makes registry upload possible for a new app. Every saved
plan passes policy before any image publication; an ordinary release permits
only image-only in-place Machine updates. App credentials use a private transient
registry auth file, removed on exit. Plan/provider diagnostics are suppressed;
plaintext plans/state are not uploaded as artifacts.

Existing Fly apps are not auto-imported. Provider source supports composite
Machine imports `<app>/<machine-id>` and resource-specific imports for other
objects, but a general reviewed Fly import UI is not implemented. If a partial
create succeeds before state publication, stop and reconcile identity under
explicit operator review; do not pick a new app name, force-unlock, or retry
blindly. Nonroot volume write access and packaged runtime readiness must be
verified before production acceptance.

Site build consumes `BMUX_LIVE_ENDPOINT_ORIGIN=https://live.bmux.dev` into the
content-addressed viewer bundle. Asset manifests use protocol 1 / renderer
`bmwp-4`; offline builder/integrity checks remain separate from live acceptance.

## Canonical state, locking and recovery

The existing R2 key, bucket and encrypted state remain the sole authority.
`live-state.py`, `archive-live-state.py`, `live-state-recovery.py`, and existing
OpenTofu encryption configuration remain in place. Preserve R2 signed conditional
writes, OpenTofu lockfile locking, private credentials, encryption passphrase,
non-cancelling concurrency, and encrypted immutable history retention. Never
publish plaintext state or plan JSON. Do not change backend identity or use a
local state file as a replacement for canonical state.

Before initialization/application the deployment checks canonical state and
archives encrypted remote bytes; after each successful stage it archives again.
Unknown/missing/corrupt state is a recovery error, not a new installation.
First-bootstrap completion/admission exists only for genuinely new installations,
not this migration. Existing-state list failures must never become empty state.
Interrupted operations require inspection of the canonical encrypted revision
and locks through existing recovery tooling. Retain independently backed-up
encryption and publisher credentials. No automatic force-unlock/state overwrite.

## Explicit former Worker retirement (downtime)

1. Review existing encrypted state lineage and exact managed identities:
   `cloudflare_workers_route.live`, `cloudflare_workers_deployment.live`,
   `cloudflare_worker_version.live`, `cloudflare_worker.live`. Confirm these are
   BMUX's old route/script and not another service. Preserve all R2, Pages,
   website DNS and unrelated Workers. Review any Durable Object data retention
   requirements before deleting the script; snapshots of encrypted Terraform
   state do not back up Durable Object application data.
2. Stop publishers. Run **Configure Production Infrastructure** with
   `retire_worker=true`, with bootstrap completion empty. The existing approval
   environment and infrastructure lock still apply. This executes a bounded
   targeted destroy and policy allows deletes only for those four addresses.
   Worker version/deployment deletes may be state-only in Cloudflare provider
   5.17.0; removing the script/route retires actual serving resources. No blanket
   account cleanup, state rm, or automatic resource adoption.
3. Archive/check the resulting encrypted canonical state and confirm the exact
   route/script are gone. If the plan includes any other delete or create/update,
   stop: the policy rejects it. Reconcile extra legacy resources individually.
4. Run configure separately with `retire_worker=false`; review Fly resources,
   certificate validation and new DNS. Deploy the independently built site and
   update publishers to the new relay origin. There is no compatibility backend.
5. Verify public health, authenticated metrics without payload content, publisher
   connection, viewer repair/reconnect, restart authority persistence and rejection
   of obsolete routes. Remove obsolete Worker source/dependencies through their
   owning application lane, not by deleting unrelated Cloudflare resources.

## Public read-only smoke

```sh
python3 scripts/smoke-live-broadcast.py --url https://bmux.dev --relay-url https://live.bmux.dev --component backend
python3 scripts/smoke-live-broadcast.py --url https://bmux.dev --relay-url https://live.bmux.dev --component site
```

Backend probes `/health` only (protocol 1, `bmwp-4`), not legacy status polling.
Site smoke discovers immutable assets at `bmux.dev`, checks every size/hash,
WASM magic, and aggregate bundle identity. It never publishes content or prints
remote bodies, credentials or arbitrary diagnostic headers. Safe bounded timing,
status and Cloudflare ray metadata may be retained. These probes do not prove
volume write access, authenticated metrics, or interactive stream acceptance.
