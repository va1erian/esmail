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

mod actions;
mod args;
mod chrome;
mod compose;
mod composes;
mod events;
mod folder;
mod links;
mod message;
mod placement;
mod reader;
mod screenshot;
mod search;
mod startup;
mod theme;
mod tree;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use esmail::imap::MailHeader;
use win32ui::prelude::*;
use win32ui::{column, split_row};

use esmail_win32::MessageList;
use esmail_win32::core_glue::mailbox::OpenFolder;
use esmail_win32::core_glue::reading::Palette;
use esmail::compose::ComposeId;
use esmail_win32::core_glue::compose::Kind;
use esmail_win32::core_glue::{BodyLoads, Core, FolderRef, FolderTree, Latest, WindowState, load_config};
use args::{Args, ThemeChoice};
use message::SeenTimer;
use reader::Reader;
use screenshot::{Capture, Step};
use composes::Composes;
use search::SearchState;
use startup::Startup;
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
    /// The divider between the folders and the list moved (to this width).
    FoldersMoved(f32),
    /// The divider between the list and the reading pane moved.
    ListMoved(f32),
    /// The window is closing.
    Close,
    Quit,
    Timer(TimerId),
}

struct App {
    core: Core,
    folders: SharedFolders,
    list: MessageList<Msg>,
    search_edit: Edit<Msg>,
    search: SearchState,
    tree: FolderView,
    reader: Reader,
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
}

pub(crate) fn main() {
    let began = Instant::now();
    let args = match Args::parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    let config = match load_config(args.profile.as_deref()) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("esmail-win32: {message}");
            std::process::exit(1);
        }
    };

    let theme = chrome::theme(args.theme);
    let spec = chrome::acrylic(WindowSpec::new("esMail").size(dip(1200.0), dip(760.0)).theme(theme), args.acrylic);
    let result = win32ui::run_app(spec, |ui| {
        build(ui, &args, &config, began)
    });
    if let Err(error) = result {
        eprintln!("esmail-win32 failed: {error}");
        std::process::exit(1);
    }
}

/// The reading pane's palette for a window theme.
fn palette_for(theme: &Theme) -> Palette {
    if theme.is_dark { Palette::DARK } else { Palette::LIGHT }
}

fn build(ui: &mut Ui<Msg>, args: &Args, config: &esmail::config::Config, began: Instant) -> App {
    let proxy = ui.proxy();
    let waker: esmail::waker::Waker = Arc::new(move || {
        let _ = proxy.send(Msg::Wake);
    });
    let (core, issues) = Core::start(config, waker).expect("start the async runtime");

    let folders: SharedFolders = Rc::new(RefCell::new(FolderTree::new(config.accounts.iter().map(|a| a.display_name.clone()))));
    let tree = tree::build(ui, &folders).expect("folder tree");
    let list = MessageList::new(ui)
        .expect("message list")
        .on_select(|rows| Some(Msg::Selected(rows.to_vec())))
        .on_open(|row| Some(Msg::Open(row)))
        .on_toggle_flag(|row| Some(Msg::ToggleFlagAt(row)))
        .on_delete(|_| Some(Msg::Delete))
        .on_context(|_, _| Some(Msg::Context))
        .on_near_end(|| Some(Msg::NearEnd))
        .on_compose(|kind| Some(Msg::Compose(kind)));
    let search_edit = Edit::single_line(ui)
        .expect("search box")
        .cue("Search mail (Ctrl+F)")
        .on_change(|text| Some(Msg::SearchChanged(text.to_string())))
        .on_focus(|focused| Some(Msg::SearchFocused(focused)));
    let reader = Reader::new(ui, palette_for(&ui.theme())).expect("reading pane");
    let status = StatusBar::new(ui).expect("status bar");
    status.set_parts(&[-1]);

    ui.set_menu_bar(chrome::menu_bar(args.theme, false, args.remote_images));
    ui.accelerator(Shortcut::ctrl(Key::F), || Some(Msg::SearchFocus));
    ui.accelerator(Shortcut::key(Key::ESCAPE), || Some(Msg::SearchClear));
    ui.accelerator(Shortcut::key(Key::RETURN), || Some(Msg::Enter));
    ui.on_close(|| Some(Msg::Close));
    ui.on_timer(|id| Some(Msg::Timer(id)));
    let capture = args.screenshot.clone().map(|path| (ui.set_timer(50).expect("screenshot timer"), Capture::new(path)));
    let window_path = if args.screenshot.is_some() { None } else { WindowState::path() };
    let window = window_path.as_deref().map(WindowState::load).unwrap_or_default();

    let mut app = App {
        core,
        folders,
        list,
        search_edit,
        search: SearchState::default(),
        tree,
        reader,
        status,
        theme: args.theme,
        original_colours: false,
        open: None,
        wanted_folder: args.folder.clone(),
        wanted_account: args.account,
        bodies: BodyLoads::default(),
        selected: None,
        selected_in: None,
        seen: SeenTimer::default(),
        action_ids: Latest::default(),
        message_shown: false,
        select_after_load: args.select,
        capture,
        window,
        window_path,
        startup: Startup::new(began),
        composes: Composes::default(),
        remote_images: args.remote_images,
        theme_poll: None,
        outbox_poll: ui.set_timer(composes::OUTBOX_POLL_MILLIS).ok(),
        acrylic: args.acrylic,
        opened_accounts: Default::default(),
        start_compose: args.compose,
    };
    app.layout(ui);
    app.reader.set_remote_images(args.remote_images);
    ui.follow_system_theme(args.theme == ThemeChoice::System);
    if args.theme == ThemeChoice::System {
        app.theme_poll = ui.set_timer(theme::POLL_MILLIS).ok();
    }
    if let Some(bounds) = app.window.bounds {
        placement::restore(ui.hwnd(), bounds, app.window.maximized);
    }
    if app.core.accounts().is_empty() {
        app.banner("No accounts are configured. Add one in the egui esMail first.");
    }
    for issue in issues {
        let name = app.core.accounts()[issue.account].display_name.clone();
        app.banner(&format!("{name}: {}", issue.message));
    }
    app.open_from_cache(ui);
    app.core.cache().due_outbox();
    app
}

