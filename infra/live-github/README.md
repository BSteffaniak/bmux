# GitHub release infrastructure

This admin-only OpenTofu root owns:

- `Production`: credentials, master-only deployment, **no reviewer gate**.
- `production-approval`: master-only, designated reviewer, no admin bypass.
- The public `LIVE_FLY_APP`, `LIVE_FLY_ORG`, `LIVE_FLY_REGION` repository variables.

Normal releases do not apply this root or receive repository-administration tokens.
The provider is pinned to 6.10.2: 6.6.0's branch-policy importer omitted the
repository/environment identity fields and incorrectly proposed replacements.
`test-live-github-provider-import.mjs` exercises the real pinned provider against
a loopback API fixture (no cloud access or apply), retaining `prevent_destroy`.
The GitHub provider uses `GITHUB_TOKEN` from the operator environment. Do not put
credentials in tfvars, shell arguments, or committed files. Secret installation
remains outside Terraform so reusable credentials never enter this root's state.

## Simplest path: temporary GitHub Actions migration

1. Create a short-lived fine-grained GitHub token limited to this repository:
   **Administration: read/write**, **Environments: read/write**, and
   **Variables: read/write** (Metadata read is implicit). No contents-write or
   secrets-read permission is needed.
2. Add it as the `Production` environment secret `LIVE_GITHUB_ADMIN_TOKEN`.
3. Push this code to `master`. Run **One-time GitHub Terraform migration** in
   Actions. There are no inputs: it discovers existing IDs and public values,
   shows the Terraform plan in the run summary, then waits for one approval.
4. Review and approve `production-approval`. Wait for the apply and verification
   job to finish successfully.
5. Delete `.github/workflows/migrate-live-github.yml`,
   `scripts/migrate-live-github.mjs`, `scripts/check-live-github-migration.mjs`,
   `scripts/live-github-plan-diagnostics.mjs`,
   and `scripts/test-live-github-migration.mjs`. Remove its test command from
   `live-validation.yml`. Delete the admin secret and revoke the token.

Keep this Terraform root, its lockfile, and `scripts/live-github-init.sh`.
State persists encrypted in the existing R2 bucket under the separate key
`bmux/github/production/terraform.tfstate`; the live application's state is untouched.
Existing R2 credentials must permit this key and its `.tflock` object. No new bucket
or application deployment is created. Plan artifacts expire after one day; they
are **not** the state authority. Canonical state stays in R2.

This deliberately stops for unexpected reviewers, timers, custom protections,
branch policies, missing resources, or public configuration changes. It never
silently deletes or recreates infrastructure. If a job fails after apply starts,
keep the workflow/token, start a **new full run** to refresh canonical state and
obtain a fresh approval; do not rerun only the old apply job or reset state.
A repeated completed run should plan no changes.

If this root was already applied using local state, stop: migrate that original
state to the remote backend explicitly before running this workflow. Do not import
the same resources into a second independent state.

## Manual administration alternative

Use a private working directory/state backup and restrictive umask (`umask 077`).
Export the existing R2 credentials, account ID, bucket, and encryption passphrase
using the environment names consumed by `scripts/live-github-init.sh`. Source that
script from Bash to initialize the dedicated backend and export Terraform's
credential/encryption variables. Do not reuse the application's state key.

Create an ignored `production.tfvars` with the **current** public values:

```hcl
owner             = "BSteffaniak"
repository        = "bmux"
approval_reviewer = "BSteffaniak"
fly_config = {
  app    = "CURRENT_APP"
  org    = "CURRENT_ORG"
  region = "CURRENT_REGION"
}
existing_environments = ["Production", "production-approval"]
existing_variables    = ["LIVE_FLY_APP", "LIVE_FLY_ORG", "LIVE_FLY_REGION"]
existing_master_policy_ids = {
  Production          = 123 # Replace with the actual existing policy ID.
  production-approval = 456 # Replace with the actual existing policy ID.
}
```

Inventory before planning (public metadata only):

```sh
gh api repos/BSteffaniak/bmux/environments
gh api repos/BSteffaniak/bmux/environments/Production/deployment-branch-policies
gh api repos/BSteffaniak/bmux/environments/production-approval/deployment-branch-policies
gh variable list --repo BSteffaniak/bmux
```

Inspect the environment details and custom deployment protections too. Import
blocks adopt only the explicitly listed resources. **Unlisted existing branch/tag
policies are not automatically deleted by Terraform.** If extras or custom
protection integrations exist, stop and explicitly reconcile them; do not assume
the plan covers them. The Python admission checker rejects unexpected policies.

```sh
source scripts/live-github-init.sh
tofu -chdir=infra/live-github plan -var-file=production.tfvars -out=production.tfplan
# Review imports, reviewer configuration, branch policies and public variables.
tofu -chdir=infra/live-github apply production.tfplan
```

For a genuinely new installation, omit adoption entries for absent resources.
Never omit them to bypass an existing resource or overwrite unknown ownership.
Lifecycle guards prohibit resource destruction. Import blocks may remain after
successful adoption. Keep the saved state; old Python setup ledgers are not an
alternative authority and are not consumed to mutate these resources anymore.

## Deployment approval

`live-promote.yml` builds both candidates, validates and previews, then requests
one approval covering backend and frontend. Its successful backend job calls the
site publisher with the verified same-run candidate; the site approval job skips.
The publisher checks the caller workflow identity and still verifies SHA/checksums.
Independently dispatched site repair requires its own single approval.

Terraform apply is an explicit administrative operation, not another per-release
approval. Applying this root also removes an accidental reviewer gate on
`Production`; inspect that change during migration.

## Offline validation

```sh
tofu -chdir=infra/live-github fmt -check -recursive
tofu -chdir=infra/live-github validate
tofu -chdir=infra/live-github test
```

Tests use a mocked provider and do not contact GitHub or change resources.
