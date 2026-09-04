//! Interactive terminal interface for local aria2c sessions.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use super::resources::Locale;
use aria2_core::engine::engine_command::{EngineCommand, EngineCommandSender};
use aria2_core::request::request_group::{DownloadOptions, GroupId};
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

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;
const REMOTE_PAGE_SIZE: u64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputMode {
    None,
    Add,
    Filter,
}

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

/// Run the TUI as a JSON-RPC client for an existing aria2 instance.
pub async fn run_remote(
    url: String,
    secret: Option<String>,
    language: Option<String>,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| format!("failed to initialize RPC client: {error}"))?;
    let locale = Locale::from_arg_or_environment(language.as_deref());
    let mut terminal = setup_terminal()?;
    let mut selected = 0usize;
    let mut filter = String::new();
    let mut input = None::<String>;
    let mut input_mode = InputMode::None;
    let mut details = false;
    let mut page = 0usize;
    let mut has_next_page = false;
    let mut all_tasks = Vec::new();
    let mut next_refresh = Instant::now();
    let mut rpc_error = None::<String>;
    let result = loop {
        if Instant::now() >= next_refresh {
            match remote_tasks(&client, &url, secret.as_deref(), page).await {
                Ok((tasks, next_page)) => {
                    all_tasks = tasks;
                    has_next_page = next_page;
                    rpc_error = None;
                    let refresh_interval = if all_tasks.iter().any(|task| task.status == "active") {
                        Duration::from_millis(750)
                    } else {
                        Duration::from_secs(3)
                    };
                    next_refresh = Instant::now() + refresh_interval;
                }
                Err(error) => {
                    rpc_error = Some(error);
                    next_refresh = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        let tasks = filtered_remote_tasks(&all_tasks, &filter);
        selected = selected.min(tasks.len().saturating_sub(1));
        terminal
            .draw(|frame| {
                draw_remote(
                    frame,
                    &tasks,
                    selected,
                    locale,
                    input.as_deref(),
                    input_mode,
                    details,
                    &filter,
                    page,
                    has_next_page,
                    rpc_error.as_deref(),
                )
            })
            .map_err(|error| format!("failed to draw TUI: {error}"))?;
        if !tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(500)))
            .await
            .map_err(|error| format!("TUI event task failed: {error}"))?
            .map_err(|error| format!("failed to read terminal event: {error}"))?
        {
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
            KeyCode::Char('a') if input.is_none() => {
                input = Some(String::new());
                input_mode = InputMode::Add;
            }
            KeyCode::Char('/') if input.is_none() => {
                input = Some(filter.clone());
                input_mode = InputMode::Filter;
            }
            KeyCode::Char(c) if input.is_some() => input.as_mut().expect("input mode").push(c),
            KeyCode::Backspace if input.is_some() => {
                input.as_mut().expect("input mode").pop();
            }
            KeyCode::Enter if input_mode == InputMode::Add => {
                let value = input.take().unwrap_or_default().trim().to_string();
                if !value.is_empty() {
                    if let Err(error) = rpc_call(
                        &client,
                        &url,
                        secret.as_deref(),
                        "aria2.addUri",
                        serde_json::json!([[value]]),
                    )
                    .await
                    {
                        rpc_error = Some(error);
                    }
                }
                next_refresh = Instant::now();
                input_mode = InputMode::None;
            }
            KeyCode::Enter if input_mode == InputMode::Filter => {
                filter = input.take().unwrap_or_default();
                input_mode = InputMode::None;
            }
            KeyCode::Esc if input.is_some() => {
                input = None;
                input_mode = InputMode::None;
            }
            KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
            KeyCode::Char('[') | KeyCode::PageUp if input.is_none() => {
                if page > 0 {
                    page -= 1;
                    next_refresh = Instant::now();
                }
            }
            KeyCode::Char(']') | KeyCode::PageDown if input.is_none() => {
                if has_next_page {
                    page += 1;
                    next_refresh = Instant::now();
                }
            }
            KeyCode::Down => selected = selected.saturating_add(1),
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Char('p') => {
                if let Some(task) = tasks.get(selected) {
                    let method = if task.status == "paused" {
                        "aria2.unpause"
                    } else {
                        "aria2.pause"
                    };
                    if let Err(error) = rpc_call(
                        &client,
                        &url,
                        secret.as_deref(),
                        method,
                        serde_json::json!([task.gid]),
                    )
                    .await
                    {
                        rpc_error = Some(error);
                    }
                    next_refresh = Instant::now();
                }
            }
            KeyCode::Char('r') => {
                if let Some(task) = tasks.get(selected) {
                    if let Err(error) = rpc_call(
                        &client,
                        &url,
                        secret.as_deref(),
                        "aria2.remove",
                        serde_json::json!([task.gid]),
                    )
                    .await
                    {
                        rpc_error = Some(error);
                    }
                    next_refresh = Instant::now();
                }
            }
            KeyCode::Char('d') if input.is_none() => details = !details,
            _ => {}
        }
    };
    restore_terminal(&mut terminal)?;
    result
}

