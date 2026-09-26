//! A compose window: one secondary window per message.
//!
//! The window runs its own `App` (xui-win32 gives every window its own message
//! queue) and never touches the mail core. It edits a message and asks the main
//! window, through a `Proxy`, to send it, save it as a draft or drop it; the
//! main window answers with [`ComposeMsg::Sending`], [`ComposeMsg::Failed`]
//! and [`ComposeMsg::Sent`]. A failed send keeps the window and the typed text.

mod attachments;
mod form;
mod recipients;

use std::rc::Rc;

use esmail::compose::{ComposeId, ComposeState};
use esmail::contacts::Contacts;
use esmail_win32::core_glue::compose::{has_content, window_title};
use xui_win32::prelude::*;

use super::{Msg, chrome};
use attachments::{FileRead, pick_and_read};
use form::{Form, Panels};
use recipients::{Suggester, Typing, accept};

/// How often a changed message is autosaved as a draft.
const AUTOSAVE_MILLIS: u32 = 30_000;

/// What a compose window asks the main window for.
pub enum Request {
    /// Send this message.
    Send(ComposeState),
    /// Write this message as a draft (`explicit`: the user asked, so say so).
    SaveDraft { state: ComposeState, explicit: bool },
    /// The user threw the message away: forget its draft and outbox row.
    Discard,
    /// The window is gone.
    Closed,
}

/// The fields the user types into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    To,
    Cc,
    Bcc,
    Subject,
    Body,
}

impl Field {
    fn is_recipient(self) -> bool {
        matches!(self, Field::To | Field::Cc | Field::Bcc)
    }

    /// The field Enter moves to.
    fn next(self) -> Field {
        match self {
            Field::To => Field::Cc,
            Field::Cc => Field::Bcc,
            Field::Bcc => Field::Subject,
            Field::Subject | Field::Body => Field::Body,
        }
    }
}

/// The messages of a compose window.
pub enum ComposeMsg {
    Changed(Field),
    Focused(Field, bool),
    /// Enter in a single-line field.
    Submit(Field),
    FromChanged,
    /// A suggestion row was clicked.
    Suggestion(usize),
    /// Ctrl+Space.
    AcceptFirst,
    AddAttachment,
    RemoveAttachment,
    AttachmentsRead(Vec<FileRead>),
    Send,
    SaveDraft,
    Discard,
    /// The window's close box.
    CloseRequested,
    /// The autosave timer.
    Timer,
    /// The main window queued the message.
    Sending,
    /// The send failed; the message is kept.
    Failed(String),
    /// The message went out: the window is done.
    Sent,
    /// The draft is written.
    DraftSaved,
    /// The theme to wear, and whether to keep following the system's.
    SetTheme(Theme, bool),
}

/// What opening a window needs.
pub struct Init {
    pub id: ComposeId,
    pub state: ComposeState,
    /// `(config id, label)` of each account the message can be sent from.
    pub accounts: Vec<(String, String)>,
    pub contacts: Rc<Contacts>,
    /// The main window's queue.
    pub host: Proxy<Msg>,
    /// Start in the text (a reply) rather than at the recipient.
    pub body_first: bool,
    pub follow_system_theme: bool,
    /// `--acrylic`.
    pub acrylic: bool,
}

/// What the user typed, for telling whether there is anything to save.
#[derive(PartialEq)]
struct Typed {
    from: Option<usize>,
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    body: String,
    attachments: usize,
}

struct ComposeApp {
    id: ComposeId,
    host: Proxy<Msg>,
    form: Form,
    /// The account ids, in the order of the From list.
    accounts: Vec<String>,
    /// The message's threading headers and draft id; its text is in the form.
    state: ComposeState,
    attachments: Vec<(String, Vec<u8>)>,
    suggester: Suggester,
    /// The recipient field the suggestions are for.
    active: Option<Field>,
    /// What was last saved as a draft (or the message opened with).
    saved: Typed,
    /// Which optional lists are open.
    panels: Panels,
    sending: bool,
}

/// Opens a compose window owned by `ui`'s window.
pub fn open(ui: &Ui<Msg>, init: Init) -> xui_win32::Result<WindowHandle<ComposeMsg>> {
    let title = window_title(&init.state.subject);
    let spec = chrome::acrylic(WindowSpec::new(title).size(dip(760.0), dip(640.0)), init.acrylic);
    ui.open_window::<ComposeApp, _>(spec, move |ui| ComposeApp::new(ui, init))
}

