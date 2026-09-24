//! The `esmail-win32` window: folders on the left, the message list (under a
//! search box) in the middle, the reading pane on the right, a status bar below.
//!
//! Everything slow happens off the UI thread. IMAP sessions live on the core's
//! runtime and wake the window through a `Proxy`; the handler for that wake
//! drains the events (`Core::pump`) and updates the widgets. The local cache is
//! read, written and searched on the runtime too, so the folder opens with what
//! the last run cached before the network answers. Message bodies are fetched by
//! the actor's body worker and rendered by the HTML view's own thread, so a
//! selection change never waits for either.
//!
//! The IMAP channel is drained through this crate's `Core` rather than
//! `esmail::app::AppCore`: `AppCore` keeps one page of headers and replaces it
//! per page, while this list accumulates pages as the user scrolls and merges
//! refreshes into them, and `AppCore` also needs the egui app's cache task.

mod accounts;
mod actions;
mod args;
mod bars;
mod chrome;
mod compose;
mod composes;
mod events;
mod folder;
mod instance;
mod links;
mod message;
mod notifications;
mod placement;
mod preferences;
mod queue;
mod reader;
mod reader_bar;
mod screenshot;
mod search;
mod setup;
mod startup;
mod theme;
mod toolbar;
mod tray;
mod tree;

use std::path::PathBuf;

use esmail::imap::MailHeader;
use win32ui::prelude::*;
use win32ui::{column, split_row};

use esmail_win32::MessageList;
use esmail_win32::core_glue::mailbox::OpenFolder;
use esmail_win32::core_glue::reading::Palette;
use esmail::compose::ComposeId;
use esmail_win32::core_glue::compose::Kind;
use esmail_win32::core_glue::{BodyLoads, ConfigSaver, Core, FolderRef, Latest, WindowState};
use accounts::{Accounts, FormRequest, ManageRequest, Outcome};
use setup::waker;

pub(crate) use setup::main;
use esmail_win32::core_glue::{Settings, ThemeChoice};
use instance::Launch;
use queue::{QueueKind, Queues};
use tray::Tray;
use message::SeenTimer;
use reader::Reader;
use reader_bar::ReaderBar;
use screenshot::{Capture, Step};
use composes::Composes;
use search::SearchState;
use startup::Startup;
use toolbar::MainBar;
use tree::{FolderView, SharedFolders};

/// The folder pane's width and the list's, in device-independent pixels, before
/// the user has dragged a divider.
const DEFAULT_FOLDERS_WIDTH: f32 = 230.0;
const DEFAULT_LIST_WIDTH: f32 = 420.0;

/// Messages the window's widgets and the core raise.
enum Msg {
    /// The core has events to drain.
    Wake,
    /// The HTML view finished a frame.
    Frame,
    Folder(i64),
    Selected(Vec<usize>),
    /// Enter or a double-click on a row.
    Open(usize),
    NearEnd,
    Link(String),
    SetTheme(ThemeChoice),
    /// View > Original colours.
    OriginalColours(bool),
    /// "Always load from <sender>", and taking it back.
    TrustSender(bool),
    /// The banner's Stop button: turns off loading for the message on screen.
    StopRemoteImages,
    /// Ctrl+N, Ctrl+R, Ctrl+Shift+R, Ctrl+L or the list's R, Shift+R, F.
    Compose(Kind),
    /// A compose window asks for something.
    ComposeRequest(ComposeId, compose::Request),
    /// View > Load remote images.
    RemoteImages(bool),
    /// A save or open of an attachment finished on its thread.
    AttachmentDone(std::result::Result<String, String>),
    Refresh,
    ToggleFlag,
    /// A click on a row's star, or Space on it.
    ToggleFlagAt(usize),
    SetSeen(bool),
    Archive,
    Delete,
    /// A right-click on a message row.
    Context,
    /// Ctrl+F.
    SearchFocus,
    /// The search box's text changed.
    SearchChanged(String),
    /// Enter: search now (in the search box) or open the focused row (in the list).
    Enter,
    /// The search box gained or lost the keyboard focus.
    SearchFocused(bool),
    /// Esc.
    SearchClear,
    /// The theme button cycled Dark / Light / System.
    CycleTheme,
    /// Export the open message's raw source as an .eml.
    Export,
    /// The divider between the folders and the list moved (to this width).
    FoldersMoved(f32),
    /// The divider between the list and the reading pane moved.
    ListMoved(f32),
    /// File > Drafts... and File > Outbox...
    ShowQueue(QueueKind),
    /// A Drafts or Outbox window asks for something.
    QueueRequest(QueueKind, queue::Request),
    /// File > Add account..., and the first-run form.
    AddAccount,
    /// File > Accounts...
    ManageAccounts,
    /// The account form asks for something.
    AccountRequest(FormRequest),
    /// The Accounts window asks for something.
    ManageRequest(ManageRequest),
    /// The work behind the account form reported.
    AccountOutcome(Outcome),
    /// The window is closing.
    Close,
    /// The tray, a toast click or a second launch asks for something.
    Launch(Launch),
    /// View > Close to tray.
    CloseToTray(bool),
    Quit,
    Timer(TimerId),
}

