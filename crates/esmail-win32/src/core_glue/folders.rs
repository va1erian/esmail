//! The account > folder tree the left pane shows, as plain data.
//!
//! Built from what IMAP `LIST` and `STATUS (UNSEEN)` report, through
//! `esmail::imap::mailbox_tree`, so folder order and nesting match the egui
//! frontend. The widget adapter turns [`Node`]s into native tree items.

use std::collections::HashMap;

use esmail::imap::{MailboxInfo, MailboxRow, flatten_tree, mailbox_tree};

/// Identifies a tree node across the widget boundary: the account's index in
/// the high half, and the folder's row + 1 in the low half (0 for the account
/// node itself).
pub type NodeId = i64;

const FOLDER_BITS: u32 = 32;

/// A tree node to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// What to display, with the unread count when there is one.
    pub text: String,
    /// The id to hand back through [`FolderTree::selection`].
    pub id: NodeId,
    /// Whether the node has children.
    pub has_children: bool,
}

/// A folder the user picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRef {
    /// Index of the account in the order the tree was built with.
    pub account: usize,
    /// The folder's full IMAP name.
    pub mailbox: String,
}

/// One account's folders and unread counts.
#[derive(Debug, Default, Clone)]
struct AccountFolders {
    label: String,
    rows: Vec<MailboxRow>,
    unread: HashMap<String, u32>,
}

/// Every account with the folders learned so far.
#[derive(Debug, Default, Clone)]
pub struct FolderTree {
    accounts: Vec<AccountFolders>,
}

impl FolderTree {
    /// A tree with one (still empty) node per account label.
    pub fn new(labels: impl IntoIterator<Item = String>) -> Self {
        let accounts = labels.into_iter().map(|label| AccountFolders { label, ..Default::default() }).collect();
        Self { accounts }
    }

    /// Replaces `account`'s folders with a fresh `LIST` result.
    pub fn set_mailboxes(&mut self, account: usize, mailboxes: &[MailboxInfo]) {
        if let Some(entry) = self.accounts.get_mut(account) {
            entry.rows = flatten_tree(&mailbox_tree(mailboxes));
        }
    }

    /// Replaces `account`'s unread counts with a fresh `STATUS` batch.
    pub fn set_unread(&mut self, account: usize, unread: HashMap<String, u32>) {
        if let Some(entry) = self.accounts.get_mut(account) {
            entry.unread = unread;
        }
    }

    /// Whether `account`'s folders have arrived.
    pub fn has_folders(&self, account: usize) -> bool {
        self.accounts.get(account).is_some_and(|a| !a.rows.is_empty())
    }

    /// The nodes under `parent`, or the accounts when `parent` is `None`.
    pub fn children(&self, parent: Option<NodeId>) -> Vec<Node> {
        let Some(parent) = parent else {
            return (0..self.accounts.len()).map(|account| self.account_node(account)).collect();
        };
        let (account, slot) = split_id(parent);
        let Some(entry) = self.accounts.get(account) else { return Vec::new() };
        let (start, depth) = match slot {
            0 => (0, 0),
            slot => match entry.rows.get(slot - 1) {
                Some(row) => (slot, row.depth + 1),
                None => return Vec::new(),
            },
        };
        entry.rows[start..]
            .iter()
            .enumerate()
            .take_while(|(_, row)| row.depth >= depth)
            .filter(|(_, row)| row.depth == depth)
            .map(|(offset, row)| Node {
                text: entry.folder_text(row),
                id: make_id(account, start + offset + 1),
                has_children: row.has_children,
            })
            .collect()
    }

    /// The folder a node stands for, or `None` for an account node or a
    /// container folder that cannot be opened.
    pub fn selection(&self, id: NodeId) -> Option<FolderRef> {
        let (account, slot) = split_id(id);
        let row = self.accounts.get(account)?.rows.get(slot.checked_sub(1)?)?;
        Some(FolderRef { account, mailbox: row.full_name.clone()? })
    }

    /// The unread count for a folder, if `STATUS` reported one.
    pub fn unread(&self, folder: &FolderRef) -> Option<u32> {
        self.accounts.get(folder.account)?.unread.get(&folder.mailbox).copied()
    }

    /// Every selectable folder name of `account`, for a `STATUS` batch.
    pub fn mailbox_names(&self, account: usize) -> Vec<String> {
        self.accounts.get(account).map_or_else(Vec::new, |a| a.rows.iter().filter_map(|r| r.full_name.clone()).collect())
    }

