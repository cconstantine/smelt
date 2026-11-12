//! What a pod's containers ask of their node (SME-484): a persona's
//! `smelt.resources`, the requests a pod spec carries, and the Kubernetes
//! quantities both are written in. No numbers live here: a container with
//! nothing asked of it gets today's `memory_only` (a request of `0` and the
//! deployment's limit).

use super::*;
use serde::{Deserialize, Serialize};

/// One container's asks, every field optional: Kubernetes quantities
/// (`256Mi`, `12Gi`, `500m`). There's no CPU limit to set (SME-77).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_request: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_request: Option<String>,
}

/// A persona's `smelt.resources`: the sandbox container's and the Docker
/// sidecar's, each optional. Stored as JSON on the conversation
/// (`conversations.pod_resources`), which every pod start reads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PodResources {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<ContainerResources>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<ContainerResources>,
}

/// A container's requests in a pod spec: `None` is today's (a memory
/// request of `0`, no CPU request).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requests {
    pub memory: Option<String>,
    pub cpu: Option<String>,
}

/// Both containers' requests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PodRequests {
    pub sandbox: Requests,
    pub docker: Requests,
}

/// Binary and decimal suffixes of a Kubernetes quantity, as multipliers.
const SUFFIXES: [(&str, f64); 12] = [
    ("Ki", 1024.0),
    ("Mi", 1024.0 * 1024.0),
    ("Gi", 1024.0 * 1024.0 * 1024.0),
    ("Ti", 1024.0 * 1024.0 * 1024.0 * 1024.0),
    ("Pi", 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0),
    ("Ei", 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0),
    ("k", 1e3),
    ("M", 1e6),
    ("G", 1e9),
    ("T", 1e12),
    ("P", 1e15),
    ("E", 1e18),
];

/// A plain non-negative decimal: digits, with at most one `.` among them.
fn decimal(number: &str) -> Option<f64> {
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let digits = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) || (number.contains('.') && fraction.is_empty()) {
        return None;
    }
    number.parse().ok()
}

/// A memory quantity in bytes: a non-negative decimal with an optional
/// binary (`Ki` … `Ei`) or decimal (`k` … `E`) suffix. Exponents and
/// milli-bytes are refused: nobody means them for memory.
pub fn memory_bytes(quantity: &str) -> Option<f64> {
    let (number, multiplier) = SUFFIXES
        .iter()
        .find_map(|(suffix, multiplier)| quantity.strip_suffix(suffix).map(|number| (number, *multiplier)))
        .unwrap_or((quantity, 1.0));
    decimal(number).map(|n| n * multiplier)
}

/// A CPU quantity in millicores: a decimal of cores, or whole millicores
/// with `m`.
pub fn cpu_millis(quantity: &str) -> Option<f64> {
    match quantity.strip_suffix('m') {
        Some(millis) if !millis.contains('.') => decimal(millis),
        Some(_) => None,
        None => decimal(quantity).map(|cores| cores * 1000.0),
    }
}

impl ContainerResources {
    /// Each field a quantity of its kind, and the memory request no more
    /// than `limit` (the container's own, else `default_limit`). Errors name
    /// the field as `<at>.<field>`.
    pub fn check(&self, at: &str, default_limit: &str) -> Result<(), String> {
        let memory = [("memory_request", &self.memory_request), ("memory_limit", &self.memory_limit)];
        for (field, value) in memory {
            if let Some(value) = value.as_deref().filter(|v| memory_bytes(v).is_none()) {
                return Err(format!(
                    "`{at}.{field}` is {value:?}, which isn't a memory quantity: a number with Ki, Mi, Gi or Ti (or k, M, G, T), as in 512Mi or 12Gi"
                ));
            }
        }
        if let Some(value) = self.cpu_request.as_deref().filter(|v| cpu_millis(v).is_none()) {
            return Err(format!(
                "`{at}.cpu_request` is {value:?}, which isn't a CPU quantity: cores (1, 0.5) or millicores (500m)"
            ));
        }
        let Some(request) = self.memory_request.as_deref() else {
            return Ok(());
        };
        let (limit, named) = match self.memory_limit.as_deref() {
            Some(own) => (own, format!("`{at}.memory_limit`")),
            None => (default_limit, format!("this deployment's limit ({})", deployment_limit_name(at))),
        };
        match (memory_bytes(request), memory_bytes(limit)) {
            (Some(asked), Some(cap)) if asked > cap => Err(format!(
                "`{at}.memory_request` {request} is above {named}, {limit}: a request can't be more than its limit"
            )),
            _ => Ok(()),
        }
    }

    /// The container's memory limit: its own, else `default_limit`.
    pub fn limit_or(&self, default_limit: &str) -> String {
        self.memory_limit.clone().unwrap_or_else(|| default_limit.to_string())
    }

    /// Its requests, for the pod spec.
    pub fn requests(&self) -> Requests {
        Requests { memory: self.memory_request.clone(), cpu: self.cpu_request.clone() }
    }
}

impl PodResources {
    /// Both containers checked (`ContainerResources::check`) against the
    /// deployment's limits; errors name `smelt.resources.<container>.<field>`.
    pub fn check(&self, sandbox_limit: &str, docker_limit: &str) -> Result<(), String> {
        if let Some(sandbox) = &self.sandbox {
            sandbox.check("smelt.resources.sandbox", sandbox_limit)?;
        }
        if let Some(docker) = &self.docker {
            docker.check("smelt.resources.docker", docker_limit)?;
        }
        Ok(())
    }

    /// `check` against this deployment's own limits (`SANDBOX_MEMORY_LIMIT`,
    /// `SANDBOX_DOCKER_MEMORY_LIMIT`), as a persona file is read.
    pub fn check_for_deployment(&self) -> Result<(), String> {
        self.check(&super::limits::default_memory_limit(), &super::limits::default_docker_memory_limit())
    }
}

/// A container's resources in the pod spec: `memory_only(limit)` when it
/// asks for nothing, byte for byte; otherwise the same limit with its
/// memory request (`0` when it names none) and its CPU request, if any.
pub(in crate::sandbox) fn container_resources(limit: &str, requests: &Requests) -> ResourceRequirements {
    let mut resources = memory_only(limit);
    if let Some(asked) = resources.requests.as_mut() {
        if let Some(memory) = &requests.memory {
            asked.insert("memory".to_string(), Quantity(memory.clone()));
        }
        if let Some(cpu) = &requests.cpu {
            asked.insert("cpu".to_string(), Quantity(cpu.clone()));
        }
    }
    resources
}

/// The deployment's variable a container's default limit comes from.
fn deployment_limit_name(at: &str) -> &'static str {
    if at.ends_with("docker") {
        "SANDBOX_DOCKER_MEMORY_LIMIT"
    } else {
        "SANDBOX_MEMORY_LIMIT"
    }
}
