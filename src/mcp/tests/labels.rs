//! Every offered MCP tool's description leads with its server (SME-309):
//! a hundred-tool list says at the choice point which server each tool
//! belongs to, so a seat choosing by meaning can't take Linear's
//! `save_comment` for smelt's `message_agent` unawares.
//!
//! The fixtures are the *deployed* tool lists — each (name, description)
//! pair copied verbatim from the tool list offered to a seat on
//! 2026-10-28 (the servers configured on the deployed instance:
//! `linear.app`, `Github`, `exa`), following the `OFFERED` precedent in
//! `text_calls`' tests. They are not made-up descriptions.

use super::*;

/// The Linear issue tracker's tools, as `linear.app` is deployed to offer
/// them. `save_comment` leads because it is the pair this ticket was
/// filed on: `message_agent` and it read the same to a seat.
const LINEAR: &[(&str, &str)] = &[
    ("save_comment", r"Create or update a comment on a Linear issue, project, initiative, document, project milestone, or project/initiative status update. If `id` is provided, updates the existing comment; otherwise creates a new one. To start a new thread, pass `body` and exactly one of `issueId`, `projectId`, `initiativeId`, `documentId`, `milestoneId`, or `statusUpdateId` (`statusUpdateType` applies only to `statusUpdateId`; omit it for every other parent) — comments on issues/projects/initiatives become top-level discussion threads; comments on documents/milestones become description comments. A status update holds a single thread, so a comment on an update that already has one is added to it as a reply. To reply to an existing thread, pass `parentId` and `body`; the reply inherits the parent's thread type, so no entity reference is needed. Parent reference fields are ignored when `id` or `parentId` is provided (`statusUpdateType` is rejected instead)."),
    ("list_comments", r"List comments on a Linear issue, project, initiative, document, project milestone, or project/initiative status update. Provide exactly one of `issueId`, `projectId`, `initiativeId`, `documentId`, `milestoneId`, or `statusUpdateId`. `statusUpdateType` applies only to `statusUpdateId`; omit it for every other parent. For issues, projects, and initiatives this returns both top-level discussion threads and inline description comments. Inline (anchored) comments carry a non-null `quotedText` set to the snippet of description text they reference. Returned comments include `author` and nullable `onBehalfOf`; when set, `onBehalfOf` identifies the user the agent author acted for. Files uploaded in comment bodies are returned in `attachments` with titles and download URLs, not issue attachment IDs. Signed upload URLs expire after five minutes; call list_comments again to refresh them. Download documents using these URLs and process them with your own file tools."),
    ("get_issue", r"Retrieve detailed information about an issue by ID, including attachments, git branch name, and active Triage Intelligence suggestions when the issue is in triage. A nonempty fields selection overrides the include options; request optional fields directly in fields."),
    ("list_issues", r#"List issues in the user's Linear workspace, including active Triage Intelligence suggestions for issues in triage. For issues assigned to me, use "me" as the assignee. For issues created by me, use "me" as the creator. Use "null" for no assignee."#),
    ("save_issue", r#"Create or update a Linear issue. If `id` is provided, updates the existing issue; otherwise creates a new one. When creating, `team` is required, and `title` is required unless `template` is set. Note: use `assignee` (not `assigneeId`) to set the assignee — it accepts a user ID, name, email, or "me"."#),
    ("save_document", r"Create or update a Linear document. If `id` is provided, edits the existing document; otherwise creates a new one. When creating, `title` is required and exactly one parent (`project`, `issue`, `initiative`, `cycle`, or `team`) must be specified. On update, passing a parent reparents the document. To change parts of the content without resending all of it, pass `patch` instead of `content`."),
    ("list_teams", r"List teams in the user's Linear workspace, including their parent IDs and hierarchy names"),
    ("get_team", r"Retrieve details of a specific Linear team, including its parent ID and hierarchy name. Use list_teams with parentTeam or ancestorTeam to find its sub-teams"),
    ("list_users", r"List workspace users with their IDs, names, emails, and admin, guest, and active flags. Use get_user for team memberships."),
    ("save_project", r"Create or update a Linear project. If `id` is provided, updates the existing project; otherwise creates a new one. When creating, `name` and at least one team (via `addTeams` or `setTeams`) are required. Pass `template` to create the project from a project template. To change parts of the description without resending all of it, pass `patch` instead of `description`. `state` resolves against the lead team's project statuses, so pass `leadTeam` when creating a multi-team project whose teams use different statuses."),
    ("get_diff", r"Exact lookup for a Linear diff. Use with review URLs, GitHub PR URLs, Linear full identifiers, UUIDs, or slugs."),
    ("get_notifications", r"Get a page of notifications from the authenticated user's Linear inbox."),
];

/// The GitHub server's tools, as `Github` (the deployed name — capital G)
/// offers them.
const GITHUB: &[(&str, &str)] = &[
    ("add_issue_comment", r"Add a comment and/or reaction to a specific issue or issue comment in a GitHub repository. Use this tool with pull requests as well (in this case pass pull request number as issue_number), but only if user is not asking specifically to add or react to review comments. At least one of body or reaction is required."),
    ("create_pull_request", r"Create a new pull request in a GitHub repository."),
    ("update_pull_request", r"Update an existing pull request in a GitHub repository."),
    ("merge_pull_request", r"Merge a pull request in a GitHub repository."),
    ("get_file_contents", r"Get the contents of a file or directory from a GitHub repository"),
    ("get_me", r"Get details of the authenticated GitHub user. Use this when a request is about the user's own profile for GitHub. Or when information is missing to build other tool calls."),
    ("pull_request_read", r"Get information on a specific pull request in GitHub repository."),
];

/// The Exa server's one tool, as keyless `exa` offers it — the description
/// with its real newlines, verbatim.
const EXA: &[(&str, &str)] = &[(
    "web_search_exa",
    "Search the web for any topic and get clean, ready-to-use content.\n\n      Best for: Finding current information, news, facts, people, companies, or answering questions about any topic.\n      Returns: Clean text content from top search results.\n\n      Query tips:\n      describe the ideal page, not keywords. \"blog post comparing React and Vue performance\" not \"React vs Vue\".\n      Use category:people / category:company to search through Linkedin profiles / companies respectively.\n      If the highlights are insufficient, follow up with web_fetch_exa on the best URLs.",
)];

/// The three deployed servers as configured on `/mcp-servers`.
pub(super) const SERVERS: &[&[(&str, &str)]] = &[LINEAR, GITHUB, EXA];

pub(super) const SERVER_NAMES: &[&str] = &["linear.app", "Github", "exa"];

/// A — every deployed MCP tool arrives labelled: run each real entry
/// through `labelled_description` with its server, and what the model
/// reads starts with that server's label.
#[test]
fn test_every_deployed_tool_arrives_labelled() {
    for (server, entries) in SERVER_NAMES.iter().zip(SERVERS) {
        for (name, description) in *entries {
            let labelled = labelled_description(server, description);
            assert!(
                labelled.starts_with(&format!("[{server}] ")),
                "{server}::{name} arrives without its label: {labelled:?}"
            );
            assert!(
                labelled.ends_with(*description),
                "{server}::{name} lost or changed its own text: {labelled:?}"
            );
        }
    }
}

/// B — no double label: a description that already leads with its
/// server's own name is left as the server wrote it, case-insensitively
/// (the deployed GitHub server is configured as `Github`); one that
/// merely *mentions* its server mid-text still gets the label.
#[test]
fn test_a_description_that_leads_with_its_server_is_not_labelled_twice() {
    // The configured name leading its own description.
    let already = "linear.app: create and read issues, comments and projects";
    assert_eq!(labelled_description("linear.app", already), already);
    // The human-name server: the ticket's literal example, "Linear issue
    // tracker: …" — the server's configured name is `Linear`.
    let human = "Linear issue tracker: track issues and project work.";
    assert_eq!(labelled_description("Linear", human), human);
    // Case-insensitive, because the deployed name is `Github`.
    let lower = "github: repos, issues and pull requests.";
    assert_eq!(labelled_description("Github", lower), lower);
    // A mention mid-text is not a lead-in — the label still goes on.
    let mention = "Create a pull request in a GitHub repository.";
    assert_eq!(
        labelled_description("Github", mention),
        format!("[Github] {mention}")
    );
    // And an exact-name lead-in only counts at the start.
    let tail = "List issues. Provided by linear.app.";
    assert_eq!(
        labelled_description("linear.app", tail),
        format!("[linear.app] {tail}")
    );
}

/// G (SME-309): the live capture — smelt's own client connects to the
/// keyless Exa server, and the purpose `prompt_servers` reports is what
/// Exa said it is at `initialize`, on one sanitized line. Same gate as
/// the live search test: `SMELT_LIVE_EXA=1 scripts/cluster-test --features
/// server -- --ignored --exact
/// mcp::tests::labels::test_live_exa_purpose_arrives_from_its_initialize_handshake`
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
#[ignore]
async fn test_live_exa_purpose_arrives_from_its_initialize_handshake(pool: sqlx::PgPool) {
    if std::env::var("SMELT_LIVE_EXA").as_deref() != Ok("1") {
        eprintln!("skipped: set SMELT_LIVE_EXA=1 to run against the real Exa service");
        return;
    }
    let config = create_static_server(&pool, EXA_MCP_URL).await;
    connection_check(&pool, &config)
        .await
        .expect("smelt's MCP client should connect to Exa keylessly");
    let servers = prompt_servers(&pool).await;
    let exa = servers
        .iter()
        .find(|s| s.name == config.name)
        .expect("the connected server is listed");
    let purpose = exa
        .purpose
        .as_deref()
        .expect("Exa sends a serverInfo description at initialize");
    assert!(!purpose.is_empty(), "purpose line is empty");
    assert!(!purpose.contains('\n') && !purpose.contains('\u{7f}'), "not one clean line: {purpose:?}");
    evict(config.id).await;
    assert!(
        prompt_servers(&pool)
            .await
            .iter()
            .find(|s| s.name == config.name)
            .expect("still listed")
            .purpose
            .is_none(),
        "evict drops the purpose, §8's state model"
    );
}

/// F — Done-when's combined bullet over the assembled real lists: every
/// offered description says who owns it (an MCP tool leads `[server] `,
/// or is skipped only by the no-double-label rule), the four smelt
/// natives lead `smelt`, and the base prompt names the confusions.
#[test]
fn test_the_assembled_offer_says_who_owns_everything() {
    // MCP: every deployed description leaves the composition point owned.
    for (server, entries) in SERVER_NAMES.iter().zip(SERVERS) {
        for (name, description) in *entries {
            let labelled = labelled_description(server, description);
            let owned = labelled.starts_with(&format!("[{server}] "))
                || description
                    .to_lowercase()
                    .starts_with(&server.to_lowercase());
            assert!(owned, "{server}::{name} offers no owner: {labelled:?}");
        }
    }
    // smelt's own four: label, and the agent-message one rules out the tracker.
    let confused = ["message_agent", "finish", "ask_parent", "ask_user"];
    let natives = crate::anthropic::tools::native_tool_definitions();
    for name in confused {
        let def = natives
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} is offered"));
        assert!(def.description.starts_with("smelt"), "{name}: {:?}", def.description);
    }
    // The prose half of the same promise, from §4.3.
    assert!(
        crate::turn::BASE_SYSTEM_PROMPT
            .contains("A comment tool on an MCP server writes a record on a ticket"),
        "the base prompt must name the comment-not-a-message confusion"
    );
}
