//! Interactive terminal interface for local aria2c sessions.

use std::io::{self, Stdout};
use std::time::Duration;

use aria2_core::engine::engine_command::{EngineCommand, EngineCommandSender};
use aria2_core::request::request_group::{DownloadOptions, DownloadStatus, GroupId};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::util::rwlock_ext::RwLockRecover;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table, TableState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Locale {
    English,
    SimplifiedChinese,
}

impl Locale {
    fn from_arg_or_environment(value: Option<&str>) -> Self {
        let value = value
            .map(str::to_owned)
            .or_else(|| std::env::var("LC_ALL").ok())
            .or_else(|| std::env::var("LANG").ok())
            .unwrap_or_else(|| "en-US".to_string())
            .to_ascii_lowercase();
        if value.starts_with("zh") {
            Self::SimplifiedChinese
        } else {
            Self::English
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::English => "aria2c TUI",
            Self::SimplifiedChinese => "aria2c 终端界面",
        }
    }

    fn empty(self) -> &'static str {
        match self {
            Self::English => "No downloads. Add a URL as a command-line argument.",
            Self::SimplifiedChinese => "暂无下载任务。请在命令行参数中添加 URL。",
        }
    }

    fn footer(self) -> &'static str {
        match self {
            Self::English => "↑/↓ Select   p Pause/Resume   r Remove   q Quit",
            Self::SimplifiedChinese => "↑/↓ 选择   p 暂停/继续   r 删除   q 退出",
        }
    }

    fn status(self, status: &DownloadStatus) -> String {
        match self {
            Self::English => status.to_string(),
            Self::SimplifiedChinese => match status {
                DownloadStatus::Waiting => "等待".into(),
                DownloadStatus::Active => "下载中".into(),
                DownloadStatus::Paused => "已暂停".into(),
                DownloadStatus::Error(_) => "错误".into(),
                DownloadStatus::Complete => "完成".into(),
                DownloadStatus::Removed => "已删除".into(),
            },
        }
    }

    fn add_prompt(self) -> &'static str {
        match self {
            Self::English => "URL (Enter to add, Esc to cancel)",
            Self::SimplifiedChinese => "URL（回车添加，Esc 取消）",
        }
    }
}

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

/// Run the local interactive UI until the user quits or the terminal closes.
pub async fn run(
    request_man: std::sync::Arc<RequestGroupMan>,
    command_tx: EngineCommandSender,
    language: Option<String>,
    options: Option<DownloadOptions>,
) -> Result<(), String> {
    let locale = Locale::from_arg_or_environment(language.as_deref());
    let mut terminal = setup_terminal()?;
    let result = run_loop(&mut terminal, request_man, command_tx, locale, options).await;
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> Result<TuiTerminal, String> {
    enable_raw_mode().map_err(|error| format!("failed to enable raw mode: {error}"))?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(format!("failed to enter alternate screen: {error}"));
    }
    Terminal::new(CrosstermBackend::new(stdout))
        .map_err(|error| format!("failed to initialize TUI terminal: {error}"))
}

fn restore_terminal(terminal: &mut TuiTerminal) -> Result<(), String> {
    disable_raw_mode().map_err(|error| format!("failed to disable raw mode: {error}"))?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .map_err(|error| format!("failed to leave alternate screen: {error}"))?;
    terminal
        .show_cursor()
        .map_err(|error| format!("failed to restore cursor: {error}"))
}

