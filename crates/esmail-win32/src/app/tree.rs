//! The left pane: a native tree of accounts and their folders.

use std::cell::RefCell;
use std::rc::Rc;

use win32ui::{Rect, TreeEntry, TreeSource, TreeView, Ui};

use esmail_win32::core_glue::FolderTree;

use super::Msg;
use super::native_tree;

/// The folder model, shared with the app so the control's lazily loaded
/// branches always see the current folders.
pub type SharedFolders = Rc<RefCell<FolderTree>>;

struct Source(SharedFolders);

impl TreeSource for Source {
    fn children(&self, parent: Option<i64>) -> Vec<TreeEntry> {
        self.0
            .borrow()
            .children(parent)
            .into_iter()
            .map(|node| if node.has_children { TreeEntry::branch(node.text, node.id) } else { TreeEntry::leaf(node.text, node.id) })
            .collect()
    }
}

/// A tree showing `folders`, with every account expanded. Later changes to the
/// folder model reach it through [`native_tree::sync`], which keeps the
/// selection and expansion.
pub fn build(ui: &mut Ui<Msg>, folders: &SharedFolders) -> win32ui::Result<TreeView<Msg>> {
    let tree = TreeView::new(ui, Rect::default(), Box::new(Source(folders.clone())))?.on_select(|item| item.map(Msg::Folder));
    native_tree::expand_roots(&tree);
    Ok(tree)
}
