use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame, Terminal,
};
use serde_json::Value;
use std::{
    error::Error,
    io,
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
struct ContainerInfo {
    id: String,
    name: String,
    status: String,
    cpu_percent: f64,
    memory_usage: String,
    memory_percent: f64,
    image: String,
    ports: String,
}

#[derive(Debug)]
enum AppMessage {
    ContainerData(Vec<ContainerInfo>),
    Error(String),
    Shutdown,
}

struct App {
    containers: Vec<ContainerInfo>,
    selected_index: usize,
    last_update: Instant,
    error_message: Option<String>,
    auto_refresh: bool,
    receiver: mpsc::Receiver<AppMessage>,
    shutdown_sender: mpsc::Sender<()>,
}

impl App {
    fn new() -> Result<App, Box<dyn Error>> {
        let (tx, rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        
        // Spawn background thread for stats collection
        let stats_tx = tx.clone();
        thread::spawn(move || {
            stats_collection_thread(stats_tx, shutdown_rx);
        });

        Ok(App {
            containers: Vec::new(),
            selected_index: 0,
            last_update: Instant::now(),
            error_message: None,
            auto_refresh: true,
            receiver: rx,
            shutdown_sender: shutdown_tx,
        })
    }

    fn next(&mut self) {
        if !self.containers.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.containers.len();
        }
    }

    fn previous(&mut self) {
        if !self.containers.is_empty() {
            if self.selected_index > 0 {
                self.selected_index -= 1;
            } else {
                self.selected_index = self.containers.len() - 1;
            }
        }
    }

    fn toggle_auto_refresh(&mut self) {
        self.auto_refresh = !self.auto_refresh;
    }

    fn handle_messages(&mut self) {
        // Process all available messages without blocking
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                AppMessage::ContainerData(containers) => {
                    self.containers = containers;
                    self.error_message = None;
                    self.last_update = Instant::now();
                    
                    // Adjust selected index if containers list changed
                    if self.selected_index >= self.containers.len() && !self.containers.is_empty() {
                        self.selected_index = self.containers.len() - 1;
                    }
                }
                AppMessage::Error(error) => {
                    self.error_message = Some(error);
                    self.last_update = Instant::now();
                }
                AppMessage::Shutdown => {
                    // Background thread is shutting down
                    break;
                }
            }
        }
    }

    fn shutdown(&self) {
        let _ = self.shutdown_sender.send(());
    }
}

fn stats_collection_thread(sender: mpsc::Sender<AppMessage>, shutdown_receiver: mpsc::Receiver<()>) {
    let mut last_refresh = Instant::now();
    
    loop {
        // Check for shutdown signal (non-blocking)
        if shutdown_receiver.try_recv().is_ok() {
            let _ = sender.send(AppMessage::Shutdown);
            break;
        }

        // Refresh data every second
        if last_refresh.elapsed() >= Duration::from_secs(1) {
            match get_container_stats() {
                Ok(containers) => {
                    if sender.send(AppMessage::ContainerData(containers)).is_err() {
                        // Main thread has disconnected, exit
                        break;
                    }
                }
                Err(e) => {
                    if sender.send(AppMessage::Error(format!("Error: {}", e))).is_err() {
                        // Main thread has disconnected, exit
                        break;
                    }
                }
            }
            last_refresh = Instant::now();
        }

        // Short sleep to prevent busy waiting
        thread::sleep(Duration::from_millis(100));
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    // Check if Docker is available
    if !is_docker_available() {
        eprintln!("Error: Docker is not available or not running");
        return Ok(());
    }

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app and run
    let mut app = App::new()?;
    let res = run_app(&mut terminal, &mut app);

    // Shutdown background thread
    app.shutdown();

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        println!("{:?}", err);
    }

    Ok(())
}

fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<(), Box<dyn Error>> {
    
    loop {
        // Handle messages from background thread
        app.handle_messages();

        // Handle user input with immediate navigation response
        if event::poll(Duration::from_millis(16))? { // ~60fps polling
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.next();
                        // Immediate redraw for navigation - no data refresh needed
                        terminal.draw(|f| ui(f, app))?;
                        continue;
                    },
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.previous();
                        // Immediate redraw for navigation - no data refresh needed
                        terminal.draw(|f| ui(f, app))?;
                        continue;
                    },
                    KeyCode::Char('r') => {
                        // Manual refresh - the background thread will pick this up automatically
                        // We could add a force refresh mechanism if needed
                    },
                    KeyCode::Char(' ') => {
                        app.toggle_auto_refresh();
                    },
                    KeyCode::Char('s') => {
                        if let Some(container) = app.containers.get(app.selected_index) {
                            if container.status.starts_with("Up") {
                                let _ = stop_container(&container.id);
                            } else {
                                let _ = start_container(&container.id);
                            }
                            // The background thread will refresh data automatically
                        }
                    },
                    KeyCode::Char('d') => {
                        if let Some(container) = app.containers.get(app.selected_index) {
                            if !container.status.starts_with("Up") {
                                let _ = delete_container(&container.id);
                                // The background thread will refresh data automatically
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Regular redraw (data updates come from background thread)
        terminal.draw(|f| ui(f, app))?;
    }
}

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),    // Main table
            Constraint::Length(5),  // Details
            Constraint::Length(3),  // Status/Help
        ])
        .split(f.size());

    // Main table
    let selected_style = Style::default().add_modifier(Modifier::REVERSED);
    let header_cells = ["ID", "Name", "Status", "CPU %", "Memory", "Mem %", "Image"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells).height(1).bottom_margin(1);

    let rows = app.containers.iter().map(|container| {
        let cpu_color = if container.cpu_percent > 80.0 {
            Color::Red
        } else if container.cpu_percent > 50.0 {
            Color::Yellow
        } else {
            Color::Green
        };

        let mem_color = if container.memory_percent > 80.0 {
            Color::Red
        } else if container.memory_percent > 50.0 {
            Color::Yellow
        } else {
            Color::Green
        };

        let status_color = if container.status.starts_with("Up") {
            Color::Green
        } else {
            Color::Red
        };

        Row::new(vec![
            Cell::from(container.id.chars().take(12).collect::<String>()),
            Cell::from(container.name.chars().take(30).collect::<String>()),
            Cell::from(container.status.chars().take(15).collect::<String>()).style(Style::default().fg(status_color)),
            Cell::from(format!("{:.1}%", container.cpu_percent)).style(Style::default().fg(cpu_color)),
            Cell::from(container.memory_usage.clone()),
            Cell::from(format!("{:.1}%", container.memory_percent)).style(Style::default().fg(mem_color)),
            Cell::from(container.image.chars().take(20).collect::<String>()),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(30),
            Constraint::Length(15),
            Constraint::Length(8),
            Constraint::Length(25),
            Constraint::Length(8),
            Constraint::Length(20),
        ]
    )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("Containers"))
        .highlight_style(selected_style)
        .highlight_symbol(">> ");

    let mut state = TableState::default();
    state.select(Some(app.selected_index));
    f.render_stateful_widget(table, chunks[0], &mut state);

    // Details panel for selected container
    if let Some(container) = app.containers.get(app.selected_index) {
        let info_text = vec![
            Line::from(vec![Span::styled("Full ID: ", Style::default().fg(Color::Yellow)), Span::raw(&container.id)]),
            Line::from(vec![Span::styled("Ports: ", Style::default().fg(Color::Yellow)), Span::raw(&container.ports)]),
        ];

        let info_paragraph = Paragraph::new(info_text)
            .block(Block::default().borders(Borders::ALL).title("Details"));
        f.render_widget(info_paragraph, chunks[1]);
    }

    // Status/Help bar
    let help_text = format!(
        "Last Update: {} | {}",
        chrono::Utc::now().format("%H:%M:%S"),
        if app.auto_refresh {
            "Auto-refresh: ON | q/ESC: Quit | ↑↓/jk: Navigate | r: Refresh | Space: Toggle auto-refresh | s: Start/Stop | d: Delete (stopped containers)"
        } else {
            "Auto-refresh: OFF | q/ESC: Quit | ↑↓/jk: Navigate | r: Refresh | Space: Toggle auto-refresh | s: Start/Stop | d: Delete (stopped containers)"
        }
    );

    let help = Paragraph::new(help_text)
        .style(Style::default().fg(Color::Cyan))
        .block(Block::default().borders(Borders::ALL));

    f.render_widget(help, chunks[2]);

    // Show error message if any
    if let Some(error) = &app.error_message {
        let error_popup = Paragraph::new(error.as_str())
            .style(Style::default().fg(Color::Red))
            .block(Block::default().borders(Borders::ALL).title("Error"));
        f.render_widget(error_popup, chunks[2]);
    }
}

