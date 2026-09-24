//! The left pane: a native tree of accounts and their folders.

use win32ui::{Rect, TreeEntry, TreeSource, TreeView, Ui};

use esmail_win32::core_glue::FolderTree;

use super::Msg;

/// Feeds the tree from a snapshot of the folder model.
struct Snapshot(FolderTree);

impl TreeSource for Snapshot {
    fn children(&self, parent: Option<i64>) -> Vec<TreeEntry> {
        self.0
            .children(parent)
            .into_iter()
            .map(|node| if node.has_children { TreeEntry::branch(node.text, node.id) } else { TreeEntry::leaf(node.text, node.id) })
            .collect()
    }
}

/// A tree showing `folders`, with every account expanded.
///
/// `TreeView` has no way to refresh its nodes, so a new folder list or new
/// unread counts mean building a new tree (see the PR's win32ui gaps).
pub fn build(ui: &mut Ui<Msg>, folders: &FolderTree) -> win32ui::Result<TreeView<Msg>> {
    let tree = TreeView::new(ui, Rect::default(), Box::new(Snapshot(folders.clone())))?
        .on_select(|item| item.map(Msg::Folder));
    expand_roots(&tree);
    Ok(tree)
}

/// Expands each top-level node, which makes the control ask the source for
/// their children.
fn expand_roots(tree: &TreeView<Msg>) {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;

    const TVM_EXPAND: u32 = 0x1102;
    const TVM_GETNEXTITEM: u32 = 0x110A;
    const TVE_EXPAND: usize = 2;
    const TVGN_ROOT: usize = 0;
    const TVGN_NEXT: usize = 1;

    use win32ui::AsControl;
    let hwnd = HWND(tree.control().hwnd().raw() as *mut core::ffi::c_void);
    // SAFETY: `hwnd` is the live tree-view window owned by `tree`; these
    // messages take a wParam flag/code and an HTREEITEM handle (0 for none)
    // and do not retain the arguments.
    unsafe {
        let mut item = SendMessageW(hwnd, TVM_GETNEXTITEM, Some(WPARAM(TVGN_ROOT)), Some(LPARAM(0))).0;
        while item != 0 {
            SendMessageW(hwnd, TVM_EXPAND, Some(WPARAM(TVE_EXPAND)), Some(LPARAM(item)));
            item = SendMessageW(hwnd, TVM_GETNEXTITEM, Some(WPARAM(TVGN_NEXT)), Some(LPARAM(item))).0;
        }
    }
}
