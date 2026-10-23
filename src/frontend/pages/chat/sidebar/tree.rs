//! The sidebar's tree (SME-246): the flat node list the server returns,
//! laid out as rows with each child under its parent.

use std::collections::{HashMap, HashSet};

use crate::api::agents::ConversationNode;

/// Rows deeper than this indent no further, so a deep tree keeps room for
/// its titles on a phone.
pub(super) const MAX_INDENT: usize = 3;

/// One row of the sidebar.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct TreeRow {
    pub node: ConversationNode,
    /// 0 for a root, 1 for its children, and so on.
    pub depth: usize,
    /// Whether it has agents under it (the row gets a caret).
    pub has_children: bool,
    /// Whether its agents are folded away.
    pub collapsed: bool,
    /// How many conversations under it, at any depth, wait for the user:
    /// the mark that finds a waiting agent from its parent, folded or not.
    pub waiting_under: usize,
}

impl TreeRow {
    /// Its indent level, capped at `MAX_INDENT`.
    pub fn indent(&self) -> usize {
        self.depth.min(MAX_INDENT)
    }
}

/// The rows to show: roots in the list's order (most recent first), each
/// followed by its children in the order they started (`id`), depth first.
/// A collapsed node's descendants are left out, but still counted in its
/// `waiting_under`. A node whose parent isn't in the list (a refetch racing
/// a delete) is shown as a root rather than lost.
pub(super) fn build_tree(nodes: &[ConversationNode], collapsed: &HashSet<i64>) -> Vec<TreeRow> {
    let ids: HashSet<i64> = nodes.iter().map(|n| n.id).collect();
    let mut children: HashMap<i64, Vec<&ConversationNode>> = HashMap::new();
    for node in nodes {
        if let Some(parent) = node.parent_id.filter(|p| ids.contains(p) && *p != node.id) {
            children.entry(parent).or_default().push(node);
        }
    }
    for list in children.values_mut() {
        list.sort_by_key(|n| n.id);
    }
    let mut rows = Vec::new();
    // Ids already placed, so a malformed list (a cycle) can't loop.
    let mut placed = HashSet::new();
    for root in nodes.iter().filter(|n| !n.parent_id.is_some_and(|p| ids.contains(&p) && p != n.id)) {
        push_subtree(root, 0, &children, collapsed, &mut placed, &mut rows);
    }
    rows
}

fn push_subtree(
    node: &ConversationNode,
    depth: usize,
    children: &HashMap<i64, Vec<&ConversationNode>>,
    collapsed: &HashSet<i64>,
    placed: &mut HashSet<i64>,
    rows: &mut Vec<TreeRow>,
) {
    if !placed.insert(node.id) {
        return;
    }
    let under = children.get(&node.id).map(Vec::as_slice).unwrap_or_default();
    let is_collapsed = !under.is_empty() && collapsed.contains(&node.id);
    rows.push(TreeRow {
        node: node.clone(),
        depth,
        has_children: !under.is_empty(),
        collapsed: is_collapsed,
        waiting_under: waiting_under(node.id, children, &mut HashSet::new()),
    });
    if !is_collapsed {
        for child in under {
            push_subtree(child, depth + 1, children, collapsed, placed, rows);
        }
    }
}

/// How many of `id`'s descendants wait for the user.
fn waiting_under(id: i64, children: &HashMap<i64, Vec<&ConversationNode>>, seen: &mut HashSet<i64>) -> usize {
    if !seen.insert(id) {
        return 0;
    }
    children
        .get(&id)
        .map(|under| {
            under
                .iter()
                .map(|child| usize::from(child.waiting_for_user) + waiting_under(child.id, children, seen))
                .sum()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i64, parent_id: Option<i64>) -> ConversationNode {
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
        }
    }

    fn waiting(mut n: ConversationNode) -> ConversationNode {
        n.waiting_for_user = true;
        n
    }

    fn shape(rows: &[TreeRow]) -> Vec<(i64, usize)> {
        rows.iter().map(|r| (r.node.id, r.depth)).collect()
    }

    #[test]
    fn test_children_follow_their_parent_in_the_order_they_started() {
        // The server's order: most recently active first, children included.
        let list = vec![node(5, Some(1)), node(9, None), node(3, Some(1)), node(1, None), node(4, Some(3))];
        let rows = build_tree(&list, &HashSet::new());
        assert_eq!(shape(&rows), vec![(9, 0), (1, 0), (3, 1), (4, 2), (5, 1)]);
        let parents: Vec<i64> = rows.iter().filter(|r| r.has_children).map(|r| r.node.id).collect();
        assert_eq!(parents, vec![1, 3]);
    }

    #[test]
    fn test_a_collapsed_parent_hides_its_whole_subtree() {
        let list = vec![node(1, None), node(2, Some(1)), node(3, Some(2)), node(4, None)];
        let rows = build_tree(&list, &HashSet::from([1]));
        assert_eq!(shape(&rows), vec![(1, 0), (4, 0)]);
        assert!(rows[0].collapsed, "the parent shows as collapsed");
        let middle = build_tree(&list, &HashSet::from([2]));
        assert_eq!(shape(&middle), vec![(1, 0), (2, 1), (4, 0)]);
    }

    #[test]
    fn test_collapsing_a_conversation_without_agents_changes_nothing() {
        let list = vec![node(1, None), node(2, Some(1))];
        let rows = build_tree(&list, &HashSet::from([2]));
        assert_eq!(shape(&rows), vec![(1, 0), (2, 1)]);
        assert!(!rows[1].collapsed && !rows[1].has_children);
    }

    #[test]
    fn test_a_waiting_agent_is_counted_on_every_ancestor_through_a_collapsed_level() {
        let list = vec![
            node(1, None),
            node(2, Some(1)),
            waiting(node(3, Some(2))),
            waiting(node(4, Some(1))),
            waiting(node(5, None)),
        ];
        // Folded in the middle: the root still counts the grandchild.
        let rows = build_tree(&list, &HashSet::from([2]));
        let counts: Vec<(i64, usize)> = rows.iter().map(|r| (r.node.id, r.waiting_under)).collect();
        assert_eq!(counts, vec![(1, 2), (2, 1), (4, 0), (5, 0)]);
        // Folded at the root: the count is on the one row left.
        let folded = build_tree(&list, &HashSet::from([1]));
        assert_eq!((folded[0].node.id, folded[0].waiting_under), (1, 2));
    }

    #[test]
    fn test_a_child_whose_parent_is_gone_shows_as_a_root() {
        let list = vec![node(1, None), node(7, Some(6)), node(8, Some(7))];
        let rows = build_tree(&list, &HashSet::new());
        assert_eq!(shape(&rows), vec![(1, 0), (7, 0), (8, 1)]);
    }

    #[test]
    fn test_indentation_stops_at_the_cap() {
        let list: Vec<ConversationNode> = (1..=6).map(|id| node(id, (id > 1).then_some(id - 1))).collect();
        let rows = build_tree(&list, &HashSet::new());
        let indents: Vec<usize> = rows.iter().map(TreeRow::indent).collect();
        assert_eq!(indents, vec![0, 1, 2, 3, 3, 3]);
        assert_eq!(rows[5].depth, 5);
    }

    #[test]
    fn test_a_cycle_in_a_malformed_list_ends() {
        let list = vec![node(1, Some(2)), node(2, Some(1)), node(3, None)];
        let rows = build_tree(&list, &HashSet::new());
        assert_eq!(shape(&rows), vec![(3, 0)], "a cycle has no root to reach it from");
    }
}