async fn run_loop(
    terminal: &mut TuiTerminal,
    request_man: std::sync::Arc<RequestGroupMan>,
    command_tx: EngineCommandSender,
    locale: Locale,
    options: Option<DownloadOptions>,
) -> Result<(), String> {
    let mut table_state = TableState::default();
    let mut selected = 0usize;
    let mut input = None::<String>;

    loop {
        let groups = request_man.all_groups();
        if groups.is_empty() {
            table_state.select(None);
        } else {
            selected = selected.min(groups.len() - 1);
            table_state.select(Some(selected));
        }
        terminal
            .draw(|frame| draw(frame, &groups, &mut table_state, locale, input.as_deref()))
            .map_err(|error| format!("failed to draw TUI: {error}"))?;

        let has_event = tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(250)))
            .await
            .map_err(|error| format!("TUI event task failed: {error}"))?
            .map_err(|error| format!("failed to read terminal event: {error}"))?;
        if !has_event {
            continue;
        }
        let event = tokio::task::spawn_blocking(event::read)
            .await
            .map_err(|error| format!("TUI event task failed: {error}"))?
            .map_err(|error| format!("failed to read terminal event: {error}"))?;
        let Event::Key(KeyEvent {
            code,
            kind: KeyEventKind::Press,
            ..
        }) = event
        else {
            continue;
        };

        match code {
            KeyCode::Char('a') if input.is_none() => input = Some(String::new()),
            KeyCode::Char(character) if input.is_some() => {
                input
                    .as_mut()
                    .expect("input mode must be active")
                    .push(character);
            }
            KeyCode::Backspace if input.is_some() => {
                input.as_mut().expect("input mode must be active").pop();
            }
            KeyCode::Enter if input.is_some() => {
                let value = input.take().unwrap_or_default().trim().to_string();
                if !value.is_empty() {
                    let task_options = options
                        .as_ref()
                        .ok_or_else(|| "TUI download options are unavailable".to_string())?;
                    let gid = request_man
                        .add_group(vec![value], task_options.clone())
                        .map_err(|error| format!("failed to add task: {error}"))?;
                    let group = request_man
                        .get_group(gid)
                        .ok_or_else(|| "new task disappeared before submission".to_string())?;
                    command_tx
                        .send(EngineCommand::AddDownload { group })
                        .map_err(|error| format!("failed to submit task: {error}"))?;
                }
            }
            KeyCode::Esc if input.is_some() => input = None,
            KeyCode::Char('q') | KeyCode::Esc => {
                command_tx
                    .send(EngineCommand::ForceHaltAll {
                        reason: aria2_core::request::request_group::HaltReason::UserRequest,
                    })
                    .map_err(|error| format!("failed to stop downloads: {error}"))?;
                return Ok(());
            }
            KeyCode::Down => selected = selected.saturating_add(1),
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Char('p') => {
                if let Some((gid, group)) = groups.get(selected) {
                    let status = group.recover().status();
                    let command = if status.is_paused() {
                        EngineCommand::Unpause { gid: *gid }
                    } else {
                        EngineCommand::Pause { gid: *gid }
                    };
                    request_man
                        .find_group(*gid)
                        .ok_or_else(|| "selected task disappeared".to_string())?;
                    command_tx
                        .send(command)
                        .map_err(|error| format!("failed to control task: {error}"))?;
                }
            }
            KeyCode::Char('r') => {
                if let Some((gid, _)) = groups.get(selected) {
                    request_man
                        .remove_group(*gid)
                        .map_err(|error| format!("failed to remove task: {error}"))?;
                    command_tx
                        .send(EngineCommand::RemoveDownload { gid: *gid })
                        .map_err(|error| format!("failed to remove task: {error}"))?;
                }
            }
            _ => {}
        }
    }
}

fn draw(
    frame: &mut ratatui::Frame<'_>,
    groups: &[(
        GroupId,
        std::sync::Arc<std::sync::RwLock<aria2_core::request::request_group::RequestGroup>>,
    )],
    table_state: &mut TableState,
    locale: Locale,
    input: Option<&str>,
) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(frame.area());
    let header = Row::new(["GID", "Status", "Progress", "Speed", "Connections", "Input"]).style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    let rows = groups.iter().map(|(gid, group)| {
        let group = group.recover();
        let snapshot = group.status_snapshot();
        let total = snapshot.total_length;
        let progress = if total == 0 {
            "--".to_string()
        } else {
            format!(
                "{:.1}%",
                snapshot.completed_length as f64 * 100.0 / total as f64
            )
        };
        let input = group.uris().first().map(|uri| uri.as_ref()).unwrap_or("-");
        Row::new([
            gid.to_hex_string(),
            locale.status(&snapshot.status),
            progress,
            format_speed(snapshot.download_speed),
            snapshot.connections.to_string(),
            input.to_string(),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(Block::default().title(locale.title()).borders(Borders::ALL))
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    if groups.is_empty() {
        frame.render_widget(
            Paragraph::new(locale.empty()).block(Block::default().borders(Borders::ALL)),
            areas[0],
        );
    } else {
        frame.render_stateful_widget(table, areas[0], table_state);
    }
    let footer = input.map_or_else(
        || locale.footer().to_string(),
        |value| format!("{}: {value}_", locale.add_prompt()),
    );
    frame.render_widget(Paragraph::new(Line::from(Span::raw(footer))), areas[1]);
}

fn format_speed(bytes_per_second: u64) -> String {
    const UNITS: [&str; 4] = ["B/s", "KiB/s", "MiB/s", "GiB/s"];
    let mut value = bytes_per_second as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::Locale;

    #[test]
    fn locale_selects_chinese_from_locale_name() {
        assert_eq!(
            Locale::from_arg_or_environment(Some("zh-CN")),
            Locale::SimplifiedChinese
        );
    }

    #[test]
    fn unknown_locale_falls_back_to_english() {
        assert_eq!(
            Locale::from_arg_or_environment(Some("fr-FR")),
            Locale::English
        );
    }

    #[test]
    fn locale_has_add_task_prompt() {
        assert!(Locale::SimplifiedChinese.add_prompt().contains("URL"));
    }
}
