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

impl LineReader {
    /// Read an editable line. `None` represents Ctrl-D; Ctrl-C returns an
    /// empty answer so callers retain their existing default/cancel behavior.
    pub fn read_line(&mut self, label: &str) -> Result<Option<String>> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            print!("{label}");
            io::stdout().flush()?;
            let mut answer = String::new();
            let read = io::stdin()
                .read_line(&mut answer)
                .context("read terminal response")?;
            return Ok((read > 0).then(|| answer.trim().to_owned()));
        }

        let prompt = DefaultPrompt::new(
            DefaultPromptSegment::Basic(label.to_owned()),
            DefaultPromptSegment::Empty,
        );
        match self
            .editor
            .read_line(&prompt)
            .context("read terminal response")?
        {
            Signal::Success(answer) => Ok(Some(answer.trim().to_owned())),
            Signal::CtrlD => Ok(None),
            Signal::CtrlC => Ok(Some(String::new())),
            Signal::ExternalBreak(answer) | Signal::HostCommand(answer) => {
                Ok(Some(answer.trim().to_owned()))
            }
            _ => Ok(None),
        }
    }
}
