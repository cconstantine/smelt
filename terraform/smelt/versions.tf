terraform {
  # 1.7: `secret_env`'s validations call nonsensitive(), which 1.5 and 1.6
  # refuse inside a validation ("the given value is not sensitive"), even for
  # the empty default, failing every plan.
  required_version = ">= 1.7.0"

  required_providers {
    kubernetes = {
      source  = "hashicorp/kubernetes"
      version = "~> 3.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.1"
    }
  }
}
