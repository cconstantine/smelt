//! Which trees the sidebar lists (SME-480): an archived top-level
//! conversation's tree is hidden unless "Show archived" is on, something
//! in it is owed to the user (D1: a question or trust card waiting, or a
//! report owed — never hidden), or it holds the open conversation (D7).

use std::collections::HashMap;

use crate::api::agents::ConversationNode;

/// Each node's root: the top of its `parent_id` chain within `nodes`. A
/// node whose parent isn't listed counts as a root, as `build_tree` does.
fn roots(nodes: &[ConversationNode]) -> HashMap<i64, i64> {
    let parents: HashMap<i64, Option<i64>> = nodes.iter().map(|n| (n.id, n.parent_id)).collect();
    nodes
        .iter()
        .map(|n| {
            let mut at = n.id;
            let mut steps = 0;
            while let Some(Some(parent)) = parents.get(&at)
                && parents.contains_key(parent)
                && steps < nodes.len()
            {
                at = *parent;
                steps += 1;
            }
            (n.id, at)
        })
        .collect()
}

/// Whether a node owes the user something: D1's marks.
fn owed(node: &ConversationNode) -> bool {
    node.waiting_for_user || node.report_owed_at.is_some()
}

/// Each node's root, and whether each root's tree is listed with the
/// toggle off: in use, owed something (D1), or holding `open` (D7).
fn kept(nodes: &[ConversationNode], open: Option<i64>) -> (HashMap<i64, i64>, HashMap<i64, bool>) {
    let roots = roots(nodes);
    let by_id: HashMap<i64, &ConversationNode> = nodes.iter().map(|n| (n.id, n)).collect();
    let mut keep: HashMap<i64, bool> = HashMap::new();
    for node in nodes {
        let root = roots[&node.id];
        let entry = keep.entry(root).or_insert_with(|| by_id[&root].archived_at.is_none());
        if owed(node) || open == Some(node.id) {
            *entry = true;
        }
    }
    (roots, keep)
}

/// The nodes the sidebar lays out: every tree kept whole or dropped whole.
pub(super) fn visible(nodes: &[ConversationNode], open: Option<i64>, show_archived: bool) -> Vec<ConversationNode> {
    if show_archived {
        return nodes.to_vec();
    }
    let (roots, keep) = kept(nodes, open);
    nodes.iter().filter(|n| keep[&roots[&n.id]]).cloned().collect()
}

/// How many archived trees the toggle would add, for its label: the
/// archived roots the filter hides with the toggle off, not those already
/// listed for something owed (D1) or the open conversation (D7) (review 1,
/// L2: "Show archived (1)" over a tree already shown added nothing).
pub(super) fn hidden_archived(nodes: &[ConversationNode], open: Option<i64>) -> usize {
    let (roots, keep) = kept(nodes, open);
    nodes.iter().filter(|n| roots[&n.id] == n.id && !keep[&n.id]).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i64, parent_id: Option<i64>, archived: bool) -> ConversationNode {
        let at = chrono::NaiveDateTime::default();
        ConversationNode {
            id,
            title: format!("c{id}"),
            created_at: at,
            updated_at: at,
            parent_id,
            persona: None,
            status: None,
            busy: false,
            waiting_for_user: false,
            report_owed_at: None,
            archived_at: archived.then_some(at),
        }
    }

    /// Root 1 (archived) with agent 2 and grandchild 3; root 4 in use with
    /// agent 5. The tree reports each node's root's mark.
    fn list() -> Vec<ConversationNode> {
        vec![node(1, None, true), node(2, Some(1), true), node(3, Some(2), true), node(4, None, false), node(5, Some(4), false)]
    }

    fn ids(nodes: &[ConversationNode]) -> Vec<i64> {
        nodes.iter().map(|n| n.id).collect()
    }

    /// T5: an archived tree is dropped whole; one in use is kept; the
    /// toggle keeps everything.
    #[test]
    fn test_an_archived_tree_is_dropped_whole_unless_shown() {
        assert_eq!(ids(&visible(&list(), None, false)), vec![4, 5]);
        assert_eq!(ids(&visible(&list(), None, true)), vec![1, 2, 3, 4, 5]);
        // Busy isn't owed (D2): a working archived tree stays hidden.
        let mut busy = list();
        busy[2].busy = true;
        assert_eq!(ids(&visible(&busy, None, false)), vec![4, 5]);
    }

    /// T6 (D1): anything owed anywhere in an archived tree keeps it whole.
    #[test]
    fn test_an_archived_tree_owing_the_user_anything_stays_listed() {
        let mut waiting = list();
        waiting[2].waiting_for_user = true;
        assert_eq!(ids(&visible(&waiting, None, false)), vec![1, 2, 3, 4, 5], "a grandchild's question");
        let mut owed_report = list();
        owed_report[2].report_owed_at = Some(chrono::NaiveDateTime::default());
        assert_eq!(ids(&visible(&owed_report, None, false)), vec![1, 2, 3, 4, 5], "a grandchild's owed report");
    }

    /// T7 (D7): the open conversation's tree stays listed; the count is of
    /// archived roots only.
    #[test]
    fn test_the_open_tree_stays_listed_and_the_count_is_of_roots() {
        assert_eq!(ids(&visible(&list(), Some(3), false)), vec![1, 2, 3, 4, 5]);
        assert_eq!(ids(&visible(&list(), Some(5), false)), vec![4, 5]);
        assert_eq!(hidden_archived(&list(), None), 1);
        let mut two = list();
        two.push(node(6, None, true));
        assert_eq!(hidden_archived(&two, None), 2);
    }

    /// Review 1, L2: the toggle counts only the archived trees it would
    /// add. One kept for an owed question (D1) and one for the open
    /// conversation (D7) aren't counted; the one hidden is.
    #[test]
    fn test_the_toggle_counts_only_the_archived_trees_it_would_add() {
        let mut nodes = list();
        nodes[2].waiting_for_user = true;
        nodes.push(node(6, None, true));
        assert_eq!(hidden_archived(&nodes, None), 1, "the owed tree is already listed");
        nodes.push(node(7, None, true));
        assert_eq!(hidden_archived(&nodes, Some(7)), 1, "the open tree is already listed");
        nodes[2].waiting_for_user = false;
        assert_eq!(hidden_archived(&nodes, Some(7)), 2, "once nothing is owed, tree 1 is hidden again");
    }
}
