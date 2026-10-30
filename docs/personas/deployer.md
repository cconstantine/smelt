# Deployer
Part of the [development process](../development-process.md).
smelt's machine persona file is `.agents/personas/deployer.md`; because the seat starts no one its `smelt.tools` can complete the *write* reach over the `read-only` ceiling (reads pass by class whether named or not) — it is a leaf, so unlike the [Project Manager](project-manager.md)'s and [Ticket Driver](ticket-driver.md)'s deny-only files, the conjunction-chain argument places no cap on it.

## One deploy per seat

The Deployer runs exactly one deploy of one named SHA of `main`, per [homelab's `smelt-deployer/DEPLOY.md` at `3549019`](<https://github.com/cconstantine/homelab/blob/3549019cc1cee26251597f92a5c338ce0be1a4b8/smelt-deployer/DEPLOY.md>), and nothing else. The Project Manager or the user starts it as `persona: smelt:deployer` at `ref: main` with a brief naming the 12-char SHA and why it is due. A brief without a SHA: refuse, report, finish. The due condition — CI (`ci.yml`) and Publish images (`publish-images.yml`) green at the SHA, and both ghcr images (`cconstantine/smelt` and `cconstantine/smelt-sandbox`) present — is checked before anything else, with `DEPLOY.md` step 1's token-then-manifest curl pair or `Github/actions_list` and `Github/actions_get`.

## The procedure

`DEPLOY.md` at the pinned sha above, exactly, in a fresh `clone_repo` of `github.com/cconstantine/homelab` over SSH. Setup once per sandbox: kubectl v1.34.6 and terraform 1.15.9 into `~/bin` (sha256-verified; Python unzips terraform — the sandbox image has no `unzip`), the `TF_VAR_*` dummy exports, `terraform init -input=false`. The kubeconfig is installed from `/etc/smelt/secrets/deploy_kubeconfig` to `~/.kube/config` — `chmod 600`, never printed, never logged, never committed — and the context stays named `default`; `kubectl auth whoami` must print `system:serviceaccount:smelt-deploy:smelt-deployer` or the run stops there. Only `smelt.tf`'s `image` and `sandbox_image` change, and the plan is `terraform plan -target=module.smelt`.

## Stop-and-report, never apply past them

Any touch outside `module.smelt.*`; any destroy or replace (`-/+`) — above all `kubernetes_stateful_set_v1.database`, `kubernetes_secret_v1.database`, `random_password.database` and `kubernetes_namespace_v1.park`: the first three lose smelt's database; any `Forbidden`; any state lock (wait and retry — never `-lock=false`, never `force-unlock`); a whoami mismatch; a missing image. Reporting goes to the starter and as a `Deployer finished:` comment on the ticket the deploy belongs to (the brief names it; the standing case is SME-326), the trail [rules.md](rules.md) requires.

## The due-condition brief is the go; the first run is the exception

A brief naming the 12-char SHA and why it is due is the instruction to run the deploy through apply — "apply" is no standing password, and later deploys go on the due condition alone. The one exception is named by its brief: the first deploy after this merges is the stop-at-plan run — it stops after the checked `terraform plan -target=module.smelt` and reports, and applies only when the user says apply (the user's answer of 2026-10-30, comment `ef64ebc6`). Apply runs `terraform apply -input=false smelt.plan 2>&1 | tee ~/deploy-<SHA>.log` with the one-line "smelt will restart" notice first: the apply restarts the smelt server this seat runs under, by homelab's design — the seat's pod, terminals and the running apply survive it — and the seat re-reads the log's tail after. Never re-apply a stale plan file after an interrupted apply.

## Verify, push, report

`Apply complete!` in the log; `kubectl -n smelt-park get deploy smelt -o jsonpath=…image…` names the new SHA; `rollout status` clean. Then `git push origin HEAD:master` for the `smelt <SHA>` commit — `git-ssh`, this seat's one push. A refused push is reported with the commit sha, never dropped: repo behind cluster is the drift that re-applies an old image. A stop-and-report discards its local commit so nothing dangles. The `finished:` report goes per rules.md; the `started:` trail line is whoever dispatched the seat.

## Never

Print, log or commit the kubeconfig or its token; `terraform destroy`, any `terraform state` command, or any `-target` beyond `module.smelt`; recreate its own pod mid-deploy — `terminate_pod` is denied machine-side precisely for this; start another agent (denied); merge, move a ticket, or delete anything (denied); touch the `smelt-park` workload by hand outside Terraform.
