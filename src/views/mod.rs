pub mod csv;
pub mod hex;
pub mod json;
pub mod markdown;
pub mod scrollbar;
pub mod text;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Text,
    Hex,
    Tree,
    Markdown,
    Table,
}

/// Emitted by the structured views: show this byte offset in the text view.
#[derive(Clone, Copy, Debug)]
pub struct JumpTo(pub u64);
