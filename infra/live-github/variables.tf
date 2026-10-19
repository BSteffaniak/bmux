variable "fly_config" {
  description = "Public Fly deployment choices; credentials remain environment secrets."
  type = object({
    app    = string
    org    = string
    region = string
  })
}
variable "existing_variables" {
  description = "Existing Fly repository variables to adopt without replacement."
  type        = set(string)
  default     = []
  validation {
    condition     = length(setsubtract(var.existing_variables, ["LIVE_FLY_APP", "LIVE_FLY_ORG", "LIVE_FLY_REGION"])) == 0
    error_message = "Only the three Fly deployment variables may be adopted."
  }
}
locals {
  fly_variables = {
    LIVE_FLY_APP    = var.fly_config.app
    LIVE_FLY_ORG    = var.fly_config.org
    LIVE_FLY_REGION = var.fly_config.region
  }
}
resource "github_actions_variable" "fly" {
  for_each      = local.fly_variables
  repository    = var.repository
  variable_name = each.key
  value         = each.value
  lifecycle { prevent_destroy = true }
}
import {
  for_each = var.existing_variables
  to       = github_actions_variable.fly[each.key]
  id       = "${var.repository}:${each.key}"
}
