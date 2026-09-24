//! Starting the program: the command line, the single-instance claim, and
//! building the window and its widgets.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use esmail_win32::MessageList;
use esmail_win32::core_glue::{BodyLoads, Core, FolderTree, Latest, Settings, ThemeChoice, WindowState, load_config};
use win32ui::prelude::*;

use super::accounts::Accounts;
use super::args::Args;
use super::composes::{self, Composes};
use super::instance::{self, Claim, Waiting};
use super::message::SeenTimer;
use super::reader::Reader;
use super::reader_bar::ReaderBar;
use super::screenshot::Capture;
use super::search::SearchState;
use super::startup::Startup;
use super::toolbar::MainBar;
use super::tray::Tray;
use super::tree::{self, SharedFolders};
use super::{App, Msg, chrome, notifications, palette_for, placement, theme};

pub(crate) fn main() {
    let began = Instant::now();
    let args = match Args::parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    // Screenshot and profile runs are throwaway views: they neither hand over to a
    // running window nor keep one from starting, and have no tray.
    let resident = args.screenshot.is_none() && args.profile.is_none();
    let waiting = if resident {
        match instance::claim(args.compose.is_some()) {
            Ok(Claim::First(waiting)) => waiting,
            Ok(Claim::HandedOver) => return,
            Err(error) => {
                eprintln!("esmail-win32: could not hand over to the running window: {error}");
                std::process::exit(1);
            }
        }
    } else {
        None
    };
    let config = match load_config(args.profile.as_deref()) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("esmail-win32: {message}");
            std::process::exit(1);
        }
    };
    let settings_path = resident.then(Settings::path).flatten();
    let mut settings = settings_path.as_deref().map(Settings::load).unwrap_or_default();
    if let Some(theme) = args.theme {
        settings.theme = theme;
    }
    settings.remote_images |= args.remote_images;

    let spec = chrome::acrylic(WindowSpec::new("esMail").size(dip(1200.0), dip(760.0)).theme(chrome::theme(settings.theme)), args.acrylic);
    let session = Session { settings, settings_path, waiting };
    let result = win32ui::run_app(spec, |ui| build(ui, &args, &config, began, session));
    if let Err(error) = result {
        eprintln!("esmail-win32 failed: {error}");
        std::process::exit(1);
    }
}

/// What `main` worked out before the window existed.
struct Session {
    settings: Settings,
    /// Where the settings are kept; none in throwaway (`--screenshot`, `--profile`) runs.
    settings_path: Option<std::path::PathBuf>,
    /// Set in the first instance: forwards what later launches ask for.
    waiting: Option<Waiting>,
}

fn build(ui: &mut Ui<Msg>, args: &Args, config: &esmail::config::Config, began: Instant, session: Session) -> App {
    let Session { settings, settings_path, waiting } = session;
    let notify = if settings_path.is_some() {
        notifications::install(ui.proxy());
        notifications::hook(ui.hwnd().raw())
    } else {
        Arc::new(|_: &str, _: &str, _: &str| {})
    };
    let (core, issues) = Core::start(config, waker(ui), notify.clone()).expect("start the async runtime");
    let tray = settings_path.is_some().then(|| Tray::new(ui.proxy())).and_then(|tray| match tray {
        Ok(tray) => Some(tray),
        Err(error) => {
            log::warn!("no tray icon, so closing the window quits: {error}");
            None
        }
    });
    if let Some(waiting) = waiting {
        waiting.forward(ui.proxy());
    }

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
    let toolbar = MainBar::new(ui, settings.theme).expect("main toolbar");
    let reader_bar = ReaderBar::new(ui).expect("reader action bar");
    let status = StatusBar::new(ui).expect("status bar");
    status.set_parts(&[-1]);

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
        config: config.clone(),
        accounts: Accounts::default(),
        editable: args.profile.is_none(),
        folders,
        list,
        search_edit,
        search: SearchState::default(),
        tree,
        reader,
        toolbar,
        reader_bar,
        status,
        theme: settings.theme,
        original_colours: false,
        open: None,
        wanted_folder: args.folder.clone(),
        wanted_account: args.account,
        bodies: BodyLoads::default(),
        selected: None,
        selected_in: None,
        seen: SeenTimer::default(),
        action_ids: Latest::default(),
        pending_actions: 0,
        message_shown: false,
        select_after_load: args.select,
        capture,
        window,
        window_path,
        startup: Startup::new(began),
        composes: Composes::default(),
        remote_images: settings.remote_images,
        theme_poll: None,
        outbox_poll: ui.set_timer(composes::OUTBOX_POLL_MILLIS).ok(),
        acrylic: args.acrylic,
        opened_accounts: Default::default(),
        start_compose: args.compose,
        settings,
        settings_path,
        tray,
        hidden: false,
        title_unread: 0,
        notify,
    };
    app.layout(ui);
    app.refresh_menu(ui);
    app.reader.set_remote_images(app.remote_images);
    ui.follow_system_theme(app.theme == ThemeChoice::System);
    if app.theme == ThemeChoice::System {
        app.theme_poll = ui.set_timer(theme::POLL_MILLIS).ok();
    }
    if let Some(bounds) = app.window.bounds {
        placement::restore(ui.hwnd(), bounds, app.window.maximized);
    }
    app.accounts.reset_status(app.core.accounts().len());
    if app.core.accounts().is_empty() {
        app.reader.show_notice("No accounts are set up. Use File > Add account... to add one.");
        ui.emit(Msg::AddAccount);
    }
    if args.accounts {
        ui.emit(Msg::ManageAccounts);
    }
    for issue in issues {
        app.account_failed(issue.account, issue.message);
    }
    app.open_from_cache(ui);
    app.core.cache().due_outbox();
    app
}

/// What makes the window drain the core: called from any thread.
pub(super) fn waker(ui: &Ui<Msg>) -> esmail::waker::Waker {
    let proxy = ui.proxy();
    Arc::new(move || {
        let _ = proxy.send(Msg::Wake);
    })
}