impl ComposeApp {
    fn new(ui: &mut Ui<ComposeMsg>, init: Init) -> ComposeApp {
        let labels: Vec<String> = init.accounts.iter().map(|(_, label)| label.clone()).collect();
        let form = Form::build(ui, &labels).expect("compose window widgets");
        let account = init.state.account_id.as_ref().and_then(|id| init.accounts.iter().position(|(account, _)| account == id));
        form.from.set_selected(&account.unwrap_or(0));
        form.to.set_text(&init.state.to);
        form.cc.set_text(&init.state.cc);
        form.bcc.set_text(&init.state.bcc);
        form.subject.set_text(&init.state.subject);
        form.body.set_text(&init.state.body);
        let attachments = init.state.attachments.clone();
        form.attachments.show(&attachments);
        let panels = Panels { suggestions: false, attachments: !attachments.is_empty() };
        form.arrange(ui, panels);
        let mut state = init.state;
        state.attachments = Vec::new();

        ui.accelerator(Shortcut::ctrl(Key::RETURN), || Some(ComposeMsg::Send));
        ui.accelerator(Shortcut::ctrl(Key::S), || Some(ComposeMsg::SaveDraft));
        ui.accelerator(Shortcut::ctrl(Key::SPACE), || Some(ComposeMsg::AcceptFirst));
        ui.on_close(|| Some(ComposeMsg::CloseRequested));
        ui.on_timer(|_| Some(ComposeMsg::Timer));
        let _ = ui.set_timer(AUTOSAVE_MILLIS);
        ui.follow_system_theme(init.follow_system_theme);

        let mut app = ComposeApp {
            id: init.id,
            host: init.host,
            form,
            accounts: init.accounts.into_iter().map(|(id, _)| id).collect(),
            state,
            attachments,
            suggester: Suggester::new(init.contacts),
            active: None,
            saved: Typed { from: None, to: String::new(), cc: String::new(), bcc: String::new(), subject: String::new(), body: String::new(), attachments: 0 },
            sending: false,
            panels,
        };
        app.saved = app.typed();
        if init.body_first {
            app.form.body.focus();
            app.form.body.set_selection(0..0);
        } else {
            app.form.to.focus();
        }
        app
    }

    fn typed(&self) -> Typed {
        Typed {
            from: self.form.from.selected().copied(),
            to: self.form.to.text(),
            cc: self.form.cc.text(),
            bcc: self.form.bcc.text(),
            subject: self.form.subject.text(),
            body: self.form.body.text(),
            attachments: self.attachments.len(),
        }
    }

    /// Whether the message differs from what was last saved.
    fn edited(&self) -> bool {
        self.typed() != self.saved
    }

    /// The message as it stands now.
    fn snapshot(&self) -> ComposeState {
        let mut state = self.state.clone();
        state.to = self.form.to.text();
        state.cc = self.form.cc.text();
        state.bcc = self.form.bcc.text();
        state.subject = self.form.subject.text();
        state.body = self.form.body.text();
        state.attachments = self.attachments.clone();
        state.account_id = self.form.from.selected().and_then(|index| self.accounts.as_slice().get(*index)).cloned();
        state
    }

    fn ask(&self, request: Request) {
        let _ = self.host.send(Msg::ComposeRequest(self.id, request));
    }

    fn show_status(&self, ui: &Ui<ComposeMsg>, text: &str) {
        self.form.status.set_text(text);
        self.form.status.set_visible(!text.is_empty());
        ui.relayout();
    }

    fn save_draft(&mut self, explicit: bool) {
        self.ask(Request::SaveDraft { state: self.snapshot(), explicit });
        self.saved = self.typed();
    }

    fn send(&mut self) {
        if self.sending {
            return;
        }
        self.ask(Request::Send(self.snapshot()));
    }

    fn discard(&mut self, ui: &Ui<ComposeMsg>) {
        if has_content(&self.snapshot()) && !confirm(ui, "Discard this message?", "It will not be saved.", "Discard") {
            return;
        }
        self.ask(Request::Discard);
        ui.close();
    }

    fn close_requested(&mut self, ui: &Ui<ComposeMsg>) {
        if self.sending || !self.edited() {
            return ui.close();
        }
        let choice = TaskDialog::new("Save this message as a draft?")
            .content("It has changes that are not saved.")
            .buttons([("Save draft", Close::Save), ("Cancel", Close::Cancel), ("Discard", Close::Discard)])
            .default(Close::Save)
            .icon(TaskDialogIcon::Warning)
            .show(ui)
            .map_or(Close::Cancel, |(choice, _)| choice);
        match choice {
            Close::Save => {
                self.save_draft(true);
                ui.close();
            }
            Close::Discard => {
                self.ask(Request::Discard);
                ui.close();
            }
            Close::Cancel => {}
        }
    }

    fn changed(&mut self, ui: &Ui<ComposeMsg>, field: Field) {
        if field == Field::Subject {
            ui.set_title(&window_title(&self.form.subject.text()));
        }
        if field.is_recipient() {
            self.active = Some(field);
            self.suggest(ui);
        }
    }

