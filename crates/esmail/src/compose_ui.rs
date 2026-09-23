//! Drawing a compose window: the form, the action bar and the recipient
//! autocomplete. Split out of `compose_window.rs` (which keeps the state and
//! OS-window lifecycle) to keep both modules under the 500-line bar; the
//! split changes no behavior.

use super::*;
use crate::compose_window::{discard_clicked, Focus, Shared};
use esmail::contacts::{complete_recipient, recipient_token, Contact, Contacts};

/// The recipient fields autocomplete applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecipientField {
    To,
    Cc,
    Bcc,
}

impl RecipientField {
    fn label(self) -> &'static str {
        match self {
            Self::To => "To:",
            Self::Cc => "Cc:",
            Self::Bcc => "Bcc:",
        }
    }

    /// A stable widget id, so the caret can be moved after a completion
    /// replaces the field's text.
    fn id(self) -> egui::Id {
        egui::Id::new(("compose_recipient", self.label()))
    }
}

/// How many recipient suggestions are offered at once.
const MAX_SUGGESTIONS: usize = 6;

/// Autocomplete state for the recipient field that currently has focus.
#[derive(Default)]
pub(super) struct SuggestState {
    /// Which field this state belongs to; switching fields resets it.
    field: Option<RecipientField>,
    /// The query the highlighted row belongs to; a change resets the row and
    /// clears a previous Escape.
    query: String,
    /// Highlighted suggestion.
    selected: usize,
    /// Escape hid the popup for the current query.
    dismissed: bool,
}

pub(super) fn draw(ui: &mut egui::Ui, s: &mut Shared) {
    let ctx = ui.ctx().clone();

    // Something would be lost by closing right now: unsaved typing, or a
    // send that's still in flight (there's no cancelling it once issued --
    // see `smtp.rs` -- so walking away from it silently would either lose
    // the message from view while it still gets sent, or, if it fails,
    // leave the error with nobody to show it to). An unedited Reply/Forward
    // sent as-is (`state == initial`) is exactly the case `state` alone
    // would miss.
    let unsent = s.state != s.initial || s.sending;

    // The OS window's own close button (or Alt+F4). Unsent text or an
    // in-flight send is asked about first; otherwise the app is told to
    // drop the window.
    if ctx.input(|i| i.viewport().close_requested()) {
        if unsent && !s.finished {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            s.confirm_discard = true;
        } else {
            s.finished = true;
        }
    }

    // Ctrl+Enter sends. Consumed up front so the text field does not also
    // insert a line break.
    let ctrl_enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter));
    let mut send = ctrl_enter && !s.sending && !s.confirm_discard;

    egui::Panel::bottom("compose_actions").show(ui, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if s.confirm_discard {
                if s.sending {
                    ui.label("A send is still in progress and can't be stopped -- discard this window anyway?");
                } else {
                    ui.label("Discard this unsent message?");
                }
                if ui.button("Discard").clicked() {
                    s.finished = true;
                }
                if ui.button("Keep editing").clicked() {
                    s.confirm_discard = false;
                }
            } else {
                if ui.add_enabled(!s.sending, egui::Button::new("Send")).on_hover_text("Ctrl+Enter").clicked() {
                    send = true;
                }
                if ui.button("Discard").clicked() {
                    discard_clicked(s);
                }
                if s.sending {
                    ui.spinner();
                    ui.weak("Sending…");
                } else if let Some(error) = &s.error {
                    ui.label(egui::RichText::new(error).color(egui::Color32::RED));
                }
            }
        });
        ui.add_space(4.0);
    });

    egui::CentralPanel::default().show(ui, |ui| {
        let Shared { state, accounts, error, sending, focus, contacts, suggest, .. } = &mut *s;
        ui.add_enabled_ui(!*sending, |ui| {
            egui::Grid::new("compose_grid").num_columns(2).show(ui, |ui| {
                // Which account this is sent from -- and whose Sent folder
                // gets the copy. Chosen when the window opened (the account
                // of the message being replied to, else the active one).
                ui.label("From:");
                let selected = state
                    .account_id
                    .as_deref()
                    .and_then(|id| accounts.iter().find(|(a, _)| a == id))
                    .map_or("(choose an account)", |(_, label)| label.as_str());
                egui::ComboBox::from_id_salt("compose_from").selected_text(selected).show_ui(ui, |ui| {
                    for (id, label) in accounts.iter() {
                        ui.selectable_value(&mut state.account_id, Some(id.clone()), label);
                    }
                });
                ui.end_row();

                recipient_field(ui, RecipientField::To, &mut state.to, contacts, suggest, matches!(focus, Some(Focus::To)));
                if matches!(focus, Some(Focus::To)) {
                    *focus = None;
                }
                recipient_field(ui, RecipientField::Cc, &mut state.cc, contacts, suggest, false);
                recipient_field(ui, RecipientField::Bcc, &mut state.bcc, contacts, suggest, false);

                ui.label("Subject:");
                ui.add(egui::TextEdit::singleline(&mut state.subject).desired_width(f32::INFINITY));
                ui.end_row();
            });

            ui.separator();

            let mut remove = None;
            ui.horizontal_wrapped(|ui| {
                if ui.button("Attach file…").clicked() {
                    // Not parented to this window: egui hands a viewport's
                    // callback no native window handle to parent it to.
                    if let Some(path) = rfd::FileDialog::new().pick_file() {
                        match std::fs::read(&path) {
                            Ok(data) => {
                                let filename = path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "attachment".to_string());
                                state.attachments.push((filename, data));
                            }
                            Err(e) => {
                                *error = Some(format!("Could not read {}: {e}", path.display()));
                            }
                        }
                    }
                }
                for (i, (filename, data)) in state.attachments.iter().enumerate() {
                    ui.label(format!("{filename} ({})", format_size(data.len())));
                    if ui.small_button("✕").on_hover_text("Remove").clicked() {
                        remove = Some(i);
                    }
                }
            });
            if let Some(i) = remove {
                state.attachments.remove(i);
            }

            // The body takes whatever room is left and scrolls past it, so it
            // follows the window as it is resized.
            let room = ui.available_size();
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                let body = ui.add(
                    egui::TextEdit::multiline(&mut state.body)
                        .desired_width(f32::INFINITY)
                        .min_size(room),
                );
                if matches!(focus, Some(Focus::Body)) {
                    body.request_focus();
                    *focus = None;
                }
            });
        });
    });

    if send {
        s.send_requested = true;
        s.error = None;
    }
}

