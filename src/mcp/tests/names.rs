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

/// Anthropic refuses a whole request whose any tool name isn't
/// `^[a-zA-Z0-9_-]{1,128}$` (SME-365: `tools.63.custom.name`).
fn anthropic_accepts(name: &str) -> bool {
    (1..=MAX_TOOL_NAME).contains(&name.len()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Every name a server and tool can make is one Anthropic takes: a dot, a
/// space, a slash, non-ASCII, a name too long, an empty one.
#[test]
fn test_every_mcp_tool_name_matches_anthropics_pattern() {
    let long = "x".repeat(200);
    let awkward = ["linear.app", "a b", "ü", "", &long, "github", "a__b"];
    let tools = ["search.web", "files/read", "naïve tool", "", &long, "list_issues", "get-thing"];
    for server in awkward {
        for tool in tools {
            let name = namespaced_tool_name(server, tool);
            assert!(anthropic_accepts(&name), "{server:?} / {tool:?} gave {name:?}");
            assert_eq!(namespaced_tool_name(server, tool), name, "the same tool always gets the same name");
        }
    }
}

/// A name that's already safe is left as it was, so nothing changes for
/// the tools that worked; one that isn't keeps its readable characters.
#[test]
fn test_a_safe_name_is_unchanged_and_an_unsafe_one_stays_readable() {
    assert_eq!(namespaced_tool_name("github", "list_issues"), "mcp__github__list_issues");
    assert_eq!(namespaced_tool_name("my-server", "get-thing_2"), "mcp__my-server__get-thing_2");
    let dotted = namespaced_tool_name("linear.app", "search.web");
    assert!(dotted.starts_with("mcp__linear_app_") && dotted.contains("__search_web_"), "{dotted}");
}

/// Names that differ only in a refused character, or only past the cut,
/// stay apart: each would otherwise reach the other's tool.
#[test]
fn test_names_that_differ_only_where_they_were_changed_stay_apart() {
    let long = "y".repeat(150);
    let pairs = [
        (("s", "a.b"), ("s", "a b")),
        (("s", "a.b"), ("s", "a_b")),
        (("s.1", "t"), ("s 1", "t")),
        (("s", &*format!("{long}1")), ("s", &*format!("{long}2"))),
    ];
    for ((s1, t1), (s2, t2)) in pairs {
        assert_ne!(namespaced_tool_name(s1, t1), namespaced_tool_name(s2, t2), "{s1}/{t1} vs {s2}/{t2}");
    }
}

/// A server row named `name`, at `url`, with an id of its own.
async fn named_server(pool: &sqlx::PgPool, name: &str, url: &str) -> McpServerConfig {
    next_server_id(pool).await;
    crate::db::create_mcp_server_config(pool, name, url, &HashMap::new(), "static_headers", None, None)
        .await
        .expect("create mcp server config")
}

/// The model's safe name finds its server, even when another server's
/// safe part is a prefix of this one's; the tool's part is mapped to its
/// real name by the call (below).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_safe_name_resolves_to_the_server_and_the_tools_real_name(pool: sqlx::PgPool) {
    let dotted = gated_http::spawn("search.web").await;
    let plain = gated_http::spawn("search.web").await;
    let config = named_server(&pool, "linear.app", &dotted.url).await;
    let other = named_server(&pool, "linear", &plain.url).await;
    let joined = named_server(&pool, "a__b", &plain.url).await;
    let short = named_server(&pool, "a", &plain.url).await;

    let dotted_name = namespaced_tool_name("linear.app", "search.web");
    let (found, part) = resolve_tool(&pool, &dotted_name).await.expect("resolve");
    assert_eq!(found.id, config.id);
    assert!(dotted_name.ends_with(&format!("__{part}")), "{dotted_name} / {part}");
    let (found, _) = resolve_tool(&pool, &namespaced_tool_name("linear", "search.web")).await.expect("resolve");
    assert_eq!(found.id, other.id);
    let (found, _) = resolve_tool(&pool, &namespaced_tool_name("a__b", "search.web")).await.expect("resolve");
    assert_eq!(found.id, joined.id, "the longest server name wins");
    let missing = resolve_tool(&pool, "mcp__nope__x").await.expect_err("no such server");
    assert!(missing.contains("nope"), "{missing}");
    for server in [&config, &other, &joined, &short] {
        evict(server.id).await;
    }
}

/// End to end: a call by the safe name reaches the tool the server listed,
/// by the name it listed it under.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_by_the_safe_name_reaches_the_real_tool(pool: sqlx::PgPool) {
    let mock = gated_http::spawn("search.web").await;
    let config = named_server(&pool, "linear.app", &mock.url).await;
    let name = namespaced_tool_name(&config.name, "search.web");
    let called = crate::anthropic::tools::execute(&pool, 1, "toolu_dotted", &name, &serde_json::json!({})).await;
    assert_eq!(called, Ok("called search.web".to_string()));
    evict(config.id).await;
}

