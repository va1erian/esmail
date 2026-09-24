//! The reading pane: an HTML view plus what it needs to repaint itself in a
//! new palette without asking the server for the message again.

use esmail::imap::MailHeader;
use esmail::render::Attachment;
use litehtml_view_d2d::{HtmlView, HtmlViewEvent};
use win32ui::prelude::*;

use esmail_win32::core_glue::reading::{self, Appearance, Palette};

use super::Msg;

/// What the pane shows.
enum Content {
    /// A line of text: "Select a message", an error.
    Notice(String),
    /// A fetched message, kept so a theme change can re-render it.
    Message { header: MailHeader, body: String, attachments: Vec<Attachment> },
}

pub struct Reader {
    view: HtmlView<Msg>,
    appearance: Appearance,
    content: Content,
}

impl Reader {
    pub fn new(ui: &mut Ui<Msg>, palette: Palette) -> win32ui::Result<Reader> {
        let appearance = Appearance { palette, original_colours: false };
        let view = HtmlView::new(
            ui,
            reading::notice("", &palette),
            || Msg::Frame,
            |event| match event {
                HtmlViewEvent::LinkClicked(href) => Some(Msg::Link(href)),
            },
        )?;
        let reader = Reader { view, appearance, content: Content::Notice(String::new()) };
        reader.render();
        Ok(reader)
    }

    pub fn show_notice(&mut self, text: &str) {
        self.content = Content::Notice(text.to_string());
        self.render();
    }

    pub fn show_message(&mut self, header: MailHeader, body: String, attachments: Vec<Attachment>) {
        self.content = Content::Message { header, body, attachments };
        self.render();
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.appearance.palette = palette;
        self.render();
    }

    /// View > Original colours: show messages as their authors wrote them.
    pub fn set_original_colours(&mut self, original: bool) {
        self.appearance.original_colours = original;
        self.render();
    }

    fn render(&self) {
        let palette = &self.appearance.palette;
        match &self.content {
            Content::Notice(text) => {
                self.view.set_background(color(palette.background));
                self.view.load(reading::notice(text, palette));
            }
            Content::Message { header, body, attachments } => {
                let themed = self.appearance.themes_body(body);
                self.view.set_background(color(self.appearance.page_background(themed)));
                self.view.load(reading::document(header, body, attachments, palette, themed));
            }
        }
    }

    pub fn is_ready(&self) -> bool {
        self.view.is_ready()
    }

    pub fn invalidate(&self) {
        self.view.invalidate();
    }

}

impl AsControl for Reader {
    fn control(&self) -> &Control {
        self.view.control()
    }
}

fn color(rgb: u32) -> Color {
    Color::hex(rgb)
}
