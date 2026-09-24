//! The `esmail-win32` window: folders on the left, the message list in the
//! middle, the reading pane on the right, a status bar below.
//!
//! Everything slow happens off the UI thread. IMAP sessions live on the core's
//! runtime and wake the window through a `Proxy`; the handler for that wake
//! drains the events (`Core::pump`) and updates the widgets. Message bodies are
//! fetched by the actor's body worker and rendered by the HTML view's own
//! thread, so a selection change never waits for either.

mod args;
mod chrome;
mod screenshot;
mod tree;

use std::sync::Arc;

use esmail::imap::{ImapCommand, ImapEvent, MailHeader};
use litehtml_view_d2d::{HtmlView, HtmlViewEvent};
use win32ui::prelude::*;
use win32ui::{column, split_row};

use esmail_win32::MessageList;
use esmail_win32::core_glue::mailbox::OpenFolder;
use esmail_win32::core_glue::{BodyLoads, Core, FolderRef, FolderTree, load_config, reading};
use args::{Args, ThemeChoice};
use screenshot::{Capture, Step};

/// Messages the window's widgets and the core raise.
enum Msg {
    /// The core has events to drain.
    Wake,
    /// The HTML view finished a frame.
    Frame,
    Folder(i64),
    Selected(Vec<usize>),
    NearEnd,
    Link(String),
    SetTheme(ThemeChoice),
    Refresh,
    Quit,
    /// Screenshot mode only: check whether the window is ready to capture.
    Tick,
}

/// `(account, mailbox, uid)`: which message a body fetch is for.
type BodyKey = (usize, String, u32);

