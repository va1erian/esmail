//! The Settings window: the Google OAuth client that "Sign in with Google"
//! needs, matching the egui app's Settings > Google page, plus a shortcut
//! reference.
//!
//! The id and secret live in the shared `config.toml` (`[google_oauth]`), so
//! both frontends sign in with the same client. Like the account windows it
//! runs its own `App` and asks the main window to save, which owns the config
//! and the background writer.

use esmail::oauth::ClientSource;
use win32ui::prelude::*;
use win32ui::row;

use crate::app::{Msg, chrome};

/// How wide the field captions are, in dips.
const CAPTION_WIDTH: f32 = 110.0;
/// Height of one field row, in dips.
const ROW_HEIGHT: f32 = 28.0;

/// What the window asks the main window for.
pub enum Request {
    /// Save these Google OAuth client fields; an empty id clears them.
    Save { client_id: String, client_secret: String },
    /// The window is gone.
    Closed,
}

/// The messages of the Settings window.
pub enum SettingsMsg {
    /// The Save button, or Enter.
    Save,
    /// The close box or Esc.
    Cancel,
    /// The theme to wear, and whether to keep following the system's.
    SetTheme(Theme, bool),
    /// The main window saved the fields: whether a client is configured now,
    /// and where it comes from.
    Saved { configured: bool, source: Option<ClientSource> },
}

/// What opening the window needs.
pub struct Init {
    /// The saved Google client id, if any.
    pub client_id: String,
    /// The saved Google client secret, if any.
    pub client_secret: String,
    /// Where the Google client in use comes from, if one is configured.
    pub source: Option<ClientSource>,
    /// The main window's queue.
    pub host: Proxy<Msg>,
    /// Keep following the system theme.
    pub follow_system_theme: bool,
    /// `--acrylic`.
    pub acrylic: bool,
    /// A `--profile` run only reads a copy of the config, so saving is off.
    pub editable: bool,
}

/// The keyboard shortcuts, as the egui Settings > General page lists them.
const SHORTCUTS: [(&str, &str); 8] = [
    ("Ctrl+F", "Focus the search box"),
    ("Ctrl+N", "Write a new message"),
    ("j / k", "Next / previous message"),
    ("Enter", "Open the selected message"),
    ("r", "Reply"),
    ("a", "Archive"),
    ("f", "Flag or unflag"),
    ("Del", "Move to the trash folder"),
];

/// The static labels, kept alive for as long as the window is: a label's child
/// window is destroyed with it.
struct Captions {
    intro: Label,
    heading: Label,
    id: Label,
    secret: Label,
    spacer: Label,
    /// One label per shortcut, in `SHORTCUTS` order.
    keys: Vec<Label>,
    what: Vec<Label>,
}

struct SettingsApp {
    host: Proxy<Msg>,
    client_id: Edit<SettingsMsg>,
    client_secret: Edit<SettingsMsg>,
    note: Label,
    status: Label,
    source: Option<ClientSource>,
    editable: bool,
    /// The buttons, kept alive.
    _widgets: Vec<Box<dyn AsControl>>,
    /// The static text, kept alive.
    _captions: Captions,
}

/// Opens the window owned by `ui`'s window.
pub fn open(ui: &Ui<Msg>, init: Init) -> win32ui::Result<WindowHandle<SettingsMsg>> {
    let spec = chrome::acrylic(WindowSpec::new("Settings").size(dip(560.0), dip(520.0)), init.acrylic);
    ui.open_window::<SettingsApp, _>(spec, move |ui| SettingsApp::new(ui, init))
}

fn caption(ui: &mut Ui<SettingsMsg>, text: &str) -> Label {
    Label::new(ui, Rect::default(), text).expect("label")
}

