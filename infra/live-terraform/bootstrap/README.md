# R2 bootstrap boundary

There is no bootstrap OpenTofu module or separate bootstrap state. The explicit
`scripts/provision-live-state.py` companion only creates or accepts an existing
Standard R2 bucket using Cloudflare's bucket API. The deployment module owns the
bucket, history lock and lifecycle resources after explicit import/adoption.
The companion never changes an existing bucket's lifecycle or lock policies.

## Review and approval

```
python3 scripts/bootstrap-live-secrets.py --repo OWNER/REPO --bootstrap-state \
  --state-dir /PRIVATE/STATE --state-mode plan
# Copy the public operation identity (not credentials).
python3 scripts/bootstrap-live-secrets.py --repo OWNER/REPO --bootstrap-state \
  --state-dir /PRIVATE/STATE --state-mode review --state-operation OP
python3 scripts/bootstrap-live-secrets.py --repo OWNER/REPO --bootstrap-state \
  --state-dir /PRIVATE/STATE --state-mode apply --state-operation OP --accept-cost
```

The account comes from exact Cloudflare zone discovery. `--state-account-id`
optionally fences it to an expected 32-character account ID. The bucket defaults
to `bmux-opentofu-state`; `--state-bucket` cannot conflict with `LIVE_STATE_BUCKET`.
`--state-region` is `auto` only. Existing buckets require `--approve-import`, which
accepts bucket ownership, not policy replacement. Import into the deployment
module requires its independent reviewed adoption. Plan/review install no secrets.
`--dry-run` makes no subprocess or provider calls.

Standalone companion: `--repo OWNER/REPO --account-id ACCOUNT --state-dir ABS`
with `--plan`, `--review OP`, `--apply OP --accept-cost`, or `--reconcile OP`.
Its default (no action) is provider-free. Schema 2 public metadata contains only
`operation`, `phase`, `backend`, `resources`, and `schema`; there are no IAM roles
or GitHub OIDC fields. Local operation records bind exact choices and executable
script hashes. Unsupported/legacy ledgers and changed identities reject.
An interrupted creation with no confirmed creation identity is deliberately not
adopted by bucket name: inspect provider state and recover manually. Never delete
an authoritative operation to retry blindly.

## Secrets and scope

Use a separate `CLOUDFLARE_PROVISION_TOKEN` authorized to inspect the target zone,
manage the selected account's R2 buckets and create scoped API tokens. The script
looks up the `Workers R2 Storage Bucket Item Write` permission group with scope
`com.cloudflare.edge.r2.bucket`, then POSTs `/user/tokens` with only the resource
`com.cloudflare.edge.r2.bucket.ACCOUNT_default_BUCKET`. This is bucket object
read/write, not R2 account administration. See Cloudflare's R2 API token docs:
<https://developers.cloudflare.com/r2/api/tokens/>

The token ID becomes `R2_STATE_ACCESS_KEY_ID`; SHA-256 of the raw token value
becomes `R2_STATE_SECRET_ACCESS_KEY`, per Cloudflare's documented S3 derivation.
Neither raw token nor derived credentials enter public output or operation state.
GitHub receives secrets through stdin, never argv. Existing secrets are skipped;
credential rotation requires selecting both R2 keys with `--only` and `--rotate`.
No automatic old-token revocation occurs.

A new encrypted backend requires a securely supplied high-entropy
`TOFU_STATE_ENCRYPTION_PASSPHRASE`, mapped by the deployment workflow to
`TF_VAR_state_encryption_passphrase`, matching WWMTF's encryption variable.
Existing/adopted backends must retain their original passphrase via `--input-env`
or an existing GitHub secret; never generate a replacement. Passphrase rotation
is a manual encrypted-state migration, not a secret-only rotation.
`LIVE_STATE_BUCKET`, optional `LIVE_STATE_REGION=auto`, and
`CLOUDFLARE_ACCOUNT_ID` define the canonical R2 endpoint.

## First encrypted state

A fresh exact apply automatically creates a private completion descriptor at
`STATE_DIR/completion-OP.json` (override with `--state-completion-file ABS`) before
installing GitHub secrets. Bootstrap never creates synthetic empty state and never
writes a state object. The first actual OpenTofu apply creates canonical encrypted
state through explicit first-deployment admission.

The descriptor is schema 1, kind `bmux-live-r2-bootstrap-completion`, with exact
operation, backend account/bucket/key/endpoint, `created: true`, confirmed
`bucket_identity` and `binding` checksum. It contains no credentials. Standalone
companion `--review OP --completion-file ABS` can emit it after a completed new
creation. Reused/adopted buckets cannot produce it. Supply the descriptor explicitly
as `--bootstrap-completion ABS` to first configure; never to ordinary release.
The runtime must reverify remote creation generation and emptiness, and consume
and fence the receipt before provider effects with a conditional lock and durable
operation. Missing state without explicit valid admission remains an error.
Ambiguous interruption requires explicit recovery, never receipt replay or a new
lineage reset. The receipt is not authority to replace existing state.

For CI, the coordinator-owned workflow accepts bounded public descriptor JSON
and its expected SHA-256, verifies exact bytes, writes a private local file and
passes `--bootstrap-completion`. The workflow must retain a one-shot remote fence;
ephemeral runner files alone do not prevent replay. No credentials belong in this
input or in the descriptor.

Securely generate and back up a high-entropy `TOFU_STATE_ENCRYPTION_PASSPHRASE`
in your local environment BEFORE apply. The helper validates it before bucket
mutation and never generates/replaces a passphrase. It remains only in GitHub
secret stdin, never operation/resume metadata. After any deployment ambiguity,
inspect canonical state and the retained fence/lock, recover this original
passphrase, and follow manual reconciliation; never generate a competing lineage.
Existing backend secret installation uses `--input-env` with original
credentials/passphrase. A partial R2 credential installation requires explicitly
rotating/installing both keys; missing passphrases must be recovered, never
substituted.

Legacy AWS region, role or static-state-key inputs and old provisioning schemas
are explicitly rejected. Moving AWS state to R2 requires manual migration;
reusing the same bucket name does not transfer state authority.