    /// Recomputes the suggestions for the active recipient field.
    fn suggest(&mut self, ui: &Ui<ComposeMsg>) {
        let Some(field) = self.active else { return };
        let text = self.form.edit(field).text();
        let caret = self.form.edit(field).selection().end;
        let shown = self.suggester.update(&Typing { text: &text, caret }).to_vec();
        self.show_suggestions(ui, shown);
    }

    fn show_suggestions(&mut self, ui: &Ui<ComposeMsg>, shown: Vec<String>) {
        self.panels.suggestions = !shown.is_empty();
        self.form.arrange(ui, self.panels);
        self.form.suggestions.set_model(shown);
    }

    fn accept_suggestion(&mut self, ui: &Ui<ComposeMsg>, index: usize) {
        let (Some(field), Some(suggestion)) = (self.active, self.suggester.get(index)) else { return };
        let text = self.form.edit(field).text();
        let (completed, caret) = accept(&Typing { text: &text, caret: self.form.edit(field).selection().end }, suggestion);
        self.suggester.clear();
        self.show_suggestions(ui, Vec::new());
        let edit = self.form.edit(field);
        edit.set_text(&completed);
        edit.focus();
        edit.set_selection(caret..caret);
    }

    fn attachments_read(&mut self, ui: &Ui<ComposeMsg>, read: Vec<FileRead>) {
        let mut problems = Vec::new();
        for file in read {
            match file {
                Ok(attachment) => self.attachments.push(attachment),
                Err(problem) => problems.push(problem),
            }
        }
        self.panels.attachments = !self.attachments.is_empty();
        self.form.arrange(ui, self.panels);
        self.form.attachments.show(&self.attachments);
        self.show_status(ui, &problems.join("\n"));
    }

    fn remove_attachment(&mut self, ui: &Ui<ComposeMsg>) {
        let Some(row) = self.form.attachments.selected().filter(|row| *row < self.attachments.len()) else { return };
        self.attachments.remove(row);
        self.panels.attachments = !self.attachments.is_empty();
        self.form.arrange(ui, self.panels);
        self.form.attachments.show(&self.attachments);
    }

    fn timer(&mut self) {
        if !self.sending && self.edited() && has_content(&self.snapshot()) {
            self.save_draft(false);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Close {
    Save,
    Cancel,
    Discard,
}

/// A yes/no question; `false` on Cancel or a dialog that could not be shown.
fn confirm(ui: &Ui<ComposeMsg>, question: &str, detail: &str, yes: &str) -> bool {
    TaskDialog::new(question)
        .content(detail)
        .buttons([(yes, true), ("Cancel", false)])
        .default(false)
        .icon(TaskDialogIcon::Warning)
        .show(ui)
        .is_ok_and(|(answer, _)| answer)
}

impl App for ComposeApp {
    type Msg = ComposeMsg;

    fn update(&mut self, msg: ComposeMsg, ui: &mut Ui<ComposeMsg>) {
        match msg {
            ComposeMsg::Changed(field) => self.changed(ui, field),
            ComposeMsg::Focused(field, true) => {
                self.active = field.is_recipient().then_some(field);
                self.suggest(ui);
                if self.active.is_none() {
                    self.show_suggestions(ui, Vec::new());
                }
            }
            ComposeMsg::Focused(..) | ComposeMsg::FromChanged => {}
            ComposeMsg::Submit(field) => {
                if self.suggester.is_empty() || !field.is_recipient() {
                    self.form.edit(field.next()).focus();
                } else {
                    self.accept_suggestion(ui, 0);
                }
            }
            ComposeMsg::Suggestion(index) => self.accept_suggestion(ui, index),
            ComposeMsg::AcceptFirst => self.accept_suggestion(ui, 0),
            ComposeMsg::AddAttachment => pick_and_read(ui),
            ComposeMsg::RemoveAttachment => self.remove_attachment(ui),
            ComposeMsg::AttachmentsRead(read) => self.attachments_read(ui, read),
            ComposeMsg::Send => self.send(),
            ComposeMsg::SaveDraft => self.save_draft(true),
            ComposeMsg::Discard => self.discard(ui),
            ComposeMsg::CloseRequested => self.close_requested(ui),
            ComposeMsg::Timer => self.timer(),
            ComposeMsg::Sending => {
                self.show_status(ui, "Sending...");
                self.sending = true;
                self.form.set_locked(true);
            }
            ComposeMsg::Failed(error) => {
                self.sending = false;
                self.form.set_locked(false);
                self.show_status(ui, &format!("Send failed: {error}\nThe message is kept, so you can send it again."));
            }
            ComposeMsg::Sent => ui.close(),
            ComposeMsg::DraftSaved => self.show_status(ui, "Draft saved."),
            ComposeMsg::SetTheme(theme, follow) => {
                ui.follow_system_theme(follow);
                ui.set_theme(theme);
            }
        }
    }
}

impl Drop for ComposeApp {
    fn drop(&mut self) {
        self.ask(Request::Closed);
    }
}
