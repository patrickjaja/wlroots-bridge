//! Input synthesis over the wlroots virtual-input protocols.
//!
//! Split into `pointer` (virtual pointer: motion, buttons, scroll, drag,
//! held-button holder process), `keyboard` (virtual keyboard: key sequences,
//! typing, hold), and `keymap` (CU key-spec parsing + hand-rolled XKB keymap
//! text generation, so we never link libxkbcommon).

pub mod keyboard;
pub mod keymap;
pub mod pointer;
