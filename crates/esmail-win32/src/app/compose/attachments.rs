//! The compose window's attachment list, and picking files to attach.

use std::path::PathBuf;

use esmail::view_model::format_size;
use esmail_win32::core_glue::files;
use xui_win32::prelude::*;

use super::ComposeMsg;

/// A file read for attachment: its name and bytes, or why it could not be read.
pub type FileRead = std::result::Result<(String, Vec<u8>), String>;

/// One row of the list.
pub struct Row {
    name: String,
    size: String,
}

/// The attachments under the message, shown only while there are some.
pub struct AttachmentList {
    pub list: ListView<Row, ComposeMsg>,
}

impl AttachmentList {
    pub fn new(ui: &mut Ui<ComposeMsg>) -> xui_win32::Result<AttachmentList> {
        let list = ListView::new(ui)?
            .column("Attachment", Fill, |row: &Row| row.name.as_str())
            .column_right("Size", dip(90.0), |row: &Row| row.size.as_str())
            .zebra(true)
            .on_key(|key, _| (key == Key::DELETE).then_some(ComposeMsg::RemoveAttachment));
        Ok(AttachmentList { list })
    }

    /// Shows `attachments`, keeping the selection where it was when it still
    /// exists.
    pub fn show(&self, attachments: &[(String, Vec<u8>)]) {
        let rows: Vec<Row> = attachments.iter().map(|(name, data)| Row { name: name.clone(), size: format_size(data.len()) }).collect();
        self.list.set_model(rows);
    }

    /// The selected row, if any.
    pub fn selected(&self) -> Option<usize> {
        self.list.selected()
    }
}

/// Asks the user for files and reads them on a thread of their own, so a large
/// file never stalls the window. The result comes back as
/// [`ComposeMsg::AttachmentsRead`].
pub fn pick_and_read(ui: &Ui<ComposeMsg>) {
    let Some(paths) = rfd::FileDialog::new().set_title("Attach files").pick_files() else { return };
    read_in_background(ui, paths);
}

fn read_in_background(ui: &Ui<ComposeMsg>, paths: Vec<PathBuf>) {
    let proxy = ui.proxy();
    std::thread::spawn(move || {
        let files: Vec<_> = paths.iter().map(|path| files::read_attachment(path)).collect();
        let _ = proxy.send(ComposeMsg::AttachmentsRead(files));
    });
}
