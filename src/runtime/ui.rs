use std::io;

use anyhow::Result;
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::disable_raw_mode;

pub(super) struct BracketedPasteGuard;

impl BracketedPasteGuard {
    pub(super) fn enter() -> Result<Self> {
        let mut stdout = io::stdout();
        execute!(stdout, EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for BracketedPasteGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableBracketedPaste);
    }
}

pub(super) struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}
