//! Setup's questions: answers, secrets that are not shown, yes or no, and
//! choices from lists. [`Terminal`] asks them with inquire at the
//! controlling terminal, which it reads even when standard input is a pipe.
//! Interrupting (Ctrl+C) or cancelling (Esc, Ctrl+D) a question, or having
//! no terminal at all, ends setup with an error once inquire has restored
//! the terminal; nothing is ever assumed for a question not answered.

use anyhow::{Result, anyhow};
use inquire::{
    Confirm, InquireError, MultiSelect, Password, PasswordDisplayMode, Select, Text,
    list_option::ListOption,
};

/// The questions setup asks, so its decisions can be exercised without a
/// terminal.
pub trait Questions {
    /// Shows a line of text.
    ///
    /// # Errors
    ///
    /// Returns an error when it cannot be shown.
    fn say(&mut self, text: &str) -> Result<()>;

    /// The answer to `question`, trimmed; `default` for an empty one.
    ///
    /// # Errors
    ///
    /// Returns an error when the question is cancelled or cannot be asked.
    fn answer(&mut self, question: &str, default: Option<&str>) -> Result<String>;

    /// The answer to `question`, trimmed, never shown.
    ///
    /// # Errors
    ///
    /// Returns an error when the question is cancelled or cannot be asked.
    fn secret(&mut self, question: &str) -> Result<String>;

    /// Yes or no to `question`, `default` for an empty answer.
    ///
    /// # Errors
    ///
    /// Returns an error when the question is cancelled or cannot be asked.
    fn confirm(&mut self, question: &str, default: bool) -> Result<bool>;

    /// The index of the chosen one of `options`, starting at `default`.
    ///
    /// # Errors
    ///
    /// Returns an error when the question is cancelled or cannot be asked.
    fn choose(&mut self, question: &str, options: &[String], default: usize) -> Result<usize>;

    /// The indexes of the chosen `options`, starting with `chosen`; possibly
    /// none.
    ///
    /// # Errors
    ///
    /// Returns an error when the question is cancelled or cannot be asked.
    fn choose_many(
        &mut self,
        question: &str,
        options: &[String],
        chosen: &[usize],
    ) -> Result<Vec<usize>>;
}

/// Questions at the controlling terminal.
pub struct Terminal;

impl Questions for Terminal {
    fn say(&mut self, text: &str) -> Result<()> {
        println!("{text}");
        Ok(())
    }

    fn answer(&mut self, question: &str, default: Option<&str>) -> Result<String> {
        let mut text = Text::new(question);
        if let Some(default) = default {
            text = text.with_default(default);
        }
        Ok(text.prompt().map_err(ended)?.trim().to_owned())
    }

    fn secret(&mut self, question: &str) -> Result<String> {
        Ok(Password::new(question)
            .with_display_mode(PasswordDisplayMode::Hidden)
            .without_confirmation()
            .prompt()
            .map_err(ended)?
            .trim()
            .to_owned())
    }

    fn confirm(&mut self, question: &str, default: bool) -> Result<bool> {
        Confirm::new(question)
            .with_default(default)
            .prompt()
            .map_err(ended)
    }

    fn choose(&mut self, question: &str, options: &[String], default: usize) -> Result<usize> {
        Ok(Select::new(question, options.to_vec())
            .with_starting_cursor(default)
            .raw_prompt()
            .map_err(ended)?
            .index)
    }

    fn choose_many(
        &mut self,
        question: &str,
        options: &[String],
        chosen: &[usize],
    ) -> Result<Vec<usize>> {
        Ok(MultiSelect::new(question, options.to_vec())
            .with_default(chosen)
            .raw_prompt()
            .map_err(ended)?
            .into_iter()
            .map(|ListOption { index, .. }| index)
            .collect())
    }
}

/// Why a question went unanswered, in the operator's terms.
fn ended(error: InquireError) -> anyhow::Error {
    match error {
        InquireError::NotTTY => anyhow!(
            "setup asks its questions at a terminal, and there is none; run it from a terminal, \
             or install with the unattended setup options"
        ),
        InquireError::OperationCanceled | InquireError::OperationInterrupted => {
            anyhow!("setup was cancelled")
        }
        other => anyhow!(other).context("ask a setup question"),
    }
}