impl win32ui::App for App {
    type Msg = Msg;

    fn update(&mut self, msg: Msg, ui: &mut Ui<Msg>) {
        match msg {
            Msg::Wake => {
                self.drain();
                self.open_requested_compose(ui);
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
                ui.set_menu_bar(chrome::menu_bar(self.theme, original, self.remote_images));
            }
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
            Msg::FoldersMoved(width) => self.window.folders_width = Some(width),
            Msg::ListMoved(width) => self.window.list_width = Some(width),
            Msg::Close => self.close(ui),
            Msg::Quit => ui.quit(),
            Msg::Timer(id) => self.timer(ui, id),
        }
    }
}

impl App {
    fn layout(&self, ui: &Ui<Msg>) {
        let list_pane = column![self.search_edit, self.list.fill(1)];
        let list_and_reader = split_row![list_pane, self.reader]
            .position(dip(self.window.list_width.unwrap_or(DEFAULT_LIST_WIDTH)))
            .min(dip(280.0), dip(320.0))
            .on_moved(|width| Some(Msg::ListMoved(width.value())));
        let panes = split_row![self.tree, list_and_reader]
            .position(dip(self.window.folders_width.unwrap_or(DEFAULT_FOLDERS_WIDTH)))
            .min(dip(140.0), dip(600.0))
            .on_moved(|width| Some(Msg::FoldersMoved(width.value())));
        // An extended (acrylic) title strip is not part of the frame: start below it.
        let below_strip = Insets::new(dip(0.0), ui.title_bar_height(), dip(0.0), dip(0.0));
        ui.set_layout(column![panes, self.status].margins(below_strip));
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

    /// Screenshot mode: check whether the window is ready to capture.
    fn tick(&mut self, ui: &mut Ui<Msg>) {
        let message_ready = self.message_shown && self.reader.is_ready();
        let loaded = self.open.as_ref().is_some_and(|o| o.loaded() > 0);
        let expects_message = self.select_after_load.is_some() || self.selected.is_some();
        let ready = loaded && (!expects_message || message_ready);
        let composed = self.start_compose.is_none();
        let compose_window = self.composes.any_window().cloned();
        let Some((_, capture)) = self.capture.as_mut() else { return };
        match capture.step(ui, ready && composed) {
            Step::Wait => {}
            Step::Repaint => {
                self.list.invalidate();
                self.reader.invalidate();
            }
            Step::Capture => {
                eprintln!("esmail-win32: {}", self.startup.report(self.list.first_content_paint()));
                capture.finish(ui, compose_window.as_ref());
            }
        }
    }
}
