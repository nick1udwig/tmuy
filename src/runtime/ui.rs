use std::io::{self, Write};

use anyhow::Result;
use crossterm::cursor::{MoveTo, RestorePosition, SavePosition};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, size};
use crossterm::{execute, queue};

use crate::model::{FsGrant, SessionRecord};

pub(super) struct StatusBarGuard {
    session: SessionRecord,
    detach_key: String,
    rows: Option<u16>,
}

pub(super) struct AlternateScreenGuard;

pub(super) struct BracketedPasteGuard;

impl AlternateScreenGuard {
    pub(super) fn enter() -> Result<Self> {
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )?;
        Ok(Self)
    }
}

impl BracketedPasteGuard {
    pub(super) fn enter() -> Result<Self> {
        let mut stdout = io::stdout();
        execute!(stdout, EnableBracketedPaste)?;
        Ok(Self)
    }
}

impl Drop for AlternateScreenGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, ResetColor);
    }
}

impl Drop for BracketedPasteGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, DisableBracketedPaste);
    }
}

impl StatusBarGuard {
    pub(super) fn enter(session: &SessionRecord, detach_key: &str) -> Result<Self> {
        let (_, rows) = terminal_size();
        let rows = rows.max(2);
        let mut stdout = io::stdout();
        write!(stdout, "\x1b[1;{}r", rows - 1)?;
        stdout.flush()?;
        Ok(Self {
            session: session.clone(),
            detach_key: detach_key.to_string(),
            rows: Some(rows),
        })
    }

    pub(super) fn render(&self, stdout: &mut impl Write) -> Result<()> {
        let Some(rows) = self.rows else {
            return Ok(());
        };
        let (width, _) = terminal_size();
        let line = format_status_bar(&self.session, &self.detach_key, width);
        queue!(
            stdout,
            SavePosition,
            MoveTo(0, rows - 1),
            SetForegroundColor(Color::Black),
            SetBackgroundColor(Color::White),
            Clear(ClearType::CurrentLine),
            Print(line),
            ResetColor,
            RestorePosition
        )?;
        stdout.flush()?;
        Ok(())
    }
}

impl Drop for StatusBarGuard {
    fn drop(&mut self) {
        if let Some(rows) = self.rows {
            let mut stdout = io::stdout();
            let _ = queue!(
                stdout,
                SavePosition,
                MoveTo(0, rows - 1),
                ResetColor,
                Clear(ClearType::CurrentLine),
                RestorePosition
            );
            let _ = write!(stdout, "\x1b[r");
            let _ = stdout.flush();
        }
    }
}

pub(super) struct RawModeGuard;

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

fn format_status_bar(session: &SessionRecord, detach_key: &str, width: u16) -> String {
    let width = width as usize;
    if width == 0 {
        return String::new();
    }

    let content = format!(
        " tmuy {} ({}) | detach {} | sandbox {} ",
        session.current_name,
        session.id_hash,
        detach_key,
        format_sandbox_summary(session)
    );
    fit_status_content(&content, width)
}

fn fit_status_content(content: &str, width: usize) -> String {
    if content.len() >= width {
        if width <= 3 {
            return ".".repeat(width);
        }
        let mut shortened = content[..width - 3].to_string();
        shortened.push_str("...");
        return shortened;
    }

    let mut padded = content.to_string();
    padded.push_str(&" ".repeat(width - content.len()));
    padded
}

fn format_sandbox_summary(session: &SessionRecord) -> String {
    let grants = session
        .sandbox
        .fs
        .iter()
        .map(|grant| match grant {
            FsGrant::Full => "fs:full".to_string(),
            FsGrant::ReadOnly(path) => format!("fs:ro:{}", path.display()),
            FsGrant::ReadWrite(path) => format!("fs:rw:{}", path.display()),
        })
        .collect::<Vec<_>>()
        .join(",");
    let net = match session.sandbox.net {
        crate::model::NetworkMode::On => "net:on",
        crate::model::NetworkMode::Off => "net:off",
    };
    format!("{grants} {net}")
}

fn terminal_size() -> (u16, u16) {
    match size() {
        Ok((cols, rows)) if cols >= 1 && rows >= 1 => (cols, rows),
        _ => (80, 24),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use chrono::Utc;

    use super::*;
    use crate::model::{CommandMode, FsGrant, NetworkMode, SandboxSpec, SessionStatus};

    fn sample_session() -> SessionRecord {
        SessionRecord {
            id_hash: "1a2b3c4".to_string(),
            started_name: "demo".to_string(),
            current_name: "demo".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            cwd: PathBuf::from("/tmp/demo"),
            command: vec!["/bin/sh".to_string()],
            mode: CommandMode::Shell,
            sandbox: SandboxSpec {
                fs: vec![
                    FsGrant::ReadOnly(PathBuf::from("/tmp/demo")),
                    FsGrant::ReadWrite(PathBuf::from("/tmp/out")),
                ],
                net: NetworkMode::Off,
            },
            status: SessionStatus::Live,
            started_log_dir: PathBuf::from("/tmp/log"),
            meta_path: PathBuf::from("/tmp/log/meta.json"),
            log_path: PathBuf::from("/tmp/log/pty.log"),
            events_path: PathBuf::from("/tmp/log/events.jsonl"),
            socket_path: PathBuf::from("/tmp/live.sock"),
            service_pid: Some(1),
            child_pid: Some(2),
            exit_code: None,
            failure_reason: None,
            env: BTreeMap::new(),
            detach_key: "C-b d".to_string(),
        }
    }

    #[test]
    fn format_status_bar_includes_requested_fields() {
        let session = sample_session();
        let line = format_status_bar(&session, "C-b d", 200);
        assert!(line.contains("demo"));
        assert!(line.contains("1a2b3c4"));
        assert!(line.contains("C-b d"));
        assert!(line.contains("fs:ro:/tmp/demo"));
        assert!(line.contains("fs:rw:/tmp/out"));
        assert!(line.contains("net:off"));
    }

    #[test]
    fn format_status_bar_truncates_to_width() {
        let session = sample_session();
        let line = format_status_bar(&session, "C-b d", 20);
        assert_eq!(line.len(), 20);
        assert!(line.ends_with("..."));
    }

    #[test]
    fn fit_status_content_handles_zero_and_tiny_widths() {
        assert_eq!(format_status_bar(&sample_session(), "C-b d", 0), "");
        assert_eq!(fit_status_content("abcdef", 3), "...");
        assert_eq!(fit_status_content("abcdef", 2), "..");
    }

    #[test]
    fn status_bar_guard_render_handles_missing_rows() {
        let session = sample_session();
        let guard = StatusBarGuard {
            session: session.clone(),
            detach_key: "C-b d".to_string(),
            rows: None,
        };
        let mut buf = Vec::new();
        guard.render(&mut buf).unwrap();
        assert!(buf.is_empty());

        let guard = StatusBarGuard {
            session,
            detach_key: "C-b d".to_string(),
            rows: Some(2),
        };
        guard.render(&mut buf).unwrap();
        assert!(!buf.is_empty());
    }
}
