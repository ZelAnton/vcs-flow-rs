//! Plain-stdin yes/no prompts, used between TUI sessions (normal terminal
//! mode): the push offers, the multi-commit "commit more?" question, etc.
//! EOF (closed stdin) always declines — never act without an explicit answer.

use std::io::{self, Write};

/// `[Y/n]` prompt, default **yes** (Enter agrees).
pub fn confirm(question: &str) -> crate::AppResult<bool> {
    let Some(answer) = ask(question, "[Y/n]")? else {
        return Ok(false); // EOF → decline
    };
    Ok(answer.is_empty() || answer.eq_ignore_ascii_case("y"))
}

/// `[y/N]` prompt, default **no** (Enter declines) — for the destructive or
/// out-of-the-ordinary offers (force push, another commit round).
pub fn confirm_no(question: &str) -> crate::AppResult<bool> {
    let Some(answer) = ask(question, "[y/N]")? else {
        return Ok(false); // EOF → decline
    };
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

/// Print `question hint `, read one line, and return it trimmed (lowercase
/// answers are the callers' concern). `None` on EOF.
fn ask(question: &str, hint: &str) -> crate::AppResult<Option<String>> {
    print!("{question} {hint} ");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        println!();
        return Ok(None);
    }
    Ok(Some(line.trim().to_string()))
}