struct App {
    core: Core,
    folders: FolderTree,
    list: MessageList<Msg>,
    tree: TreeView<Msg>,
    reader: HtmlView<Msg>,
    status: StatusBar<Msg>,
    /// The folder shown in the list, once one has been opened.
    open: Option<OpenFolder>,
    /// Which folder to open when an account's folder list first arrives.
    wanted_folder: Option<String>,
    bodies: BodyLoads<BodyKey>,
    /// The message whose body is (being) shown.
    selected: Option<MailHeader>,
    /// The selected message's body is on screen (screenshots wait for this).
    message_shown: bool,
    select_after_load: Option<usize>,
    capture: Option<Capture>,
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

fn build(ui: &mut Ui<Msg>, args: &Args, config: &esmail::config::Config) -> App {
    let proxy = ui.proxy();
    let waker: esmail::waker::Waker = Arc::new(move || {
        let _ = proxy.send(Msg::Wake);
    });
    let (core, issues) = Core::start(config, waker).expect("start the async runtime");

    let folders = FolderTree::new(config.accounts.iter().map(|a| a.display_name.clone()));
    let tree = tree::build(ui, &folders).expect("folder tree");
    let list = MessageList::new(ui)
        .expect("message list")
        .on_select(|rows| Some(Msg::Selected(rows.to_vec())))
        .on_near_end(|| Some(Msg::NearEnd));
    let reader = HtmlView::new(
        ui,
        reading::notice(""),
        || Msg::Frame,
        |event| match event {
            HtmlViewEvent::LinkClicked(href) => Some(Msg::Link(href)),
        },
    )
    .expect("reading pane");
    let status = StatusBar::new(ui).expect("status bar");
    status.set_parts(&[-1]);

    ui.set_menu_bar(chrome::menu_bar(args.theme));
    ui.accelerator(Shortcut::key(Key::F5), || Some(Msg::Refresh));
    ui.accelerator(Shortcut::ctrl(Key::Q), || Some(Msg::Quit));
    if args.screenshot.is_some() {
        let timer = ui.set_timer(50).expect("screenshot timer");
        ui.on_timer(move |id| (id == timer).then_some(Msg::Tick));
    }

    let app = App {
        core,
        folders,
        list,
        tree,
        reader,
        status,
        open: None,
        wanted_folder: args.folder.clone(),
        bodies: BodyLoads::default(),
        selected: None,
        message_shown: false,
        select_after_load: args.select,
        capture: args.screenshot.clone().map(Capture::new),
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
                if let Some(folder) = self.folders.selection(id) {
                    self.open_folder(ui, folder);
                }
            }
            Msg::Selected(rows) => self.select(&rows),
            Msg::NearEnd => self.request_page(),
            Msg::Link(href) => self.set_status(&format!("Links are not opened in this prototype: {href}")),
            Msg::SetTheme(choice) => {
                ui.set_theme(chrome::palette(choice));
                ui.set_menu_bar(chrome::menu_bar(choice));
            }
            Msg::Refresh => {
                if let Some(folder) = self.open.as_ref().map(|open| open.folder().clone()) {
                    self.open_folder(ui, folder);
                }
            }
            Msg::Quit => ui.quit(),
            Msg::Tick => self.tick(ui),
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
    fn banner(&self, text: &str) {
        self.set_status(&format!("Error: {text}"));
        if self.selected.is_none() {
            self.reader.load(reading::notice(text));
        }
    }

    fn drain(&mut self, ui: &mut Ui<Msg>) {
        let mut tree_changed = false;
        for (account, event) in self.core.pump() {
            tree_changed |= self.handle(ui, account, event);
        }
        if tree_changed {
            match tree::build(ui, &self.folders) {
                Ok(tree) => {
                    self.tree = tree;
                    self.layout(ui);
                }
                Err(error) => self.banner(&format!("could not rebuild the folder tree: {error}")),
            }
        }
    }

    /// Applies one event. Returns whether the folder tree needs rebuilding.
    fn handle(&mut self, ui: &mut Ui<Msg>, account: usize, event: ImapEvent) -> bool {
        match event {
            ImapEvent::Connected => {
                self.set_status("Connected");
                self.core.send(account, ImapCommand::FetchMailboxes);
            }
            ImapEvent::Disconnected => self.set_status("Disconnected, reconnecting..."),
            ImapEvent::Error(error) => {
                if let Some(open) = self.open.as_mut() {
                    open.page_failed();
                }
                self.banner(&error);
            }
            ImapEvent::Mailboxes(mailboxes) => {
                self.folders.set_mailboxes(account, &mailboxes);
                self.core.send(account, ImapCommand::FetchUnreadCounts { mailboxes: self.folders.mailbox_names(account) });
                self.open_first_folder(ui, account);
                return true;
            }
            ImapEvent::UnreadCounts(counts) => {
                self.folders.set_unread(account, counts);
                return true;
            }
            ImapEvent::Headers { mailbox, headers, page, total_pages, req_id, .. } => {
                self.apply_headers(account, &mailbox, req_id, page, total_pages, headers);
            }
            ImapEvent::Body { uid, html, attachments, req_id } => {
                self.body_arrived(account, uid, req_id, |header| reading::document(header, &html, &attachments));
            }
            ImapEvent::BodyFailed { uid, req_id, error } => {
                self.body_arrived(account, uid, req_id, |_| reading::notice(&format!("Could not load this message: {error}")));
            }
            _ => {}
        }
        false
    }

    fn open_first_folder(&mut self, ui: &mut Ui<Msg>, account: usize) {
        if self.open.is_some() {
            return;
        }
        let wanted = self.wanted_folder.clone().unwrap_or_else(|| "INBOX".to_string());
        let mailbox = self.folders.mailbox_names(account).into_iter().find(|name| name.eq_ignore_ascii_case(&wanted));
        if let Some(mailbox) = mailbox {
            self.open_folder(ui, FolderRef { account, mailbox });
        }
    }

    fn open_folder(&mut self, ui: &Ui<Msg>, folder: FolderRef) {
        self.bodies.cancel();
        self.selected = None;
        ui.set_title(&format!("{} - esMail", folder.mailbox));
        self.set_status(&format!("Loading {}...", folder.mailbox));
        self.list.set_rows(Arc::from([]));
        self.reader.load(reading::notice("Select a message to read it."));
        self.open = Some(OpenFolder::new(folder));
        self.request_page();
    }

    fn request_page(&mut self) {
        let Some(open) = self.open.as_mut() else { return };
        let Some(request) = open.next_request() else { return };
        let folder = open.folder().clone();
        let sent = self.core.send(folder.account, ImapCommand::FetchHeaders { mailbox: folder.mailbox, page: request.page, req_id: request.id });
        if !sent {
            open.page_failed();
            self.banner("this account is not connected");
        }
    }

    fn apply_headers(&mut self, account: usize, mailbox: &str, req_id: u64, page: u32, total_pages: u32, headers: Vec<MailHeader>) {
        let Some(open) = self.open.as_mut().filter(|o| o.folder().account == account && o.folder().mailbox == mailbox) else { return };
        let Some(added) = open.apply_page(req_id, page, total_pages, headers) else { return };
        if page == 1 {
            self.list.set_rows(open.rows());
        } else {
            self.list.extend_rows(open.rows());
        }
        let loaded = open.loaded();
        let more = if open.is_complete() { "" } else { " (scroll for older)" };
        self.set_status(&format!("{mailbox}: {loaded} messages{more}"));
        if page == 1 && added > 0 {
            if let Some(row) = self.select_after_load.take() {
                self.list.set_selection(&[row]);
                self.list.focus();
                self.select(&[row]);
            }
        }
    }

    fn select(&mut self, rows: &[usize]) {
        let [row] = rows else {
            self.bodies.cancel();
            return;
        };
        let Some((folder, header)) = self.open.as_ref().and_then(|o| Some((o.folder().clone(), o.header(*row)?.clone()))) else { return };
        let key = (folder.account, folder.mailbox, header.uid);
        self.set_status("Loading message...");
        self.selected = Some(header);
        self.message_shown = false;
        if let Some((key, id)) = self.bodies.want(key) {
            self.fetch_body(key, id);
        }
    }

    fn fetch_body(&mut self, (account, mailbox, uid): BodyKey, req_id: u64) {
        if !self.core.send(account, ImapCommand::FetchBody { mailbox, uid, req_id }) {
            self.banner("this account is not connected");
        }
    }

    /// A body (or its failure) came back: show it if it is still the message
    /// the user wants, and start the next wanted fetch.
    fn body_arrived(&mut self, account: usize, uid: u32, req_id: u64, document: impl FnOnce(&MailHeader) -> String) {
        let Some(mailbox) = self.open.as_ref().map(|o| o.folder().mailbox.clone()) else { return };
        let finished = self.bodies.finished(&(account, mailbox, uid), req_id);
        if finished.show {
            if let Some(header) = self.selected.as_ref() {
                self.reader.load(document(header));
                self.message_shown = true;
                self.set_status("Ready");
            }
        }
        if let Some((key, id)) = finished.next {
            self.fetch_body(key, id);
        }
    }

    fn tick(&mut self, ui: &mut Ui<Msg>) {
        let Some(capture) = self.capture.as_mut() else { return };
        let message_ready = self.message_shown && self.reader.is_ready();
        let loaded = self.open.as_ref().is_some_and(|o| o.loaded() > 0);
        let expects_message = self.select_after_load.is_some() || self.selected.is_some();
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
