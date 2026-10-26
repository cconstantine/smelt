//! A server's name as a rule can reach it (SME-241): a name holding `/`
//! or `*` can't be named by a `smelt.tools` pattern, so such a name is
//! refused going forward, and a row that predates the rule is reported
//! rather than broken.

use super::*;

/// The two characters a pattern's server part can't hold: `/` is the
/// separator itself, and `*` would be a wildcard where the grammar
/// insists on one named server. The message names both rules and the name
/// it refused.
#[test]
fn test_a_server_name_no_pattern_could_name_is_refused_naming_both_rules() {
    for name in ["a/b", "a*b", "*/x", "prod/deleted"] {
        let error = check_server_name(name).expect_err("{name} can't be named by a tool rule");
        assert!(error.contains('/') && error.contains('*'), "{name}: {error}");
        assert!(error.contains(name), "{name}: {error} names the name it refused");
    }
    for name in ["github", "linear.app", "smelt", "a b", "c-d_e"] {
        check_server_name(name).expect("{name} is a name a rule can use");
    }
}
