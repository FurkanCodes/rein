//! How rein looks in a terminal: each step under a label, answers rendered
//! from Markdown and wrapped, and under it all, the input box and its
//! status bar (see `crate::input`).

pub mod editor;
pub mod history;
pub mod keys;
pub mod markdown;
pub mod screen;
pub mod spinner;
pub mod steps;
pub mod style;
pub mod transcript;
pub mod viewer;
pub mod wrap;
