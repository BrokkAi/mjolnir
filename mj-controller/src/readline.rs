//! Readline-backed input for standalone terminal prompts.

use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result};
use reedline::{DefaultPrompt, DefaultPromptSegment, Reedline, Signal};

pub struct LineReader {
    editor: Reedline,
}

impl Default for LineReader {
    fn default() -> Self {
        Self {
            editor: Reedline::create(),
        }
    }
}

/// What one prompt returned.
pub enum Line {
    Text(String),
    /// End of input (Ctrl-D, or a closed stdin).
    Eof,
    /// Ctrl-C at the prompt.
    Interrupted,
}

impl LineReader {
    /// Read an editable line. `None` represents Ctrl-D; Ctrl-C returns an
    /// empty answer so callers retain their existing default/cancel behavior.
    pub fn read_line(&mut self, label: &str) -> Result<Option<String>> {
        Ok(match self.read_line_or_interrupt(label)? {
            Line::Text(answer) => Some(answer),
            Line::Eof => None,
            Line::Interrupted => Some(String::new()),
        })
    }

    /// As [`Self::read_line`], but Ctrl-C is not an empty answer: a prompt
    /// whose empty answer picks a default needs to tell the two apart.
    pub fn read_line_or_interrupt(&mut self, label: &str) -> Result<Line> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            print!("{label}");
            io::stdout().flush()?;
            let mut answer = String::new();
            let read = io::stdin()
                .read_line(&mut answer)
                .context("read terminal response")?;
            return Ok(if read > 0 {
                Line::Text(answer.trim().to_owned())
            } else {
                Line::Eof
            });
        }

        let prompt = DefaultPrompt::new(
            DefaultPromptSegment::Basic(label.to_owned()),
            DefaultPromptSegment::Empty,
        );
        Ok(
            match self
                .editor
                .read_line(&prompt)
                .context("read terminal response")?
            {
                Signal::Success(answer)
                | Signal::ExternalBreak(answer)
                | Signal::HostCommand(answer) => Line::Text(answer.trim().to_owned()),
                Signal::CtrlC => Line::Interrupted,
                _ => Line::Eof,
            },
        )
    }
}
