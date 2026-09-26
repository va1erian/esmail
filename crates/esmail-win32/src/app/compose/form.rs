//! The compose window's widgets and their layout.

use xui_win32::prelude::*;
use xui_win32::{column, row};

use super::attachments::AttachmentList;
use super::{ComposeMsg, Field};

/// Width of the field captions, in dips.
const CAPTION_WIDTH: f32 = 70.0;
/// Height of one field row, in dips.
const ROW_HEIGHT: f32 = 28.0;

/// Which optional lists are open.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct Panels {
    /// The address suggestions.
    pub suggestions: bool,
    /// The attachments.
    pub attachments: bool,
}

/// Every widget of the window.
pub struct Form {
    pub from: ComboBox<usize, ComposeMsg>,
    pub to: Edit<ComposeMsg>,
    pub cc: Edit<ComposeMsg>,
    pub bcc: Edit<ComposeMsg>,
    pub subject: Edit<ComposeMsg>,
    pub body: Edit<ComposeMsg>,
    pub suggestions: ListView<String, ComposeMsg>,
    pub attachments: AttachmentList,
    pub add: Button<ComposeMsg>,
    pub remove: Button<ComposeMsg>,
    pub send: Button<ComposeMsg>,
    pub save: Button<ComposeMsg>,
    pub discard: Button<ComposeMsg>,
    pub status: Label,
    /// The field captions, then the empty label that pushes the buttons right.
    /// Kept alive: a label's window is destroyed with it.
    _captions: Vec<Label>,
}

fn recipient(ui: &mut Ui<ComposeMsg>, field: Field) -> xui_win32::Result<Edit<ComposeMsg>> {
    Ok(Edit::single_line(ui)?
        .on_change(move |_| Some(ComposeMsg::Changed(field)))
        .on_focus(move |focused| Some(ComposeMsg::Focused(field, focused)))
        .on_submit(move || Some(ComposeMsg::Submit(field))))
}

fn button(ui: &mut Ui<ComposeMsg>, text: &str, msg: fn() -> ComposeMsg) -> xui_win32::Result<Button<ComposeMsg>> {
    Ok(Button::new(ui, text)?.on_click(move || Some(msg())))
}

impl Form {
    /// Creates the widgets and lays them out. `accounts` are the labels of the
    /// accounts the message can be sent from.
    pub fn build(ui: &mut Ui<ComposeMsg>, accounts: &[String]) -> xui_win32::Result<Form> {
        let from = ComboBox::new(ui, accounts.iter().cloned().enumerate().map(|(index, label)| (label, index)))?.on_select(|_| Some(ComposeMsg::FromChanged));
        let subject = Edit::single_line(ui)?
            .on_change(|_| Some(ComposeMsg::Changed(Field::Subject)))
            .on_focus(|focused| Some(ComposeMsg::Focused(Field::Subject, focused)))
            .on_submit(|| Some(ComposeMsg::Submit(Field::Subject)));
        let body = Edit::multi_line(ui)?
            .on_change(|_| Some(ComposeMsg::Changed(Field::Body)))
            .on_focus(|focused| Some(ComposeMsg::Focused(Field::Body, focused)));
        let suggestions = ListView::new(ui)?
            .column("Suggestions (Ctrl+Space takes the first)", Fill, |address: &String| address.as_str())
            .on_select(|rows| rows.first().map(|row| ComposeMsg::Suggestion(*row)));
        let form = Form {
            from,
            to: recipient(ui, Field::To)?,
            cc: recipient(ui, Field::Cc)?,
            bcc: recipient(ui, Field::Bcc)?,
            subject,
            body,
            suggestions,
            attachments: AttachmentList::new(ui)?,
            add: button(ui, "Attach file...", || ComposeMsg::AddAttachment)?,
            remove: button(ui, "Remove", || ComposeMsg::RemoveAttachment)?,
            send: button(ui, "Send (Ctrl+Enter)", || ComposeMsg::Send)?,
            save: button(ui, "Save draft", || ComposeMsg::SaveDraft)?,
            discard: button(ui, "Discard", || ComposeMsg::Discard)?,
            status: Label::new(ui, Rect::default(), "")?,
            _captions: ["From", "To", "Cc", "Bcc", "Subject", ""].into_iter().map(|text| Label::new(ui, Rect::default(), text)).collect::<xui_win32::Result<_>>()?,
        };
        form.status.set_visible(false);
        form.arrange(ui, Panels::default());
        Ok(form)
    }

    /// Lays the form out with the optional lists open or closed. A closed list is
    /// zero-high rather than hidden: a native list that starts hidden does not paint
    /// its rows once shown.
    pub fn arrange(&self, ui: &Ui<ComposeMsg>, panels: Panels) {
        let open = |open: bool, height: f32| dip(if open { height } else { 0.0 });
        let caption = |index: usize| self._captions[index].width(dip(CAPTION_WIDTH));
        let field = |index: usize, edit: &Edit<ComposeMsg>| row![caption(index), edit.fill(1)].height(dip(ROW_HEIGHT));
        ui.set_layout(
            column![
                row![caption(0), self.from.fill(1)].height(dip(ROW_HEIGHT)),
                field(1, &self.to),
                field(2, &self.cc),
                field(3, &self.bcc),
                self.suggestions.height(open(panels.suggestions, 136.0)),
                field(4, &self.subject),
                self.body.fill(1),
                self.attachments.list.height(open(panels.attachments, 84.0)),
                self.status.height(dip(52.0)),
                row![self.add.width(dip(120.0)), self.remove.width(dip(90.0)), self._captions[5].fill(1), self.save.width(dip(110.0)), self.discard.width(dip(90.0)), self.send.width(dip(150.0))]
                    .spacing(dip(8.0))
                    .height(dip(32.0)),
            ]
            .spacing(dip(6.0))
            .margins(Insets::new(dip(12.0), dip(12.0) + ui.title_bar_height(), dip(12.0), dip(12.0))),
        );
    }

    pub fn edit(&self, field: Field) -> &Edit<ComposeMsg> {
        match field {
            Field::To => &self.to,
            Field::Cc => &self.cc,
            Field::Bcc => &self.bcc,
            Field::Subject => &self.subject,
            Field::Body => &self.body,
        }
    }

    /// Locks the form while a send is in flight, unlocks it after.
    pub fn set_locked(&self, locked: bool) {
        for edit in [&self.to, &self.cc, &self.bcc, &self.subject, &self.body] {
            edit.set_read_only(locked);
        }
        self.from.set_enabled(!locked);
        for button in [&self.add, &self.remove, &self.send, &self.save] {
            button.set_enabled(!locked);
        }
    }
}