fn is_docker_available() -> bool {
    Command::new("docker")
        .arg("version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn get_container_stats() -> Result<Vec<ContainerInfo>, Box<dyn std::error::Error>> {
    let containers_output = Command::new("docker")
        .args(&["ps", "-a", "--format", "{{.ID}}\t{{.Names}}\t{{.Status}}\t{{.Image}}\t{{.Ports}}"])
        .output()?;

    if !containers_output.status.success() {
        return Err("Failed to get container list".into());
    }

    let containers_list = String::from_utf8(containers_output.stdout)?;
    let mut containers = Vec::new();

    for line in containers_list.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 4 {
            let container_id = parts[0].to_string();
            let name = parts[1].to_string();
            let status = parts[2].to_string();
            let image = parts[3].to_string();
            let ports = parts.get(4).unwrap_or(&"").to_string();

            let (cpu_percent, memory_usage, memory_percent) = if status.starts_with("Up") {
                get_container_resource_stats(&container_id).unwrap_or((0.0, "N/A".to_string(), 0.0))
            } else {
                (0.0, "N/A".to_string(), 0.0)
            };

            containers.push(ContainerInfo {
                id: container_id,
                name,
                status,
                cpu_percent,
                memory_usage,
                memory_percent,
                image,
                ports,
            });
        }
    }

    Ok(containers)
}

fn get_container_resource_stats(container_id: &str) -> Result<(f64, String, f64), Box<dyn std::error::Error>> {
    let stats_output = Command::new("docker")
        .args(&["stats", "--no-stream", "--format", "json", container_id])
        .output()?;

    if !stats_output.status.success() {
        return Ok((0.0, "N/A".to_string(), 0.0));
    }

    let stats_json = String::from_utf8(stats_output.stdout)?;
    let stats: Value = serde_json::from_str(&stats_json)?;

    let cpu_percent = stats["CPUPerc"]
        .as_str()
        .unwrap_or("0%")
        .trim_end_matches('%')
        .parse::<f64>()
        .unwrap_or(0.0);

    let memory_usage = stats["MemUsage"]
        .as_str()
        .unwrap_or("N/A")
        .to_string();

    let memory_percent = stats["MemPerc"]
        .as_str()
        .unwrap_or("0%")
        .trim_end_matches('%')
        .parse::<f64>()
        .unwrap_or(0.0);

    Ok((cpu_percent, memory_usage, memory_percent))
}

fn start_container(container_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("docker")
        .args(&["start", container_id])
        .output()?;
        
    if !output.status.success() {
        return Err("Failed to start container".into());
    }
    
    Ok(())
}

fn stop_container(container_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("docker")
        .args(&["stop", container_id])
        .output()?;
        
    if !output.status.success() {
        return Err("Failed to stop container".into());
    }
    
    Ok(())
}

fn delete_container(container_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("docker")
        .args(&["rm", container_id])
        .output()?;
        
    if !output.status.success() {
        return Err("Failed to delete container".into());
    }
    
    Ok(())
}

// Add these dependencies to your Cargo.toml:
/*
[package]
name = "docker-monitor-tui"
version = "0.1.0"
edition = "2021"

[dependencies]
ratatui = "0.26"
crossterm = "0.27"
serde_json = "1.0"
chrono = { version = "0.4", features = ["serde"] }
*/
