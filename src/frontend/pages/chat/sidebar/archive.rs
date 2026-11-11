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

/// The nodes the sidebar lays out: every tree kept whole or dropped whole.
pub(super) fn visible(nodes: &[ConversationNode], open: Option<i64>, show_archived: bool) -> Vec<ConversationNode> {
    if show_archived {
        return nodes.to_vec();
    }
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
    nodes.iter().filter(|n| keep[&roots[&n.id]]).cloned().collect()
}

/// How many archived top-level conversations the list has, for the
/// toggle's label.
pub(super) fn archived_roots(nodes: &[ConversationNode]) -> usize {
    let roots = roots(nodes);
    nodes.iter().filter(|n| roots[&n.id] == n.id && n.archived_at.is_some()).count()
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
        assert_eq!(archived_roots(&list()), 1);
        let mut two = list();
        two.push(node(6, None, true));
        assert_eq!(archived_roots(&two), 2);
        assert_eq!(archived_roots(&visible(&two, None, false)), 0);
    }
}
