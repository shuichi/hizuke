//! Terminal-adaptive presentation. Machine output never contains terminal escapes.
use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use hizuke::control;
use hizuke::planner::Plan;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Color {
    Auto,
    Always,
    Never,
}

#[derive(Args, Clone, Debug)]
pub struct Output {
    /// Emit JSON; suppress progress and color
    #[arg(long, global = true)]
    pub json: bool,
    /// Suppress routine output (questions and errors remain visible)
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,
    /// Also show unchanged files and full diagnostic details
    #[arg(short, long, global = true)]
    pub verbose: bool,
    /// Color policy; auto respects NO_COLOR and terminal detection
    #[arg(long, value_enum, global = true, default_value = "auto")]
    pub color: Color,
}

pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

impl Output {
    pub fn paint(&self, text: &str, code: &str, stderr: bool) -> String {
        let terminal = if stderr {
            io::stderr().is_terminal()
        } else {
            io::stdout().is_terminal()
        };
        let colored = !self.json
            && match self.color {
                Color::Always => true,
                Color::Never => false,
                Color::Auto => {
                    terminal
                        && std::env::var_os("NO_COLOR").is_none()
                        && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
                }
            };
        if colored {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    pub fn progress(&self, label: &str) -> Progress {
        Progress::new(
            label,
            !self.quiet
                && !self.json
                && io::stderr().is_terminal()
                && io::stdout().is_terminal()
                && std::env::var_os("TERM").is_none_or(|term| term != "dumb"),
        )
    }

    pub fn plan(&self, plan: &Plan, stderr: bool) -> Result<()> {
        let mut out: Box<dyn Write> = if stderr {
            Box::new(io::stderr().lock())
        } else {
            Box::new(io::stdout().lock())
        };
        if self.quiet {
            return Ok(());
        }
        writeln!(
            out,
            "{} {:?}",
            self.paint("Directory:", "1", stderr),
            plan.root
        )?;
        for warning in &plan.warnings {
            writeln!(out, "warning: {}", safe_text(warning))?;
        }
        let mut hidden_unchanged = 0;
        let mut warning_counts = std::collections::BTreeMap::new();
        for row in &plan.rows {
            control::check_cancelled()?;
            let show = self.verbose || row.action != "unchanged";
            if show {
                let (label, color) = match row.action {
                    "rename" => ("RENAME", "36"),
                    "quarantine" => ("ARCHIVE", "33"),
                    "needs_decision" => ("ASK", "33"),
                    "skip" => ("SKIP", "2"),
                    _ => ("KEEP", "2"),
                };
                write!(
                    out,
                    "{} {:?}",
                    self.paint(&format!("{label:7}"), color, stderr),
                    row.source
                )?;
                match row.action {
                    "rename" => write!(
                        out,
                        " -> {:?} [{}]",
                        row.target.as_ref().context("missing target")?,
                        row.time_source
                    )?,
                    "quarantine" => write!(out, " -> recoverable duplicate storage")?,
                    "needs_decision" => write!(out, " (exact duplicate; choose during apply)")?,
                    "skip" => write!(out, " (duplicate group)")?,
                    _ => (),
                }
                writeln!(out)?;
            } else {
                hidden_unchanged += 1;
            }
            for warning in &row.warnings {
                if self.verbose {
                    writeln!(out, "       warning: {}", safe_text(warning))?;
                }
                *warning_counts.entry(warning.as_str()).or_insert(0usize) += 1;
            }
        }
        if hidden_unchanged > 0 {
            writeln!(
                out,
                "{hidden_unchanged} already correctly named (--verbose shows them)."
            )?;
        }
        if !self.verbose {
            for (warning, count) in warning_counts {
                writeln!(out, "warning: {} ({count} file(s))", safe_text(warning))?;
            }
        }
        let s = &plan.summary;
        writeln!(
            out,
            "\n{} file(s) · {} · {} rename · {} archive · {} unchanged · {} skipped",
            plan.scanned,
            human_bytes(s.total_bytes),
            s.rename,
            s.quarantine,
            s.unchanged,
            s.skipped_duplicates + plan.skipped_non_images
        )?;
        writeln!(
            out,
            "Dates: {} EXIF / {} MP4 / {} mtime. Duplicates: {} group(s), {} awaiting choice.",
            s.exif_dates,
            s.mp4_dates,
            s.mtime_dates,
            plan.duplicates.len(),
            plan.pending_groups
        )?;
        if s.quarantine > 0 {
            writeln!(
                out,
                "Archived copies retain {} and can be restored with undo.",
                human_bytes(s.quarantined_bytes)
            )?;
        }
        Ok(())
    }
}

/// Keep Unicode filenames readable while preventing terminal-control injection in
/// diagnostics assembled by dependencies or paths formatted with Display.
pub fn safe_text(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_control() {
            safe.extend(ch.escape_default());
        } else {
            safe.push(ch);
        }
    }
    safe
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn write_json(value: &impl serde::Serialize) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer_pretty(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

pub struct Progress {
    state: Arc<Mutex<(usize, usize)>>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Progress {
    fn new(label: &str, enabled: bool) -> Self {
        let state = Arc::new(Mutex::new((0, 0)));
        if !enabled {
            return Self {
                state,
                stop: None,
                thread: None,
            };
        }
        let worker_state = Arc::clone(&state);
        let (stop, receiver) = mpsc::channel();
        let label = label.to_owned();
        let thread = thread::spawn(move || {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut frame = 0;
            loop {
                if !matches!(
                    receiver.recv_timeout(Duration::from_millis(100)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    break;
                }
                let Ok(counts) = worker_state.lock() else {
                    break;
                };
                let mut out = io::stderr().lock();
                let action = if control::is_cancelled() {
                    "Stopping safely; preserving files…"
                } else {
                    &label
                };
                let written = if counts.1 > 0 {
                    write!(
                        out,
                        "\r\x1b[2K{} {} {}/{}",
                        frames[frame % frames.len()],
                        action,
                        counts.0,
                        counts.1
                    )
                } else {
                    write!(out, "\r\x1b[2K{} {}", frames[frame % frames.len()], action)
                };
                if written.and_then(|_| out.flush()).is_err() {
                    break;
                }
                frame += 1;
            }
            let mut out = io::stderr().lock();
            let _ = write!(out, "\r\x1b[2K");
            let _ = out.flush();
        });
        Self {
            state,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    pub fn update(&self, done: usize, total: usize) {
        if let Ok(mut state) = self.state.lock() {
            *state = (done, total);
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A single input worker allows cancellation while waiting at a prompt. It never
/// uses /dev/tty behind the caller's back or reads redirected input as approval.
pub struct Input {
    lines: Option<mpsc::Receiver<io::Result<String>>>,
}

impl Input {
    pub fn new() -> Self {
        Self { lines: None }
    }
    pub fn line(&mut self) -> Result<String> {
        if self.lines.is_none() {
            let (sender, receiver) = mpsc::channel();
            thread::spawn(move || {
                use io::BufRead;
                let stdin = io::stdin();
                for line in stdin.lock().lines() {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
            });
            self.lines = Some(receiver);
        }
        let receiver = self.lines.as_ref().context("input worker unavailable")?;
        loop {
            control::check_cancelled()?;
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(line) => return line.context("cannot read terminal input"),
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(control::Cancelled).context("input ended; no approval received");
                }
            }
        }
    }
}