#[derive(Clone, Debug)]
struct RemoteTask {
    gid: String,
    status: String,
    completed: u64,
    total: u64,
    speed: u64,
    input: String,
}

async fn remote_tasks(
    client: &reqwest::Client,
    url: &str,
    secret: Option<&str>,
    page: usize,
) -> Result<(Vec<RemoteTask>, bool), String> {
    let mut tasks = Vec::new();
    let responses = rpc_batch(
        client,
        url,
        secret,
        [
            ("aria2.tellActive", serde_json::json!([])),
            (
                "aria2.tellWaiting",
                serde_json::json!([page as u64 * REMOTE_PAGE_SIZE, REMOTE_PAGE_SIZE]),
            ),
            (
                "aria2.tellStopped",
                serde_json::json!([page as u64 * REMOTE_PAGE_SIZE, REMOTE_PAGE_SIZE]),
            ),
        ],
    )
    .await?;
    let has_next_page = responses.iter().skip(1).any(|value| {
        value
            .as_array()
            .is_some_and(|items| items.len() == REMOTE_PAGE_SIZE as usize)
    });
    for value in responses {
        if let Some(items) = value.as_array() {
            tasks.extend(items.iter().map(remote_task));
        }
    }
    Ok((tasks, has_next_page))
}

async fn rpc_batch<const N: usize>(
    client: &reqwest::Client,
    url: &str,
    secret: Option<&str>,
    calls: [(&str, serde_json::Value); N],
) -> Result<Vec<serde_json::Value>, String> {
    let requests = calls
        .into_iter()
        .enumerate()
        .map(|(id, (method, mut params))| {
            if let Some(secret) = secret {
                params
                    .as_array_mut()
                    .expect("RPC params are an array")
                    .insert(0, serde_json::Value::String(format!("token:{secret}")));
            }
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": format!("aria2c-tui-{id}"),
                "method": method,
                "params": params,
            })
        })
        .collect::<Vec<_>>();
    let response = client
        .post(url)
        .json(&requests)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    let responses = body
        .as_array()
        .ok_or_else(|| "RPC batch response is not an array".to_string())?;
    responses
        .iter()
        .map(|response| {
            if let Some(error) = response.get("error") {
                return Err(error.to_string());
            }
            Ok(response
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null))
        })
        .collect()
}