/// Draws one recipient field and, while it has focus, its autocomplete popup.
fn recipient_field(
    ui: &mut egui::Ui,
    field: RecipientField,
    text: &mut String,
    contacts: &Arc<Contacts>,
    suggest: &mut SuggestState,
    focus: bool,
) {
    ui.label(field.label());
    let id = field.id();
    // While a popup is up, Tab and the arrows belong to it rather than to
    // egui's focus navigation (see `TextEdit::event_filter`); with no popup
    // they keep their usual meaning, so Tab still moves to the next field.
    // The state read here is the previous frame's, which is what the filter
    // can act on -- it only takes effect once the widget already had focus.
    let capture = suggest.field == Some(field) && !suggest.dismissed && !suggest.query.is_empty();
    let output = egui::TextEdit::singleline(text)
        .id(id)
        .desired_width(f32::INFINITY)
        .event_filter(egui::EventFilter { tab: capture, vertical_arrows: capture, escape: capture, ..Default::default() })
        .show(ui);
    let response = &output.response.response;
    if focus {
        response.request_focus();
    }
    autocomplete(ui, field, id, response, output.cursor_range.map(|r| r.primary.index.0), text, contacts, suggest, capture);
    ui.end_row();
}

/// Shows recipient suggestions for the token under the caret and applies the
/// one the user picks (Tab, or a click), if any.
#[allow(clippy::too_many_arguments)]
fn autocomplete(
    ui: &egui::Ui,
    field: RecipientField,
    id: egui::Id,
    response: &egui::Response,
    cursor: Option<usize>,
    text: &mut String,
    contacts: &Arc<Contacts>,
    suggest: &mut SuggestState,
    capture_keys: bool,
) {
    // `lost_focus` too, not just `has_focus`: clicking a suggestion presses
    // outside the field, which surrenders its focus before this runs, yet the
    // click still has to reach the popup drawn below.
    if !response.has_focus() && !response.lost_focus() {
        if suggest.field == Some(field) {
            *suggest = SuggestState::default();
        }
        return;
    }
    if suggest.field != Some(field) {
        *suggest = SuggestState { field: Some(field), ..SuggestState::default() };
    }

    let cursor = cursor.unwrap_or_else(|| text.chars().count());
    let query = recipient_token(text, cursor);
    if query != suggest.query {
        suggest.query = query;
        suggest.selected = 0;
        suggest.dismissed = false;
    }
    let matches = contacts.matching(&suggest.query, text, MAX_SUGGESTIONS);
    if matches.is_empty() || suggest.dismissed {
        return;
    }
    suggest.selected = suggest.selected.min(matches.len() - 1);

    let mut chosen: Option<Contact> = None;
    if capture_keys {
        ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowDown) {
                suggest.selected = (suggest.selected + 1) % matches.len();
            }
            if i.key_pressed(egui::Key::ArrowUp) {
                suggest.selected = (suggest.selected + matches.len() - 1) % matches.len();
            }
            if i.key_pressed(egui::Key::Escape) {
                suggest.dismissed = true;
            }
            if i.key_pressed(egui::Key::Tab) {
                chosen = Some(matches[suggest.selected].clone());
            }
        });
    }
    if suggest.dismissed {
        return;
    }

    egui::Popup::from_response(response).open(true).gap(2.0).width(response.rect.width()).show(|ui| {
        for (i, contact) in matches.iter().enumerate() {
            if ui.selectable_label(i == suggest.selected, &contact.display).clicked() {
                chosen = Some((*contact).clone());
            }
        }
    });

    if let Some(contact) = chosen {
        let (completed, caret) = complete_recipient(text, cursor, &contact.display);
        *text = completed;
        set_caret(ui.ctx(), id, caret);
        // A click on a suggestion took focus away from the field; give it
        // back so the user can carry on typing the next recipient.
        response.request_focus();
        *suggest = SuggestState { field: Some(field), ..SuggestState::default() };
    }
}

