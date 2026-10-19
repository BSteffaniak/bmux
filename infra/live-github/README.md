# GitHub release infrastructure

This admin-only OpenTofu root owns:

- `Production`: credentials, master-only deployment, **no reviewer gate**.
- `production-approval`: master-only, designated reviewer, no admin bypass.
- The public `LIVE_FLY_APP`, `LIVE_FLY_ORG`, `LIVE_FLY_REGION` repository variables.

Normal releases do not apply this root or receive repository-administration tokens.
The GitHub provider uses `GITHUB_TOKEN` from the operator environment. Do not put
credentials in tfvars, shell arguments, or committed files. Secret installation
remains outside Terraform so reusable credentials never enter this root's state.

## Existing installation: adopt before changing

Use a private working directory/state backup and restrictive umask (`umask 077`).
The default local state belongs to this root, not the live application's R2 state.
Keep it durable and backed up; never run from an ephemeral CI checkout. A dedicated
remote backend can be configured and migrated explicitly later; do not reuse the
application's state key.

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
tofu -chdir=infra/live-github init -lockfile=readonly
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
