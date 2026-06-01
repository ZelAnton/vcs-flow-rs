//! Terminal setup/teardown with guaranteed restore.
//!
//! Enters the alternate screen + raw mode and best-effort enables keyboard
//! enhancement flags so `Ctrl+Enter` is reported distinctly (where the terminal
//! supports it; `Ctrl+S` is the universal fallback). A panic hook and the RAII
//! `Drop` both restore the terminal so a crash never leaves it wedged.

use std::io::{self, Stdout};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

pub type Tui = Terminal<CrosstermBackend<Stdout>>;

/// Restores terminal state when dropped.
pub struct TerminalGuard;

impl TerminalGuard {
    /// Enter the TUI. Returns the ratatui terminal plus a guard; keep the guard
    /// alive for the whole interactive session and drop it before printing.
    pub fn enter() -> io::Result<(Tui, Self)> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen)?;
        // Ignored by terminals that don't support it; restored on teardown.
        let _ = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        install_panic_hook();
        let terminal = Terminal::new(CrosstermBackend::new(out))?;
        Ok((terminal, TerminalGuard))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Undo everything `enter` did. Safe to call more than once.
fn restore() {
    let mut out = io::stdout();
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    let _ = execute!(out, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// Restore the terminal before the default panic handler prints, so the message
/// isn't swallowed by the alternate screen / mangled by raw mode.
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        original(info);
    }));
}
