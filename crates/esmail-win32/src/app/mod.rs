//! The `esmail-win32` window: folders on the left, the message list in the
//! middle, the reading pane on the right, a status bar below.
//!
//! Everything slow happens off the UI thread. IMAP sessions live on the core's
//! runtime and wake the window through a `Proxy`; the handler for that wake
//! drains the events (`Core::pump`) and updates the widgets. Message bodies are
//! fetched by the actor's body worker and rendered by the HTML view's own
//! thread, so a selection change never waits for either.
//!
//! The IMAP channel is drained through this crate's `Core` rather than
//! `esmail::app::AppCore`: `AppCore` keeps one page of headers and replaces it
//! per page, while this list accumulates pages as the user scrolls and merges
//! refreshes into them, and `AppCore` also needs the egui app's cache task.

mod actions;
mod args;
mod chrome;
mod events;
mod folder;
mod message;
mod native_tree;
mod reader;
mod screenshot;
mod tree;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use esmail::imap::MailHeader;
use win32ui::prelude::*;
use win32ui::{column, split_row};

use esmail_win32::MessageList;
use esmail_win32::core_glue::mailbox::OpenFolder;
use esmail_win32::core_glue::reading::Palette;
use esmail_win32::core_glue::{BodyLoads, Core, FolderTree, Latest, load_config};
use args::{Args, ThemeChoice};
use message::SeenTimer;
use reader::Reader;
use screenshot::{Capture, Step};
use tree::SharedFolders;

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
    Refresh,
    ToggleFlag,
    SetSeen(bool),
    Archive,
    Delete,
    /// A right-click on a message row.
    Context,
    Quit,
    Timer(TimerId),
}

struct App {
    core: Core,
    folders: SharedFolders,
    list: MessageList<Msg>,
    tree: TreeView<Msg>,
    reader: Reader,
    status: StatusBar<Msg>,
    theme: ThemeChoice,
    original_colours: bool,
    /// The folder shown in the list, once one has been opened.
    open: Option<OpenFolder>,
    /// Which folder to open when an account's folder list first arrives.
    wanted_folder: Option<String>,
    bodies: BodyLoads<message::BodyKey>,
    /// The message whose body is (being) shown.
    selected: Option<MailHeader>,
    /// Marks the selected message read once it has been open a moment.
    seen: SeenTimer,
    /// Ids for flag and move commands (the actor echoes them back).
    action_ids: Latest,
    /// The selected message's body is on screen (screenshots wait for this).
    message_shown: bool,
    select_after_load: Option<usize>,
    capture: Option<(TimerId, Capture)>,
}

pub(crate) fn main() {
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

    let theme = chrome::palette(args.theme);
    let result = win32ui::run_app(WindowSpec::new("esMail").size(dip(1200.0), dip(760.0)).theme(theme), |ui| {
        build(ui, &args, &config)
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

fn build(ui: &mut Ui<Msg>, args: &Args, config: &esmail::config::Config) -> App {
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
        .on_flag(|_| Some(Msg::ToggleFlag))
        .on_delete(|_| Some(Msg::Delete))
        .on_context(|_, _| Some(Msg::Context))
        .on_near_end(|| Some(Msg::NearEnd));
    let reader = Reader::new(ui, palette_for(&ui.theme())).expect("reading pane");
    let status = StatusBar::new(ui).expect("status bar");
    status.set_parts(&[-1]);

    ui.set_menu_bar(chrome::menu_bar(args.theme, false));
    ui.accelerator(Shortcut::key(Key::F5), || Some(Msg::Refresh));
    ui.accelerator(Shortcut::ctrl(Key::Q), || Some(Msg::Quit));
    ui.on_timer(|id| Some(Msg::Timer(id)));
    let capture = args.screenshot.clone().map(|path| (ui.set_timer(50).expect("screenshot timer"), Capture::new(path)));

    let mut app = App {
        core,
        folders,
        list,
        tree,
        reader,
        status,
        theme: args.theme,
        original_colours: false,
        open: None,
        wanted_folder: args.folder.clone(),
        bodies: BodyLoads::default(),
        selected: None,
        seen: SeenTimer::default(),
        action_ids: Latest::default(),
        message_shown: false,
        select_after_load: args.select,
        capture,
    };
    app.layout(ui);
    if app.core.accounts().is_empty() {
        app.banner("No accounts are configured. Add one in the egui esMail first.");
    }
    for issue in issues {
        let name = app.core.accounts()[issue.account].display_name.clone();
        app.banner(&format!("{name}: {}", issue.message));
    }
    app
}

impl win32ui::App for App {
    type Msg = Msg;

    fn update(&mut self, msg: Msg, ui: &mut Ui<Msg>) {
        match msg {
            Msg::Wake => self.drain(ui),
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
            Msg::NearEnd => self.request_page(),
            Msg::Link(href) => self.set_status(&format!("Links are not opened in this prototype: {href}")),
            Msg::SetTheme(choice) => {
                self.theme = choice;
                let theme = chrome::palette(choice);
                self.reader.set_palette(palette_for(&theme));
                ui.set_theme(theme);
                ui.set_menu_bar(chrome::menu_bar(choice, self.original_colours));
            }
            Msg::OriginalColours(original) => {
                self.original_colours = original;
                self.reader.set_original_colours(original);
                ui.set_menu_bar(chrome::menu_bar(self.theme, original));
            }
            Msg::Refresh => self.refresh(ui),
            Msg::ToggleFlag => self.toggle_flag(),
            Msg::SetSeen(seen) => self.set_seen(seen),
            Msg::Archive => self.archive(),
            Msg::Delete => self.delete(),
            Msg::Context => ui.popup(&chrome::message_menu(), ui.cursor_position()),
            Msg::Quit => ui.quit(),
            Msg::Timer(id) => self.timer(ui, id),
        }
    }
}

impl App {
    fn layout(&self, ui: &Ui<Msg>) {
        ui.set_layout(column![
            split_row![self.tree, split_row![self.list, self.reader].position(dip(420.0)).min(dip(280.0), dip(320.0))]
                .position(dip(230.0))
                .min(dip(140.0), dip(600.0)),
            self.status,
        ]);
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
        } else if self.capture.as_ref().is_some_and(|(timer, _)| *timer == id) {
            self.tick(ui);
        }
    }

    /// Screenshot mode: check whether the window is ready to capture.
    fn tick(&mut self, ui: &mut Ui<Msg>) {
        let message_ready = self.message_shown && self.reader.is_ready();
        let loaded = self.open.as_ref().is_some_and(|o| o.loaded() > 0);
        let expects_message = self.select_after_load.is_some() || self.selected.is_some();
        let Some((_, capture)) = self.capture.as_mut() else { return };
        match capture.step(ui, loaded && (!expects_message || message_ready)) {
            Step::Wait => {}
            Step::Repaint => {
                self.list.invalidate();
                self.reader.invalidate();
            }
            Step::Capture => capture.finish(ui),
        }
    }
}
