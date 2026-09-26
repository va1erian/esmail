//! The left pane: a native tree of accounts and their folders.

use std::cell::RefCell;
use std::rc::Rc;

use xui_win32::{Node, TreeModel, TreeView, Ui};

use esmail_win32::core_glue::{FolderTree, NodeId};

use super::Msg;

/// The folder model, shared with the app so the control's lazily loaded
/// branches always see the current folders.
pub type SharedFolders = Rc<RefCell<FolderTree>>;

/// The folder tree in the shape `TreeView` reads.
pub type FolderView = TreeView<NodeId, Msg>;

struct Source(SharedFolders);

impl TreeModel for Source {
    type Key = NodeId;

    fn children(&self, parent: Option<&NodeId>) -> Vec<Node<NodeId>> {
        self.0
            .borrow()
            .children(parent.copied())
            .into_iter()
            .map(|node| if node.has_children { Node::branch(node.id, node.text) } else { Node::leaf(node.id, node.text) })
            .collect()
    }
}

/// A tree showing `folders`, with every account expanded. Later changes to the
/// folder model reach it through `TreeView::refresh`, which keeps the selection
/// and expansion.
pub fn build(ui: &mut Ui<Msg>, folders: &SharedFolders) -> xui_win32::Result<FolderView> {
    let tree = TreeView::new(ui, Source(folders.clone()))?
        .on_select(|id| Some(Msg::Folder(*id)))
        .on_toggle(|id, expanded| Some(Msg::FolderToggled(*id, expanded)));
    for account in folders.borrow().children(None) {
        tree.expand(&account.id, true);
    }
    Ok(tree)
}