/// Moves a recipient field's caret to `char_index` after a completion
/// replaced its text, so the next keystroke continues after the `", "`.
fn set_caret(ctx: &egui::Context, id: egui::Id, char_index: usize) {
    if let Some(mut state) = egui::widgets::text_edit::TextEditState::load(ctx, id) {
        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(char_index))));
        state.store(ctx, id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contacts() -> Arc<Contacts> {
        let header = esmail::imap::MailHeader {
            uid: 1,
            subject: String::new(),
            from: "Alice <alice@example.com>".to_string(),
            to: String::new(),
            date: String::new(),
            message_id: String::new(),
            flags: Vec::new(),
        };
        Arc::new(Contacts::from_headers([&header], []))
    }

    /// Runs one egui pass over a single To field.
    fn pass(
        ctx: &egui::Context,
        contacts: &Arc<Contacts>,
        to: &mut String,
        suggest: &mut SuggestState,
        events: Vec<egui::Event>,
        focus: bool,
    ) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            egui::Grid::new("compose_grid").num_columns(2).show(ui, |ui| {
                recipient_field(ui, RecipientField::To, to, contacts, suggest, focus);
            });
        });
        // No renderer here to apply the font/texture uploads.
        output.textures_delta.clear();
    }

    fn tab() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    /// Drives a focused recipient field with a matching query through a real
    /// egui pass, so the popup path is exercised -- the pure matching and
    /// text-editing logic is tested in `esmail::contacts`.
    #[test]
    fn a_focused_recipient_field_with_matches_draws_without_panicking() {
        let contacts = contacts();
        let ctx = egui::Context::default();
        let mut to = "al".to_string();
        let mut suggest = SuggestState::default();
        pass(&ctx, &contacts, &mut to, &mut suggest, Vec::new(), true);
        pass(&ctx, &contacts, &mut to, &mut suggest, Vec::new(), false);
        assert_eq!(suggest.field, Some(RecipientField::To));
    }

    /// The whole interaction: focus the To field, type a query, press Tab,
    /// and check the completed address landed in the field.
    #[test]
    fn tab_accepts_the_highlighted_suggestion() {
        let contacts = contacts();
        let ctx = egui::Context::default();
        let mut to = String::new();
        let mut suggest = SuggestState::default();
        pass(&ctx, &contacts, &mut to, &mut suggest, Vec::new(), true);
        pass(&ctx, &contacts, &mut to, &mut suggest, vec![egui::Event::Text("al".to_string())], false);
        pass(&ctx, &contacts, &mut to, &mut suggest, vec![tab()], false);
        assert_eq!(to, "Alice <alice@example.com>, ");
    }
}
