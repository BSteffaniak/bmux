mock_provider "github" {
  mock_data "github_user" {
    defaults = { id = "7" }
  }
}
variables {
  owner             = "owner"
  repository        = "repo"
  approval_reviewer = "owner"
  fly_config = {
    app    = "bmux-test"
    org    = "example"
    region = "iad"
  }
}
run "single_approval_gate" {
  command = plan
  assert {
    condition     = length(github_repository_environment.release["Production"].reviewers) == 0
    error_message = "Credential jobs must not request another approval."
  }
  assert {
    condition     = github_repository_environment.release["production-approval"].reviewers[0].users == toset([7])
    error_message = "One designated reviewer must approve the shared release."
  }
  assert {
    condition = alltrue([for env in github_repository_environment.release :
      !env.can_admins_bypass && !env.deployment_branch_policy[0].protected_branches && env.deployment_branch_policy[0].custom_branch_policies
    ])
    error_message = "Both environments must restrict branches and disable admin bypass."
  }
  assert {
    condition     = alltrue([for policy in github_repository_environment_deployment_policy.master : policy.branch_pattern == "master" && policy.tag_pattern == null])
    error_message = "Only master branches, never tags, may deploy."
  }
  assert {
    condition     = github_actions_variable.fly["LIVE_FLY_APP"].value == "bmux-test" && length(github_actions_variable.fly) == 3
    error_message = "Public deployment configuration belongs in Terraform."
  }
}