    fn account_node(&self, account: usize) -> Node {
        Node { text: self.accounts[account].label.clone(), id: make_id(account, 0), has_children: true }
    }
}

impl AccountFolders {
    fn folder_text(&self, row: &MailboxRow) -> String {
        match row.full_name.as_ref().and_then(|name| self.unread.get(name)) {
            Some(&count) if count > 0 => format!("{} ({count})", row.label),
            _ => row.label.clone(),
        }
    }
}

fn make_id(account: usize, slot: usize) -> NodeId {
    ((account as i64) << FOLDER_BITS) | slot as i64
}

fn split_id(id: NodeId) -> (usize, usize) {
    ((id >> FOLDER_BITS) as usize, (id & ((1 << FOLDER_BITS) - 1)) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use esmail::imap::SpecialUse;

    fn mailbox(name: &str, noselect: bool) -> MailboxInfo {
        let special_use = (name == "INBOX").then_some(SpecialUse::Inbox);
        MailboxInfo { name: name.into(), delimiter: Some("/".into()), special_use, noselect }
    }

    fn tree() -> FolderTree {
        let mut tree = FolderTree::new(["Work".to_string(), "Home".to_string()]);
        tree.set_mailboxes(
            0,
            &[mailbox("INBOX", false), mailbox("Projects", false), mailbox("Projects/Alpha", false), mailbox("Projects/Beta", false), mailbox("[Gmail]", true)],
        );
        tree
    }

    fn texts(nodes: &[Node]) -> Vec<&str> {
        nodes.iter().map(|n| n.text.as_str()).collect()
    }

    #[test]
    fn the_roots_are_the_accounts() {
        let roots = tree().children(None);
        assert_eq!(texts(&roots), ["Work", "Home"]);
        assert!(roots.iter().all(|n| n.has_children));
    }

    #[test]
    fn an_account_lists_its_top_level_folders_inbox_first() {
        let tree = tree();
        let account = tree.children(None)[0].id;
        let folders = tree.children(Some(account));
        assert_eq!(texts(&folders), ["INBOX", "[Gmail]", "Projects"]);
        assert_eq!(folders.iter().map(|n| n.has_children).collect::<Vec<_>>(), [false, false, true]);
    }

    #[test]
    fn a_folder_lists_only_its_direct_children() {
        let tree = tree();
        let account = tree.children(None)[0].id;
        let projects = tree.children(Some(account))[2].id;
        assert_eq!(texts(&tree.children(Some(projects))), ["Alpha", "Beta"]);
    }

    #[test]
    fn an_account_without_folders_yet_has_no_children() {
        let tree = tree();
        let home = tree.children(None)[1].id;
        assert!(tree.children(Some(home)).is_empty());
        assert!(!tree.has_folders(1));
        assert!(tree.has_folders(0));
    }

    #[test]
    fn selecting_a_folder_names_its_account_and_full_imap_name() {
        let tree = tree();
        let account = tree.children(None)[0].id;
        let projects = tree.children(Some(account))[2].id;
        let alpha = tree.children(Some(projects))[0].id;
        assert_eq!(tree.selection(alpha), Some(FolderRef { account: 0, mailbox: "Projects/Alpha".into() }));
    }

    #[test]
    fn accounts_and_noselect_containers_are_not_selectable() {
        let tree = tree();
        let account = tree.children(None)[0].id;
        assert_eq!(tree.selection(account), None);
        let gmail = tree.children(Some(account))[1].id;
        assert_eq!(tree.selection(gmail), None);
    }

    #[test]
    fn unread_counts_show_in_the_label_only_when_positive() {
        let mut tree = tree();
        tree.set_unread(0, HashMap::from([("INBOX".to_string(), 3), ("Projects".to_string(), 0)]));
        let account = tree.children(None)[0].id;
        assert_eq!(texts(&tree.children(Some(account))), ["INBOX (3)", "[Gmail]", "Projects"]);
        let inbox = FolderRef { account: 0, mailbox: "INBOX".into() };
        assert_eq!(tree.unread(&inbox), Some(3));
    }

    #[test]
    fn status_is_requested_for_selectable_folders_only() {
        assert_eq!(tree().mailbox_names(0), ["INBOX", "Projects", "Projects/Alpha", "Projects/Beta"]);
    }
}
