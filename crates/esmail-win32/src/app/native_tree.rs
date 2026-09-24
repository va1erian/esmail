//! Raw `SysTreeView32` messages for the folder tree.
//!
//! win32ui's `TreeView` loads nodes lazily and has no way to change one, so a
//! new unread count or folder would mean building a new control and losing the
//! selection and which folders are expanded. This module edits the live control
//! instead: [`sync`] brings the nodes that are already in it in line with the
//! folder model and leaves everything else (selection, expansion, scroll)
//! alone. It is the smallest workaround for the gap reported on win32ui #21.

use core::ffi::c_void;

use win32ui::{AsControl, TreeView};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::{
    HTREEITEM, TVE_EXPAND, TVGN_CHILD, TVGN_NEXT, TVGN_ROOT, TVI_FIRST, TVI_ROOT, TVIF_CHILDREN, TVIF_HANDLE, TVIF_PARAM, TVIF_TEXT, TVINSERTSTRUCTW,
    TVINSERTSTRUCTW_0, TVITEMEXW_CHILDREN, TVITEMW, TVM_DELETEITEM, TVM_EXPAND, TVM_GETITEMW, TVM_GETNEXTITEM, TVM_INSERTITEMW, TVM_SETITEMW,
};
use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
use windows::core::PWSTR;

use esmail_win32::core_glue::{FolderTree, Node, NodeId};

/// Longest node text read back, in UTF-16 units; longer names are compared
/// truncated, which at worst rewrites the text once more than needed.
const TEXT_CAPACITY: usize = 256;

/// One node as the control currently holds it.
struct Existing {
    item: HTREEITEM,
    id: NodeId,
    text: String,
}

/// Expands each top-level node, which makes the control ask the source for
/// their children.
pub fn expand_roots<M>(tree: &TreeView<M>) {
    let hwnd = handle(tree);
    let mut item = next(hwnd, TVGN_ROOT, HTREEITEM(0));
    while item.0 != 0 {
        expand(hwnd, item);
        item = next(hwnd, TVGN_NEXT, item);
    }
}

/// Makes the nodes the control already holds match `folders`: changed text is
/// rewritten, new folders are inserted in place, vanished ones are removed.
/// Branches that were never expanded are left for the control to load later
/// from the (shared) source, which sees the new model.
pub fn sync<M>(tree: &TreeView<M>, folders: &FolderTree) {
    let hwnd = handle(tree);
    reconcile(hwnd, TVI_ROOT, None, folders);
}

fn reconcile(hwnd: HWND, parent: HTREEITEM, parent_id: Option<NodeId>, folders: &FolderTree) {
    let mut existing = children(hwnd, parent);
    let mut first_folders = false;
    if existing.is_empty() && parent != TVI_ROOT {
        // An account node was expanded before its folders had arrived, which
        // left it empty and collapsed: it must receive them, and be expanded
        // again. Deeper nodes without children were never loaded; the control
        // loads them from the (shared) source when the user expands them.
        if !parent_id.is_some_and(|id| is_account(folders, id)) {
            return;
        }
        // If the control has not asked the source for this node's children yet
        // it does so now, and finds the folders already there.
        expand(hwnd, parent);
        existing = children(hwnd, parent);
        first_folders = true;
    }
    let wanted = folders.children(parent_id);
    for stale in existing.iter().filter(|old| wanted.iter().all(|node| node.id != old.id)) {
        // SAFETY: deleting an item handle this control returned a moment ago.
        unsafe {
            SendMessageW(hwnd, TVM_DELETEITEM, Some(WPARAM(0)), Some(LPARAM(stale.item.0)));
        }
    }
    existing.retain(|old| wanted.iter().any(|node| node.id == old.id));

    let mut after = TVI_FIRST;
    for node in &wanted {
        match existing.iter().find(|old| old.id == node.id) {
            Some(old) => {
                if old.text != node.text {
                    set_text(hwnd, old.item, &node.text);
                }
                after = old.item;
                reconcile(hwnd, old.item, Some(node.id), folders);
            }
            None => after = insert(hwnd, parent, after, node),
        }
    }
    if first_folders {
        expand(hwnd, parent);
    }
}