struct App {
    core: Core,
    /// The accounts as saved: what the core was started with.
    config: esmail::config::Config,
    /// Writes `config.toml` when the trusted senders change.
    config_saver: ConfigSaver,
    accounts: Accounts,
    /// Accounts can be added and removed (not while `--profile` reads a copy).
    editable: bool,
    folders: SharedFolders,
    list: MessageList<Msg>,
    search_edit: Edit<Msg>,
    search: SearchState,
    tree: FolderView,
    reader: Reader,
    toolbar: MainBar,
    reader_bar: ReaderBar,
    status: StatusBar<Msg>,
    theme: ThemeChoice,
    original_colours: bool,
    /// The folder shown in the list, once one has been opened.
    open: Option<OpenFolder>,
    /// The folder to open at start (`--folder`), else the inbox.
    wanted_folder: Option<String>,
    /// The account whose folder opens at start (`--account`).
    wanted_account: usize,
    bodies: BodyLoads<message::BodyKey>,
    /// The message whose body is (being) shown, and the folder it is in (a
    /// search result can come from any folder).
    selected: Option<MailHeader>,
    selected_in: Option<FolderRef>,
    /// Marks the selected message read once it has been open a moment.
    seen: SeenTimer,
    /// Ids for flag and move commands (the actor echoes them back).
    action_ids: Latest,
    /// Flag, move or export commands sent to the server and not yet confirmed,
    /// so the toolbars can disable actions while one is running.
    pending_actions: usize,
    /// The selected message's body is on screen (screenshots wait for this).
    message_shown: bool,
    select_after_load: Option<usize>,
    capture: Option<(TimerId, Capture)>,
    /// Where the window and its dividers were last left, and the file that
    /// keeps it (none for `--screenshot` runs, which must not change it).
    window: WindowState,
    window_path: Option<PathBuf>,
    startup: Startup,
    composes: Composes,
    /// View > Load remote images.
    remote_images: bool,
    /// The timer that watches the followed system theme.
    theme_poll: Option<TimerId>,
    /// Asks the outbox for messages due another send attempt.
    outbox_poll: Option<TimerId>,
    /// `--acrylic`: compose windows get the same look.
    acrylic: bool,
    /// Accounts whose tree node was opened once their folders arrived.
    opened_accounts: std::collections::HashSet<usize>,
    /// `--compose`: the window to open once the folder (and the selected message) is up.
    start_compose: Option<Kind>,
    /// `--show`: the Drafts or Outbox window to open once the folder is up, and
    /// whether one was opened and is still waiting for its rows.
    start_queue: Option<QueueKind>,
    queue_pending: bool,
    queues: Queues,
    /// The saved View choices, and the file that keeps them (none for
    /// `--screenshot` runs, which must not change it).
    settings: Settings,
    settings_path: Option<PathBuf>,
    /// The tray icon; none in `--screenshot` and `--profile` runs, or when the
    /// shell has no notification area.
    tray: Option<Tray>,
    /// The window is hidden in the tray.
    hidden: bool,
    /// The unread total last put in the window title.
    title_unread: u32,
    /// Called with each batch of new mail; what the account sessions are started with.
    notify: esmail::session::NotifyFn,
}

/// The reading pane's palette for a window theme.
fn palette_for(theme: &Theme) -> Palette {
    if theme.is_dark { Palette::DARK } else { Palette::LIGHT }
}

impl win32ui::App for App {
    type Msg = Msg;

