//! The account > folder tree the left pane shows, as plain data.
//!
//! Built from what IMAP `LIST` and `STATUS (UNSEEN)` report, through
//! `esmail::imap::mailbox_tree`, so folder order and nesting match the egui
//! frontend. The widget adapter turns [`Node`]s into native tree items.

use std::collections::HashMap;

use esmail::imap::{MailboxInfo, MailboxRow, SpecialUse, flatten_tree, mailbox_tree};
use esmail::view_model::find_special_use_mailbox;

/// Identifies a tree node across the widget boundary: the account's index in
/// the high half, and a hash of the folder's path in the low half (0 for the
/// account node itself). Hashing the path rather than numbering the rows keeps
/// a folder's id when another folder appears before it, so the native tree can
/// be updated in place.
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

    /// Shows the folders the local cache knows for `account`, as a stand-in until
    /// the server's `LIST` arrives. Ignored once real folders are known. The
    /// cache stores names only, so nested folders appear flat and no
    /// special-use roles are known (except `INBOX`, which is special by name).
    pub fn set_cached_mailboxes(&mut self, account: usize, names: &[String]) {
        if self.has_folders(account) {
            return;
        }
        let mailboxes: Vec<MailboxInfo> = names
            .iter()
            .map(|name| MailboxInfo {
                name: name.clone(),
                delimiter: None,
                special_use: name.eq_ignore_ascii_case("INBOX").then_some(SpecialUse::Inbox),
                noselect: false,
            })
            .collect();
        self.set_mailboxes(account, &mailboxes);
    }

    /// Replaces `account`'s folders with a fresh `LIST` result.
    pub fn set_mailboxes(&mut self, account: usize, mailboxes: &[MailboxInfo]) {
        if let Some(entry) = self.accounts.get_mut(account) {
            entry.rows = flatten_tree(&mailbox_tree(mailboxes));
        }
    }

    /// Records the counts of a `STATUS` batch. Folders the batch does not
    /// mention keep their count, so asking about one folder after a change
    /// leaves the others alone.
    pub fn set_unread(&mut self, account: usize, unread: HashMap<String, u32>) {
        if let Some(entry) = self.accounts.get_mut(account) {
            entry.unread.extend(unread);
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
        let (account, folder) = split_id(parent);
        let Some(entry) = self.accounts.get(account) else { return Vec::new() };
        let (start, depth) = match folder {
            0 => (0, 0),
            folder => match entry.rows.iter().position(|row| folder_hash(&row.key) == folder) {
                Some(index) => (index + 1, entry.rows[index].depth + 1),
                None => return Vec::new(),
            },
        };
        entry.rows[start..]
            .iter()
            .take_while(|row| row.depth >= depth)
            .filter(|row| row.depth == depth)
            .map(|row| Node { text: entry.folder_text(row), id: make_id(account, folder_hash(&row.key)), has_children: row.has_children })
            .collect()
    }

    /// The folder a node stands for, or `None` for an account node or a
    /// container folder that cannot be opened.
    pub fn selection(&self, id: NodeId) -> Option<FolderRef> {
        let (account, folder) = split_id(id);
        if folder == 0 {
            return None;
        }
        let row = self.accounts.get(account)?.rows.iter().find(|row| folder_hash(&row.key) == folder)?;
        Some(FolderRef { account, mailbox: row.full_name.clone()? })
    }

    /// `(node id, key, has_children, depth)` for every folder of `account`, in
    /// the order the tree shows them (a parent before its children), for
    /// restoring fold state.
    pub fn rows_with_ids(&self, account: usize) -> Vec<(NodeId, String, bool, usize)> {
        self.accounts
            .get(account)
            .map(|entry| entry.rows.iter().map(|row| (make_id(account, folder_hash(&row.key)), row.key.clone(), row.has_children, row.depth)).collect())
            .unwrap_or_default()
    }

    /// The `(account, folder key)` a folder node stands for, for persisting its
    /// fold state. `None` for an account node.
    pub fn row_key(&self, id: NodeId) -> Option<(usize, String)> {
        let (account, folder) = split_id(id);
        if folder == 0 {
            return None;
        }
        let row = self.accounts.get(account)?.rows.iter().find(|row| folder_hash(&row.key) == folder)?;
        Some((account, row.key.clone()))
    }

    /// The name of `account`'s folder of kind `want` (Trash, Archive), or
    /// `default` when the server names none.
    pub fn special_folder(&self, account: usize, want: SpecialUse, default: &str) -> String {
        self.accounts.get(account).map_or_else(|| default.to_string(), |a| find_special_use_mailbox(&a.rows, want, default))
    }

    /// The unread messages in every account's inbox, which is what the tray
    /// icon's tooltip counts (the other folders are not new mail).
    pub fn inbox_unread(&self) -> u32 {
        self.accounts
            .iter()
            .flat_map(|account| &account.unread)
            .filter(|(name, _)| name.eq_ignore_ascii_case("INBOX"))
            .map(|(_, count)| count)
            .sum()
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

fn make_id(account: usize, folder: u32) -> NodeId {
    ((account as i64) << FOLDER_BITS) | i64::from(folder)
}

fn split_id(id: NodeId) -> (usize, u32) {
    ((id >> FOLDER_BITS) as usize, (id & ((1 << FOLDER_BITS) - 1)) as u32)
}

/// FNV-1a of a folder's path key, never 0 (which names the account node).
fn folder_hash(key: &str) -> u32 {
    key.bytes().fold(0x811c_9dc5_u32, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)).max(1)
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
    fn cached_folders_stand_in_until_the_server_lists_them() {
        let mut tree = FolderTree::new(["Work".to_string()]);
        tree.set_cached_mailboxes(0, &["Sent".to_string(), "INBOX".to_string()]);
        let account = tree.children(None)[0].id;
        assert_eq!(texts(&tree.children(Some(account))), ["INBOX", "Sent"]);
        tree.set_mailboxes(0, &[mailbox("INBOX", false)]);
        tree.set_cached_mailboxes(0, &["Stale".to_string()]);
        assert_eq!(texts(&tree.children(Some(account))), ["INBOX"]);
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
    fn a_row_key_names_the_account_and_folder_for_persisting_folds() {
        let tree = tree();
        let account = tree.children(None)[0].id;
        let projects = tree.children(Some(account))[2].id;
        assert_eq!(tree.row_key(account), None, "an account node has no folder key");
        assert_eq!(tree.row_key(projects), Some((0, "Projects".to_string())));
    }

    #[test]
    fn rows_with_ids_are_in_tree_order_with_depth_and_children() {
        let rows = tree().rows_with_ids(0);
        let keys: Vec<&str> = rows.iter().map(|(_, key, _, _)| key.as_str()).collect();
        assert_eq!(keys, ["INBOX", "[Gmail]", "Projects", "Projects/Alpha", "Projects/Beta"]);
        let projects = rows.iter().find(|(_, key, _, _)| key == "Projects").unwrap();
        assert!(projects.2, "Projects has children");
        assert_eq!(projects.3, 0, "Projects is a top-level row");
        assert_eq!(rows.iter().find(|(_, key, _, _)| key == "Projects/Alpha").unwrap().3, 1);
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
    fn a_folder_keeps_its_id_when_another_folder_appears_before_it() {
        let mut tree = tree();
        let account = tree.children(None)[0].id;
        let projects = |tree: &FolderTree| tree.children(Some(account)).into_iter().find(|n| n.text == "Projects").unwrap().id;
        let before = projects(&tree);
        tree.set_mailboxes(
            0,
            &[mailbox("INBOX", false), mailbox("Archive", false), mailbox("Projects", false), mailbox("Projects/Alpha", false), mailbox("[Gmail]", true)],
        );
        assert_eq!(projects(&tree), before);
        assert_eq!(tree.selection(before), Some(FolderRef { account: 0, mailbox: "Projects".into() }));
    }

    #[test]
    fn the_inbox_total_sums_every_accounts_inbox_and_nothing_else() {
        let mut tree = FolderTree::new(["A".to_string(), "B".to_string()]);
        tree.set_unread(0, HashMap::from([("INBOX".to_string(), 3), ("Projects".to_string(), 9)]));
        tree.set_unread(1, HashMap::from([("Inbox".to_string(), 2)]));
        assert_eq!(tree.inbox_unread(), 5);
        assert_eq!(FolderTree::default().inbox_unread(), 0);
    }

    #[test]
    fn a_later_unread_batch_keeps_the_counts_it_does_not_mention() {
        let mut tree = tree();
        tree.set_unread(0, HashMap::from([("INBOX".to_string(), 3), ("Projects".to_string(), 1)]));
        tree.set_unread(0, HashMap::from([("INBOX".to_string(), 2)]));
        let account = tree.children(None)[0].id;
        assert_eq!(texts(&tree.children(Some(account))), ["INBOX (2)", "[Gmail]", "Projects (1)"]);
    }

    #[test]
    fn the_trash_folder_is_found_by_special_use_or_defaults() {
        let tree = tree();
        assert_eq!(tree.special_folder(0, SpecialUse::Trash, "Trash"), "Trash");
        assert_eq!(tree.special_folder(5, SpecialUse::Archive, "Archive"), "Archive");
    }

    #[test]
    fn status_is_requested_for_selectable_folders_only() {
        assert_eq!(tree().mailbox_names(0), ["INBOX", "Projects", "Projects/Alpha", "Projects/Beta"]);
    }
}
