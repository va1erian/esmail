//! The events a [`MessageList`](crate::MessageList) raises and the app-side
//! closures that map them to the app's `Msg`.

use win32ui::Point;

/// An event raised by a [`MessageList`], mapped to the app's `Msg` by the
/// closures given at construction.
#[derive(Clone, Debug)]
pub enum MessageListEvent {
    /// The selection changed. Carries every selected row, ascending.
    Selected(Vec<usize>),
    /// A row was opened (Enter or double-click).
    Open(usize),
    /// The selected rows were deleted.
    Delete(Vec<usize>),
    /// The flag of the focused row should toggle (Space).
    ToggleFlag(usize),
    /// A context menu was requested for `row` at the pointer position `at`
    /// (client coordinates, device pixels).
    Context {
        /// The row the pointer is over.
        row: usize,
        /// The pointer position in client coordinates (device pixels).
        at: Point,
    },
}

type SelectMapper<M> = Box<dyn Fn(&[usize]) -> Option<M>>;
type RowMapper<M> = Box<dyn Fn(usize) -> Option<M>>;
type ContextMapper<M> = Box<dyn Fn(usize, Point) -> Option<M>>;

/// The app-side event mappings a [`MessageList`] is built with.
pub(crate) struct MessageListEvents<M> {
    pub(crate) on_select: Option<SelectMapper<M>>,
    pub(crate) on_open: Option<RowMapper<M>>,
    pub(crate) on_delete: Option<Box<dyn Fn(&[usize]) -> Option<M>>>,
    pub(crate) on_flag: Option<RowMapper<M>>,
    pub(crate) on_context: Option<ContextMapper<M>>,
    pub(crate) on_near_end: Option<Box<dyn Fn() -> Option<M>>>,
}

impl<M> MessageListEvents<M> {
    pub(crate) fn new() -> MessageListEvents<M> {
        MessageListEvents {
            on_select: None,
            on_open: None,
            on_delete: None,
            on_flag: None,
            on_context: None,
            on_near_end: None,
        }
    }
}
