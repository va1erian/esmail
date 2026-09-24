//! The reading pane: an HTML view plus what it needs to repaint itself in a
//! new palette without asking the server for the message again.

use esmail::imap::MailHeader;
use esmail::render::Attachment;
use std::sync::Arc;

use litehtml_view_d2d::{HtmlView, HtmlViewEvent, ImageFetcher};
use win32ui::prelude::*;

use esmail_win32::core_glue::images;
use esmail_win32::core_glue::reading::{self, Appearance, Palette};

use super::Msg;

/// What the pane shows.
enum Content {
    /// A line of text: "Select a message", an error, with the label and href of
    /// the action it offers.
    Notice(String, Option<(String, String)>),
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
            reading::notice("", None, &palette),
            || Msg::Frame,
            |event| match event {
                HtmlViewEvent::LinkClicked(href) => Some(Msg::Link(href)),
            },
        )?;
        let reader = Reader { view, appearance, content: Content::Notice(String::new(), None) };
        reader.render();
        Ok(reader)
    }

    pub fn show_notice(&mut self, text: &str) {
        self.content = Content::Notice(text.to_string(), None);
        self.render();
    }

    /// A notice with a link under it: `action` is the link's label and href.
    pub fn show_notice_with_link(&mut self, text: &str, label: &str, href: &str) {
        self.content = Content::Notice(text.to_string(), Some((label.to_string(), href.to_string())));
        self.render();
    }

    pub fn show_message(&mut self, header: MailHeader, body: String, attachments: Vec<Attachment>) {
        self.content = Content::Message { header, body, attachments };
        self.render();
    }

    /// View > Load remote images: fetch the images of the message on screen (and of
    /// the next ones) from the web, or leave them blank.
    pub fn set_remote_images(&self, allow: bool) {
        self.view.set_image_fetcher(allow.then(|| Arc::new(images::fetch) as ImageFetcher));
        self.render();
    }

    pub fn is_dark(&self) -> bool {
        self.appearance.palette.dark
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
            Content::Notice(text, action) => {
                self.view.set_background(color(palette.background));
                let action = action.as_ref().map(|(label, href)| (label.as_str(), href.as_str()));
                self.view.load(reading::notice(text, action, palette));
            }
            Content::Message { header, body, attachments } => {
                let themed = self.appearance.themes_body(body);
                self.view.set_background(color(self.appearance.page_background(themed)));
                self.view.load(reading::document(header, body, attachments, palette, themed));
            }
        }
    }

    /// The message on screen: its header and sanitised HTML body.
    /// The attachments of the message on screen.
    pub fn current_attachments(&self) -> &[Attachment] {
        match &self.content {
            Content::Message { attachments, .. } => attachments,
            Content::Notice(..) => &[],
        }
    }

    pub fn current_message(&self) -> Option<(&MailHeader, &str)> {
        match &self.content {
            Content::Message { header, body, .. } => Some((header, body)),
            Content::Notice(..) => None,
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