impl SettingsApp {
    fn new(ui: &mut Ui<SettingsMsg>, init: Init) -> SettingsApp {
        let client_id = Edit::single_line(ui).expect("client id").cue("...apps.googleusercontent.com");
        let client_secret = Edit::password(ui).expect("client secret").on_submit(|| Some(SettingsMsg::Save));
        let note = caption(ui, "");
        let status = caption(ui, "");
        client_id.set_text(&init.client_id);
        client_secret.set_text(&init.client_secret);
        let save = Button::new(ui, "Save").expect("save button").on_click(|| Some(SettingsMsg::Save)).default();
        let cancel = Button::new(ui, "Close").expect("close button").on_click(|| Some(SettingsMsg::Cancel));
        if init.editable {
            client_id.focus();
        } else {
            client_id.set_read_only(true);
            client_secret.set_read_only(true);
            save.set_enabled(false);
            status.set_text("Settings cannot be saved while a --profile is open.");
        }

        let c = Captions {
            intro: caption(ui, "Create a \"Desktop app\" OAuth client in Google Cloud Console and enter it here. Google issues sign-in tokens only to registered applications; this program cannot supply its own."),
            heading: caption(ui, "Keyboard shortcuts"),
            id: caption(ui, "Client ID"),
            secret: caption(ui, "Client secret"),
            spacer: caption(ui, ""),
            keys: SHORTCUTS.iter().map(|(key, _)| caption(ui, key)).collect(),
            what: SHORTCUTS.iter().map(|(_, what)| caption(ui, what)).collect(),
        };

        let mut rows = Vec::new();
        rows.push(c.intro.height(dip(58.0)));
        rows.push(row![c.id.width(dip(CAPTION_WIDTH)), client_id.fill(1)].spacing(dip(6.0)).height(dip(ROW_HEIGHT)));
        rows.push(row![c.secret.width(dip(CAPTION_WIDTH)), client_secret.fill(1)].spacing(dip(6.0)).height(dip(ROW_HEIGHT)));
        rows.push(note.height(dip(22.0)));
        rows.push(status.height(dip(22.0)));
        rows.push(c.heading.height(dip(22.0)));
        for (key, what) in c.keys.iter().zip(&c.what) {
            rows.push(row![key.width(dip(90.0)), what.fill(1)].spacing(dip(6.0)).height(dip(20.0)));
        }
        rows.push(row![c.spacer.fill(1), save.width(dip(90.0)), cancel.width(dip(90.0))].spacing(dip(8.0)).height(dip(32.0)));
        let margins = Insets::new(dip(16.0), dip(16.0) + ui.title_bar_height(), dip(16.0), dip(16.0));
        ui.set_layout(rows.into_iter().fold(Layout::column(), Layout::item).spacing(dip(6.0)).margins(margins));
        ui.accelerator(Shortcut::key(Key::ESCAPE), || Some(SettingsMsg::Cancel));
        ui.on_close(|| Some(SettingsMsg::Cancel));
        ui.follow_system_theme(init.follow_system_theme);
        let app = SettingsApp {
            host: init.host,
            client_id,
            client_secret,
            note,
            status,
            source: init.source,
            editable: init.editable,
            _widgets: vec![Box::new(save), Box::new(cancel)],
            _captions: c,
        };
        app.update_note();
        app
    }

    /// The line under the fields: where the client in use comes from.
    fn update_note(&self) {
        self.note.set_text(match self.source {
            Some(ClientSource::Environment) => "The ESMAIL_GOOGLE_CLIENT_ID environment variable is set and takes precedence over these fields.",
            Some(ClientSource::Config) => "Using the saved client.",
            Some(ClientSource::Build) => "Using the client built into this program.",
            None => "No Google OAuth client is configured, so Sign in with Google is unavailable.",
        });
    }

    fn save(&self) {
        if !self.editable {
            self.status.set_text("Settings cannot be saved while a --profile is open.");
            return;
        }
        let client_id = self.client_id.text().trim().to_string();
        let client_secret = self.client_secret.text().trim().to_string();
        let _ = self.host.send(Msg::SettingsRequest(Request::Save { client_id, client_secret }));
    }

    /// The main window saved the fields: report it, and whether they took.
    fn saved(&mut self, configured: bool, source: Option<ClientSource>) {
        self.source = source;
        self.update_note();
        self.status.set_text(if configured {
            "Saved. Google sign-in is available."
        } else {
            "Saved. No client id, so Google sign-in is off."
        });
    }
}

impl App for SettingsApp {
    type Msg = SettingsMsg;

    fn update(&mut self, msg: SettingsMsg, ui: &mut Ui<SettingsMsg>) {
        match msg {
            SettingsMsg::Save => self.save(),
            SettingsMsg::Cancel => ui.close(),
            SettingsMsg::SetTheme(theme, follow) => {
                ui.follow_system_theme(follow);
                ui.set_theme(theme);
            }
            SettingsMsg::Saved { configured, source } => self.saved(configured, source),
        }
    }
}

impl Drop for SettingsApp {
    fn drop(&mut self) {
        let _ = self.host.send(Msg::SettingsRequest(Request::Closed));
    }
}