fn remote_task(value: &serde_json::Value) -> RemoteTask {
    let string = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let number = |key: &str| string(key).parse().unwrap_or(0);
    RemoteTask {
        gid: string("gid"),
        status: string("status"),
        completed: number("completedLength"),
        total: number("totalLength"),
        speed: number("downloadSpeed"),
        input: value
            .get("files")
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
            .and_then(|v| v.get("uris"))
            .and_then(|v| v.as_array())
            .and_then(|v| v.first())
            .and_then(|v| v.get("uri"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

async fn rpc_call(
    client: &reqwest::Client,
    url: &str,
    secret: Option<&str>,
    method: &str,
    mut params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    if let Some(secret) = secret {
        params
            .as_array_mut()
            .expect("RPC params are an array")
            .insert(0, serde_json::Value::String(format!("token:{secret}")));
    }
    let response = client
        .post(url)
        .json(
            &serde_json::json!({"jsonrpc":"2.0","id":"aria2c-tui","method":method,"params":params}),
        )
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if let Some(error) = body.get("error") {
        return Err(error.to_string());
    }
    Ok(body
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

fn draw_remote(
    frame: &mut ratatui::Frame<'_>,
    tasks: &[&RemoteTask],
    selected: usize,
    locale: Locale,
    input: Option<&str>,
    input_mode: InputMode,
    details: bool,
    filter: &str,
    page: usize,
    has_next_page: bool,
    rpc_error: Option<&str>,
) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if details {
            vec![
                Constraint::Min(3),
                Constraint::Length(5),
                Constraint::Length(2),
            ]
        } else {
            vec![Constraint::Min(3), Constraint::Length(2)]
        })
        .split(frame.area());
    let rows = tasks.iter().map(|task| {
        Row::new([
            task.gid.clone(),
            locale.remote_status(&task.status).to_string(),
            format!(
                "{:.1}%",
                if task.total == 0 {
                    0.0
                } else {
                    task.completed as f64 * 100.0 / task.total as f64
                }
            ),
            format_speed(task.speed),
            task.input.clone(),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Min(20),
        ],
    )
    .block(
        Block::default()
            .title(format!("{} (RPC)", locale.title()))
            .borders(Borders::ALL),
    )
    .header(
        Row::new(locale.remote_headers()).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = TableState::default();
    if !tasks.is_empty() {
        state.select(Some(selected));
    }
    if tasks.is_empty() {
        frame.render_widget(
            Paragraph::new(locale.empty()).block(Block::default().borders(Borders::ALL)),
            areas[0],
        );
    } else {
        frame.render_stateful_widget(table, areas[0], &mut state);
    }
    if details {
        let text = tasks
            .get(selected)
            .map(|task| {
                let (gid, status, completed, speed, input) = locale.detail_labels();
                format!(
                    "{gid}: {}\n{status}: {}\n{completed}: {} / {} bytes\n{speed}: {}\n{input}: {}",
                    task.gid,
                    locale.remote_status(&task.status),
                    task.completed,
                    task.total,
                    format_speed(task.speed),
                    task.input
                )
            })
            .unwrap_or_else(|| locale.empty().to_string());
        frame.render_widget(
            Paragraph::new(text).block(
                Block::default()
                    .title(locale.details())
                    .borders(Borders::ALL),
            ),
            areas[1],
        );
    }
    let footer = input.map_or_else(
        || {
            let page_status = locale.page(page + 1, has_next_page);
            let footer = if filter.is_empty() {
                locale.footer().to_string()
            } else {
                format!("{}: {filter}", locale.filtered())
            };
            format!("{footer}  {page_status}")
        },
        |value| {
            format!(
                "{}: {value}_",
                if input_mode == InputMode::Filter {
                    locale.filter_prompt()
                } else {
                    locale.add_prompt()
                }
            )
        },
    );
    let footer = match rpc_error {
        Some(error) => format!("{footer}  {}", locale.error(error)),
        None => footer,
    };
    frame.render_widget(
        Paragraph::new(footer),
        if details { areas[2] } else { areas[1] },
    );
}

fn filtered_remote_tasks<'a>(tasks: &'a [RemoteTask], filter: &str) -> Vec<&'a RemoteTask> {
    let needle = filter.to_ascii_lowercase();
    tasks
        .iter()
        .filter(|task| {
            needle.is_empty()
                || task.gid.to_ascii_lowercase().contains(&needle)
                || task.input.to_ascii_lowercase().contains(&needle)
        })
        .collect()
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
    let mut input_mode = InputMode::None;
    let mut filter = String::new();
    let mut details = false;

    loop {
        let groups = request_man.all_groups();
        let visible_groups = filtered_groups(&groups, &filter);
        if visible_groups.is_empty() {
            table_state.select(None);
        } else {
            selected = selected.min(visible_groups.len() - 1);
            table_state.select(Some(selected));
        }
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    &visible_groups,
                    &mut table_state,
                    locale,
                    input.as_deref(),
                    input_mode,
                    details,
                    &filter,
                )
            })
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
            KeyCode::Char('a') if input.is_none() => {
                input = Some(String::new());
                input_mode = InputMode::Add;
            }
            KeyCode::Char('/') if input.is_none() => {
                input = Some(filter.clone());
                input_mode = InputMode::Filter;
            }
            KeyCode::Char(character) if input.is_some() => {
                input
                    .as_mut()
                    .expect("input mode must be active")
                    .push(character);
            }
            KeyCode::Backspace if input.is_some() => {
                input.as_mut().expect("input mode must be active").pop();
            }
            KeyCode::Enter if input.is_some() && input_mode == InputMode::Add => {
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
                input_mode = InputMode::None;
            }
            KeyCode::Enter if input.is_some() && input_mode == InputMode::Filter => {
                filter = input.take().unwrap_or_default();
                input_mode = InputMode::None;
            }
            KeyCode::Esc if input.is_some() => {
                input = None;
                input_mode = InputMode::None;
            }
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
                if let Some((gid, group)) = visible_groups.get(selected) {
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
                if let Some((gid, _)) = visible_groups.get(selected) {
                    request_man
                        .remove_group(*gid)
                        .map_err(|error| format!("failed to remove task: {error}"))?;
                    command_tx
                        .send(EngineCommand::RemoveDownload { gid: *gid })
                        .map_err(|error| format!("failed to remove task: {error}"))?;
                }
            }
            KeyCode::Char('d') if input.is_none() => details = !details,
            _ => {}
        }
    }
}

fn draw(
    frame: &mut ratatui::Frame<'_>,
    groups: &[&(
        GroupId,
        std::sync::Arc<std::sync::RwLock<aria2_core::request::request_group::RequestGroup>>,
    )],
    table_state: &mut TableState,
    locale: Locale,
    input: Option<&str>,
    input_mode: InputMode,
    details: bool,
    filter: &str,
) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(if details {
            vec![
                Constraint::Min(3),
                Constraint::Length(5),
                Constraint::Length(2),
            ]
        } else {
            vec![Constraint::Min(3), Constraint::Length(2)]
        })
        .split(frame.area());
    let header = Row::new(locale.headers()).style(
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
    if details {
        let text = groups
            .get(table_state.selected().unwrap_or(0))
            .map(|(_, group)| {
                let group = group.recover();
                let snapshot = group.status_snapshot();
                let (gid, status, completed, speed, input) = locale.detail_labels();
                format!(
                    "{gid}: {}\n{status}: {}\n{completed}: {} / {} bytes\n{speed}: {}\n{input}: {}",
                    group.gid().to_hex_string(),
                    locale.status(&snapshot.status),
                    snapshot.completed_length,
                    snapshot.total_length,
                    format_speed(snapshot.download_speed),
                    group.uris().first().map(|u| u.as_ref()).unwrap_or("-")
                )
            })
            .unwrap_or_else(|| locale.empty().to_string());
        frame.render_widget(
            Paragraph::new(text).block(
                Block::default()
                    .title(locale.details())
                    .borders(Borders::ALL),
            ),
            areas[1],
        );
    }
    let footer = input.map_or_else(
        || {
            if filter.is_empty() {
                locale.footer().to_string()
            } else {
                format!("{}: {filter}", locale.filtered())
            }
        },
        |value| {
            format!(
                "{}: {value}_",
                if input_mode == InputMode::Filter {
                    locale.filter_prompt()
                } else {
                    locale.add_prompt()
                }
            )
        },
    );
    let footer_area = if details { areas[2] } else { areas[1] };
    frame.render_widget(Paragraph::new(Line::from(Span::raw(footer))), footer_area);
}

fn filtered_groups<'a>(
    groups: &'a [(
        GroupId,
        std::sync::Arc<std::sync::RwLock<aria2_core::request::request_group::RequestGroup>>,
    )],
    filter: &str,
) -> Vec<&'a (
    GroupId,
    std::sync::Arc<std::sync::RwLock<aria2_core::request::request_group::RequestGroup>>,
)> {
    let needle = filter.to_ascii_lowercase();
    groups
        .iter()
        .filter(|(_, group)| {
            needle.is_empty()
                || group
                    .recover()
                    .uris()
                    .iter()
                    .any(|uri| uri.to_ascii_lowercase().contains(&needle))
        })
        .collect()
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
    use super::super::resources::Locale;

    #[test]
    fn locale_selects_chinese_from_locale_name() {
        assert_eq!(
            Locale::from_arg_or_environment(Some("zh-CN")),
            Locale::SimplifiedChinese
        );
    }

    #[test]
    fn locale_selects_regional_languages() {
        assert_eq!(
            Locale::from_arg_or_environment(Some("zh-TW")),
            Locale::TraditionalChinese
        );
        assert_eq!(
            Locale::from_arg_or_environment(Some("ru-RU")),
            Locale::Russian
        );
        assert_eq!(
            Locale::from_arg_or_environment(Some("hi-IN")),
            Locale::Hindi
        );
        assert_eq!(
            Locale::from_arg_or_environment(Some("vi-VN")),
            Locale::Vietnamese
        );
        assert_eq!(
            Locale::from_arg_or_environment(Some("id-ID")),
            Locale::Indonesian
        );
        assert_eq!(
            Locale::from_arg_or_environment(Some("bn-BD")),
            Locale::Bengali
        );
        assert_eq!(
            Locale::from_arg_or_environment(Some("ta-IN")),
            Locale::Tamil
        );
        assert_eq!(Locale::from_arg_or_environment(Some("th-TH")), Locale::Thai);
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

    #[test]
    fn every_locale_has_complete_display_resources() {
        let locales = [
            Locale::English,
            Locale::SimplifiedChinese,
            Locale::TraditionalChinese,
            Locale::Japanese,
            Locale::Spanish,
            Locale::Russian,
            Locale::Hindi,
            Locale::Bengali,
            Locale::Tamil,
            Locale::Vietnamese,
            Locale::Thai,
            Locale::Indonesian,
        ];
        for locale in locales {
            assert!(locale.headers().iter().all(|text| !text.is_empty()));
            assert!(locale.remote_headers().iter().all(|text| !text.is_empty()));
            assert!(!locale.page(1, true).contains("{page}"));
            assert!(!locale.error("test").contains("{message}"));
        }
    }
}
