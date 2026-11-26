use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// An image the user attached to a message (kb d191): a file pasted or
/// dropped into the input box, or the clipboard's picture as a data URL.
/// The engine prepares it, Codex's resize included, when the message is
/// sent; a saved chat's image comes back for `/rewind` as the data URL it
/// was sent as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageInput {
    File { path: PathBuf },
    DataUrl { url: String },
}
