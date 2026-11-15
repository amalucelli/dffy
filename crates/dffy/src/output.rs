//! Where output goes: the terminal facts that shape it (TTY, width, color)
//! and the sink itself, stdout or a pager process, behind one buffered
//! writer.

use std::io::{self, BufWriter, IsTerminal, Write};
use std::os::fd::AsFd;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::ColorWhen;
use crate::render::Canvas;
use crate::theme::Theme;

/// Facts about the terminal, read once at startup and before a pager takes
/// over stdout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Term {
    pub stdout_tty: bool,
    pub stderr_tty: bool,
    /// Columns of the terminal behind stdout, or behind stderr when stdout is
    /// redirected (so `dffy --split | less -R` still fits the terminal), or
    /// `$COLUMNS`.
    pub width: Option<usize>,
}

impl Term {
    pub(crate) fn detect() -> Self {
        let stdout_tty = io::stdout().is_terminal();
        let stderr_tty = io::stderr().is_terminal();
        let width = tty_width(io::stdout())
            .or_else(|| tty_width(io::stderr()))
            .or_else(|| {
                std::env::var("COLUMNS")
                    .ok()?
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
            });
        Self {
            stdout_tty,
            stderr_tty,
            width,
        }
    }
}

fn tty_width(fd: impl AsFd) -> Option<usize> {
    let size = rustix::termios::tcgetwinsize(fd).ok()?;
    (size.ws_col > 0).then_some(usize::from(size.ws_col))
}

/// Whether to color a stream, given the `--color`/`color` setting.
pub(crate) fn use_color(when: ColorWhen, tty: bool) -> bool {
    let set = |name| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    match when {
        ColorWhen::Always => true,
        ColorWhen::Never => false,
        ColorWhen::Auto if set("NO_COLOR") => false,
        ColorWhen::Auto
            if set("CLICOLOR_FORCE") && std::env::var_os("CLICOLOR_FORCE") != Some("0".into()) =>
        {
            true
        }
        ColorWhen::Auto => tty,
    }
}

/// The pager command: `$DFFY_PAGER`, the `pager` key, `$PAGER`, then
/// `less -RFX`.
/// `None` when the chosen pager is empty or `cat`.
pub(crate) fn pager_command(configured: &str) -> Option<String> {
    let env = |name| std::env::var(name).ok();
    let command = env("DFFY_PAGER")
        .or_else(|| (!configured.is_empty()).then(|| configured.to_owned()))
        .or_else(|| env("PAGER"))
        .unwrap_or_else(|| "less -RFX".to_owned());
    let trimmed = command.trim();
    (!trimmed.is_empty() && trimmed != "cat").then(|| trimmed.to_owned())
}

/// Buffered stdout or pager stdin. `BrokenPipe` errors from writes mean the
/// reader went away (`| head`, quitting the pager); callers stop and exit 0.
/// `Interrupted` errors mean Ctrl-C while paging.
pub(crate) struct Output {
    writer: BufWriter<Box<dyn Write>>,
    pager: Option<Child>,
    /// Set by SIGINT while a pager runs.
    interrupted: Option<Arc<AtomicBool>>,
}

impl Output {
    pub(crate) fn stdout() -> Self {
        Self {
            writer: BufWriter::with_capacity(64 << 10, Box::new(io::stdout().lock())),
            pager: None,
            interrupted: None,
        }
    }

    /// Starts `command` through `sh -c`, as git does, and writes to it. Falls
    /// back to stdout when the pager can't start.
    pub(crate) fn paged(command: &str) -> Self {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command).stdin(Stdio::piped());
        // git's defaults, so a bare `less` shows colors and quits when the
        // output fits one screen.
        if std::env::var_os("LESS").is_none() {
            cmd.env("LESS", "FRX");
        }
        if std::env::var_os("LV").is_none() {
            cmd.env("LV", "-c");
        }
        match cmd.spawn() {
            Ok(mut child) => match child.stdin.take() {
                Some(stdin) => {
                    // Ctrl-C reaches the pager and dffy alike. Dying here
                    // would leave the pager drawing over the shell's prompt;
                    // instead dffy stops writing and waits for the pager to
                    // exit, as git does.
                    let interrupted = Arc::new(AtomicBool::new(false));
                    let registered = signal_hook::flag::register(
                        signal_hook::consts::SIGINT,
                        Arc::clone(&interrupted),
                    );
                    Self {
                        writer: BufWriter::with_capacity(64 << 10, Box::new(stdin)),
                        pager: Some(child),
                        interrupted: registered.is_ok().then_some(interrupted),
                    }
                }
                None => Self::stdout(),
            },
            Err(_) => Self::stdout(),
        }
    }

    fn check_interrupt(&self) -> io::Result<()> {
        if self
            .interrupted
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
        }
        Ok(())
    }

    pub(crate) fn write(&mut self, canvas: &Canvas, theme: Option<&Theme>) -> io::Result<()> {
        self.check_interrupt()?;
        canvas.write_to(&mut self.writer, theme)
    }

    /// Writes bytes as they are, for input passed through unchanged.
    pub(crate) fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.check_interrupt()?;
        self.writer.write_all(bytes)
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        self.check_interrupt()?;
        self.writer.flush()
    }

    /// Flushes, closes the pager's input and waits for it to exit.
    pub(crate) fn finish(self) -> io::Result<()> {
        let interrupted = self.check_interrupt();
        let Self {
            mut writer, pager, ..
        } = self;
        let flushed = interrupted.and_then(|()| writer.flush());
        // Dropping the writer closes the pipe so the pager sees EOF; writing
        // nothing more after a failed flush is fine.
        drop(writer.into_parts());
        if let Some(mut pager) = pager {
            pager.wait()?;
        }
        flushed
    }
}
