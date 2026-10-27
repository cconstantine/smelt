//! `create_pod`'s limit overrides.

use super::*;

#[test]
fn test_create_pod_reads_both_memory_overrides_and_ignores_cpu() {
    let input = serde_json::json!({
        "memory_limit": "4Gi",
        "cpu_limit": "2",
        "docker_memory_limit": "6Gi",
        "docker_cpu_limit": "3",
    });
    let overrides = pod_limit_overrides(&input);
    assert_eq!(overrides.memory.as_deref(), Some("4Gi"));
    assert_eq!(overrides.docker_memory.as_deref(), Some("6Gi"));

    let none = pod_limit_overrides(&serde_json::json!({}));
    assert!(none.memory.is_none() && none.docker_memory.is_none());
}