/// What the rule check judges is the tool as its server lists it, not the
/// safe name the model called: a deny rule naming `search.web` must catch
/// a call to `search_web_<hash>` (SME-365, SME-241).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_rule_check_is_handed_the_tools_real_name(pool: sqlx::PgPool) {
    let mock = gated_http::spawn("search.web").await;
    let config = named_server(&pool, "linear.app", &mock.url).await;
    let (_, part) = resolve_tool(&pool, &namespaced_tool_name(&config.name, "search.web")).await.expect("resolve");
    assert_ne!(part, "search.web", "the model's part is the safe one");
    let judged = std::sync::Mutex::new(None);
    let refused = call_tool_classed(&pool, &config, &part, serde_json::json!({}), |_, tool| {
        *judged.lock().unwrap_or_else(|e| e.into_inner()) = Some(tool.to_string());
        Err("denied".to_string())
    })
    .await;
    assert_eq!(refused, Err("denied".to_string()));
    assert_eq!(judged.into_inner().unwrap_or_else(|e| e.into_inner()), Some("search.web".to_string()));
    evict(config.id).await;
}

/// One offered tool for the routing tests below.
fn offer(server: &str, tool: &str) -> OfferedTool {
    let schema = Arc::new(serde_json::from_value(serde_json::json!({"type": "object"})).expect("a schema object"));
    OfferedTool { server: server.to_string(), tool: McpTool::new(tool.to_string(), "t", schema), class: crate::anthropic::tools::ToolClass::ReadOnly }
}

/// SME-386 T7: servers `a` and `a__b` can both make `mcp__a__b__c` (`a`'s
/// `b__c`, `a__b`'s `c`), and a call by it reaches `a__b`, the longest
/// server name, then the tool that server lists under it. So an offered
/// tool is kept only when a call by its name reaches it: `a__b`'s `c`
/// stays and `a`'s `b__c` goes. Review 1 (L1): within one server too, a
/// tool listed under another's safe name takes the call, so the other
/// goes, even when the one that takes it isn't offered.
#[test]
fn test_an_offered_tool_a_call_wouldnt_reach_is_left_out() {
    let configs = [test_config(1, "a"), test_config(2, "a__b"), test_config(3, "z")];
    let listed_as = |offered: &[OfferedTool]| {
        let mut listed: HashMap<String, Vec<McpTool>> = HashMap::new();
        for o in offered {
            listed.entry(o.server.clone()).or_default().push(o.tool.clone());
        }
        listed
    };
    let names = |offered: Vec<OfferedTool>, listed: &HashMap<String, Vec<McpTool>>| {
        routable_offers(offered, &configs, listed).iter().map(|o| format!("{}/{}", o.server, o.tool.name)).collect::<Vec<_>>()
    };
    let both = vec![offer("a", "b__c"), offer("a__b", "c"), offer("a", "x")];
    assert_eq!(names(both.clone(), &listed_as(&both)), ["a__b/c", "a/x"], "the one a call reaches stays");
    let alone = vec![offer("a", "b__c"), offer("a", "x")];
    assert_eq!(names(alone.clone(), &listed_as(&alone)), ["a/x"], "a call by `a`'s name would reach `a__b`");
    let plain = vec![offer("a__b", "c"), offer("z", "y")];
    assert_eq!(names(plain.clone(), &listed_as(&plain)), ["a__b/c", "z/y"], "a name that routes back stays");
    // L1: `z` lists `x.y` and a tool literally named after its safe name.
    let safe = namespaced_tool_name("z", "x.y").trim_start_matches("mcp__z__").to_string();
    let listed = listed_as(&[offer("z", "x.y"), offer("z", &safe)]);
    assert_eq!(names(vec![offer("z", "x.y")], &listed), Vec::<String>::new(), "its call reaches the other, unoffered tool");
    assert_eq!(names(vec![offer("z", "x.y"), offer("z", &safe)], &listed), [format!("z/{safe}")], "the exact name takes it");
}
