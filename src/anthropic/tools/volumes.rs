//! Volumes the model creates for its own pod and passes to the agents it
//! starts (SME-239): a claim of its conversation's, mounted at a path of its
//! choosing on top of the `/workspace` claim every conversation already has.
//! The logic is `crate::agents`; the classes are the design table's rows,
//! `create_volume` writing and `delete_volume` destructive.

use super::{Class, Tool};
use crate::anthropic::ToolDefinition;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool {
            def: ToolDefinition {
                name: "create_volume".to_string(),
                description: "Create a volume this conversation owns: a persistent claim mounted at `mount_path` \
                              in your pod and in every agent you pass it to with start_agent(volumes: [name]) — a \
                              child never gets a volume you don't hold yourself. Use it so your agents share one \
                              checkout and one build cache instead of each cloning and rebuilding. `name` is 1-64 \
                              letters, digits, `-` and `_`, and must differ from every volume you mount, your own \
                              and any passed down to you. `mount_path` is absolute (`~` is the sandbox home); it is \
                              refused at or under /workspace, /etc/smelt, /run/docker-sock, /run/docker, \
                              /run/containerd or a system directory (/etc, /usr, /bin, /lib, /lib64, \
                              /sbin, /proc, /sys, /dev, /var/lib/docker), at a global volume's path, or \
                              where one of your other mounts already sits. Your pod's volumes are fixed \
                              when it starts: with a pod running, the volume mounts from your next start \
                              (terminate_pod, then create_pod — /workspace survives), and smelt never restarts the \
                              pod for you. Claims are ReadWriteOnce, so on smelt's single-node k3s every pod that \
                              mounts one runs on that node; a pod's Docker images are shared with nothing. Write for \
                              more than one agent: one working tree each (`git worktree add` off a shared clone, and \
                              `git config gc.auto 0` on the clone so no agent's gc repacks under another's fetch), \
                              and a `CARGO_TARGET_DIR` per agent unless your agents build one after the other — \
                              cargo locks a target directory, so a shared one is safe but serializes builds."
                        .to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "1-64 letters, digits, '-' and '_', different from every volume you mount"
                            },
                            "mount_path": {
                                "type": "string",
                                "description": "absolute path in the pod, e.g. /cache or ~/build-targets; `~` is the sandbox home"
                            }
                        },
                        "required": ["name", "mount_path"]
                    }),
                },
            class: Class::WRITING,
            run: run!(|c| crate::agents::create_volume(c.pool, c.conversation_id, c.input)),
        },
        Tool {
            def: ToolDefinition {
                name: "delete_volume".to_string(),
                description: "Delete a volume this conversation created, with its mount rows and its claim: what \
                              was on it is gone. Only your own — a volume passed to you is deleted by the \
                              conversation that created it. Refused while another conversation that mounts it has \
                              an agent still starting or active: stop or finish that agent first, so no pod is left \
                              mounted to a volume that isn't there. A pod already running keeps what it mounted \
                              until its next start, since Kubernetes holds a claim until its last pod is gone."
                        .to_string(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "name": {"type": "string", "description": "the volume's name, as create_volume gave it"}
                        },
                        "required": ["name"]
                    }),
                },
            class: Class::DESTRUCTIVE,
            run: run!(|c| crate::agents::delete_volume(c.pool, c.conversation_id, c.input)),
        },
    ]
}