    fn update(&mut self, msg: Msg, ui: &mut Ui<Msg>) {
        let sync_bars = !matches!(&msg, Msg::Frame);
        match msg {
            Msg::Wake => {
                self.drain(ui);
                self.sync_unread(ui);
                self.open_requested_compose(ui);
                self.open_requested_queue(ui);
            }
            Msg::Frame => self.reader.invalidate(),
            Msg::Folder(id) => {
                let folder = self.folders.borrow().selection(id);
                if let Some(folder) = folder {
                    self.open_folder(ui, folder);
                }
            }
            Msg::Selected(rows) => self.select(ui, &rows),
            Msg::Open(row) => {
                self.select(ui, &[row]);
                self.reader.focus();
            }
            Msg::NearEnd => {
                if !self.search.active() {
                    self.request_page();
                }
            }
            Msg::Link(href) => self.link_clicked(ui, href),
            Msg::AttachmentDone(result) => self.attachment_done(result),
            Msg::SetTheme(choice) => self.choose_theme(ui, choice),
            Msg::OriginalColours(original) => {
                self.original_colours = original;
                self.reader.set_original_colours(original);
                self.refresh_menu(ui);
            }
            Msg::TrustSender(trusted) => self.trust_sender(trusted),
            Msg::StopRemoteImages => self.stop_remote_images(ui),
            Msg::Compose(kind) => self.open_compose(ui, kind),
            Msg::ComposeRequest(id, request) => self.compose_request(id, request),
            Msg::RemoteImages(allow) => self.set_remote_images(ui, allow),
            Msg::Refresh => self.refresh(ui),
            Msg::ToggleFlag => self.toggle_flag(),
            Msg::ToggleFlagAt(row) => self.toggle_flag_at(row),
            Msg::SetSeen(seen) => self.set_seen(seen),
            Msg::Archive => self.archive(),
            Msg::Delete => self.delete(),
            Msg::Context => ui.popup(&chrome::message_menu(), ui.cursor_position()),
            Msg::SearchFocus => self.focus_search(),
            Msg::SearchChanged(text) => self.search_changed(ui, text),
            Msg::Enter => self.enter(ui),
            Msg::SearchFocused(focused) => self.search.set_focused(focused),
            Msg::SearchClear => self.clear_search(ui),
            Msg::CycleTheme => self.choose_theme(ui, self.theme.next()),
            Msg::Export => self.export_selected(),
            Msg::FoldersMoved(width) => self.window.folders_width = Some(width),
            Msg::ListMoved(width) => self.window.list_width = Some(width),
            Msg::ShowQueue(kind) => self.show_queue(ui, kind),
            Msg::QueueRequest(kind, request) => self.queue_request(ui, kind, request),
            Msg::AddAccount => self.add_account(ui),
            Msg::ManageAccounts => self.manage_accounts(ui),
            Msg::AccountRequest(request) => self.form_request(ui, request),
            Msg::ManageRequest(request) => self.manage_request(ui, request),
            Msg::AccountOutcome(outcome) => self.account_outcome(ui, outcome),
            Msg::Close => self.close(ui),
            Msg::Quit => self.quit(ui),
            Msg::Launch(launch) => self.launch(ui, launch),
            Msg::CloseToTray(on) => self.set_close_to_tray(ui, on),
            Msg::Timer(id) => self.timer(ui, id),
        }
        if sync_bars {
            self.sync_bars(ui);
        }
    }
}

impl App {
    fn layout(&self, ui: &Ui<Msg>) {
        let list_pane = column![self.search_edit, self.list.fill(1)];
        let reading_pane = Layout::column()
            .item(self.reader_bar.actions_layout())
            .item(self.reader_bar.banner_layout())
            .item(self.reader.fill(1));
        let list_and_reader = split_row![list_pane, reading_pane]
            .position(dip(self.window.list_width.unwrap_or(DEFAULT_LIST_WIDTH)))
            .min(dip(280.0), dip(320.0))
            .on_moved(|width| Some(Msg::ListMoved(width.value())));
        let panes = split_row![self.tree, list_and_reader]
            .position(dip(self.window.folders_width.unwrap_or(DEFAULT_FOLDERS_WIDTH)))
            .min(dip(140.0), dip(600.0))
            .on_moved(|width| Some(Msg::FoldersMoved(width.value())));
        // An extended (acrylic) title strip is not part of the frame: start below it.
        let below_strip = Insets::new(dip(0.0), ui.title_bar_height(), dip(0.0), dip(0.0));
        let main = Layout::column()
            .item(self.toolbar.bar_layout(ui.dpi()))
            .item(panes)
            .item(&self.status);
        ui.set_layout(main.margins(below_strip));
    }