fn expand(hwnd: HWND, item: HTREEITEM) {
    // SAFETY: `TVM_EXPAND` takes an action flag and an item handle and keeps
    // neither.
    unsafe {
        SendMessageW(hwnd, TVM_EXPAND, Some(WPARAM(TVE_EXPAND.0 as usize)), Some(LPARAM(item.0)));
    }
}

/// Whether `id` names an account rather than a folder.
fn is_account(folders: &FolderTree, id: NodeId) -> bool {
    folders.children(None).iter().any(|node| node.id == id)
}

fn handle<M>(tree: &TreeView<M>) -> HWND {
    HWND(tree.control().hwnd().raw() as *mut c_void)
}

fn next(hwnd: HWND, relation: u32, item: HTREEITEM) -> HTREEITEM {
    // SAFETY: `TVM_GETNEXTITEM` takes a relation code and an item handle (0
    // for none) and returns a handle or 0; it retains nothing.
    HTREEITEM(unsafe { SendMessageW(hwnd, TVM_GETNEXTITEM, Some(WPARAM(relation as usize)), Some(LPARAM(item.0))) }.0)
}

fn children(hwnd: HWND, parent: HTREEITEM) -> Vec<Existing> {
    let mut found = Vec::new();
    let relation_first = if parent == TVI_ROOT { TVGN_ROOT } else { TVGN_CHILD };
    let mut item = next(hwnd, relation_first, if parent == TVI_ROOT { HTREEITEM(0) } else { parent });
    while item.0 != 0 {
        let mut text = [0u16; TEXT_CAPACITY];
        let mut entry = TVITEMW {
            mask: TVIF_HANDLE | TVIF_PARAM | TVIF_TEXT,
            hItem: item,
            pszText: PWSTR(text.as_mut_ptr()),
            cchTextMax: TEXT_CAPACITY as i32,
            ..Default::default()
        };
        // SAFETY: `entry` and the buffer `pszText` points at outlive the call,
        // which fills them in place and keeps neither.
        unsafe {
            SendMessageW(hwnd, TVM_GETITEMW, Some(WPARAM(0)), Some(LPARAM(&mut entry as *mut TVITEMW as isize)));
        }
        let length = text.iter().position(|&unit| unit == 0).unwrap_or(TEXT_CAPACITY);
        found.push(Existing { item, id: entry.lParam.0 as i64, text: String::from_utf16_lossy(&text[..length]) });
        item = next(hwnd, TVGN_NEXT, item);
    }
    found
}

fn set_text(hwnd: HWND, item: HTREEITEM, text: &str) {
    let mut wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let entry = TVITEMW { mask: TVIF_HANDLE | TVIF_TEXT, hItem: item, pszText: PWSTR(wide.as_mut_ptr()), ..Default::default() };
    // SAFETY: `entry` and `wide` outlive the call; the control copies the text.
    unsafe {
        SendMessageW(hwnd, TVM_SETITEMW, Some(WPARAM(0)), Some(LPARAM(&entry as *const TVITEMW as isize)));
    }
}

fn insert(hwnd: HWND, parent: HTREEITEM, after: HTREEITEM, node: &Node) -> HTREEITEM {
    let mut wide: Vec<u16> = node.text.encode_utf16().chain(std::iter::once(0)).collect();
    let item = TVITEMW {
        mask: TVIF_TEXT | TVIF_PARAM | TVIF_CHILDREN,
        pszText: PWSTR(wide.as_mut_ptr()),
        cChildren: TVITEMEXW_CHILDREN(i32::from(node.has_children)),
        lParam: LPARAM(node.id as isize),
        ..Default::default()
    };
    let request = TVINSERTSTRUCTW { hParent: parent, hInsertAfter: after, Anonymous: TVINSERTSTRUCTW_0 { item } };
    // SAFETY: `request` and `wide` outlive the call; the control copies both.
    let inserted = unsafe { SendMessageW(hwnd, TVM_INSERTITEMW, Some(WPARAM(0)), Some(LPARAM(&request as *const TVINSERTSTRUCTW as isize))) };
    HTREEITEM(inserted.0)
}