    /// Opens the wanted folder of the wanted account straight away, and asks the
    /// cache which folders exist: both show what the last run cached while the
    /// IMAP session is still connecting.
    fn open_from_cache(&mut self, ui: &Ui<Msg>) {
        for account in 0..self.core.accounts().len() {
            self.core.cache().load_mailboxes(account);
        }
        if !self.core.accounts().is_empty() {
            let mailbox = self.wanted_folder.clone().unwrap_or_else(|| "INBOX".to_string());
            self.open_folder(ui, FolderRef { account: self.wanted_account.min(self.core.accounts().len() - 1), mailbox });
        }
    }

    /// Remembers where the window is, so the next start reopens it there.
    fn save_window_state(&mut self, ui: &Ui<Msg>) {
        let Some(path) = self.window_path.as_deref() else { return };
        if let Some((bounds, maximized)) = placement::read(ui.hwnd()) {
            self.window.bounds = Some(bounds);
            self.window.maximized = maximized;
        }
        if let Err(error) = self.window.save(path) {
            log::warn!("could not save the window state to {}: {error}", path.display());
        }
    }

    fn set_status(&self, text: &str) {
        self.status.set_text(0, text);
    }

    /// An error the user must see: in the status bar, and in the reading pane
    /// when there is no message there to cover.
    fn banner(&mut self, text: &str) {
        self.set_status(&format!("Error: {text}"));
        if self.selected.is_none() {
            self.reader.show_notice(text);
        }
    }

    fn timer(&mut self, ui: &mut Ui<Msg>, id: TimerId) {
        if self.seen.owns(id) {
            self.mark_seen_when_due(ui);
        } else if self.outbox_poll == Some(id) {
            self.core.cache().due_outbox();
        } else if self.theme_poll == Some(id) {
            self.sync_reader_theme(ui);
        } else if self.search.owns(id) {
            self.run_search(ui);
        } else if self.capture.as_ref().is_some_and(|(timer, _)| *timer == id) {
            self.tick(ui);
        }
    }

    /// `--compose`: opens that window as soon as its message is on screen.
    fn open_requested_compose(&mut self, ui: &Ui<Msg>) {
        let loaded = self.open.as_ref().is_some_and(|o| o.loaded() > 0);
        let message_up = self.reader.current_message().is_some();
        let waiting_for_message = self.select_after_load.is_some() || self.selected.is_some() && !self.message_shown;
        let ready = |kind: Kind| loaded && if kind.needs_original() { message_up } else { !waiting_for_message };
        if let Some(kind) = self.start_compose.filter(|kind| ready(*kind)) {
            self.start_compose = None;
            self.open_compose(ui, kind);
        }
    }

    /// `--show`: opens that window as soon as the folder is up.
    fn open_requested_queue(&mut self, ui: &Ui<Msg>) {
        let loaded = self.open.as_ref().is_some_and(|o| o.loaded() > 0);
        if let Some(kind) = self.start_queue.filter(|_| loaded) {
            self.start_queue = None;
            self.queue_pending = true;
            self.show_queue(ui, kind);
        }
    }

    /// Screenshot mode: check whether the window is ready to capture.
    fn tick(&mut self, ui: &mut Ui<Msg>) {
        let message_ready = self.message_shown && self.reader.is_ready();
        let loaded = self.open.as_ref().is_some_and(|o| o.loaded() > 0);
        let expects_message = self.select_after_load.is_some() || self.selected.is_some();
        let ready = loaded && (!expects_message || message_ready);
        let composed = self.start_compose.is_none() && self.start_queue.is_none() && !self.queue_pending;
        let compose_window = self.composes.any_window().cloned();
        let queue_window = self.queues.any_window().cloned();
        let Some((_, capture)) = self.capture.as_mut() else { return };
        match capture.step(ui, ready && composed) {
            Step::Wait => {}
            Step::Repaint => {
                self.list.invalidate();
                self.reader.invalidate();
            }
            Step::Capture => {
                eprintln!("esmail-win32: {}", self.startup.report(self.list.first_content_paint()));
                let secondary = compose_window.map(|window| ("compose", window.capture())).or_else(|| queue_window.map(|window| ("window", window.capture())));
                capture.finish(ui, secondary);
            }
        }
    }
}
