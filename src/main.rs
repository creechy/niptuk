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
use bollard::{
    Docker,
    container::{
        ListContainersOptions, 
        StartContainerOptions, 
        StopContainerOptions, 
        RemoveContainerOptions,
        StatsOptions
    },
    models::ContainerSummary,
};
use futures::stream::StreamExt;
use tokio::sync::mpsc;
use std::{
    collections::HashMap,
    io,
    time::{Duration, Instant},
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone)]
struct PreviousStats {
    cpu_total: u64,
    system_cpu: u64,
    timestamp: Instant,
}

static PREVIOUS_STATS: std::sync::LazyLock<Arc<Mutex<HashMap<String, PreviousStats>>>> = 
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

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
    state: String,
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
    receiver: mpsc::UnboundedReceiver<AppMessage>,
    shutdown_sender: tokio::sync::oneshot::Sender<()>,
}

impl App {
    async fn new() -> Result<App, String> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        
        // Spawn background task for stats collection
        let stats_tx = tx.clone();
        tokio::spawn(async move {
            stats_collection_task(stats_tx, shutdown_rx).await;
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
                    // Background task is shutting down
                    break;
                }
            }
        }
    }

    fn shutdown(self) {
        let _ = self.shutdown_sender.send(());
    }
}

async fn stats_collection_task(
    sender: mpsc::UnboundedSender<AppMessage>, 
    mut shutdown_receiver: tokio::sync::oneshot::Receiver<()>
) {
    let mut last_refresh = Instant::now();
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    
    loop {
        tokio::select! {
            _ = &mut shutdown_receiver => {
                let _ = sender.send(AppMessage::Shutdown);
                break;
            }
            _ = interval.tick() => {
                // Refresh data every second
                if last_refresh.elapsed() >= Duration::from_secs(1) {
                    match get_container_stats().await {
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
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    // Check if Docker is available
    if !is_docker_available().await {
        eprintln!("Error: Docker is not available or not running");
        return Ok(());
    }

    // Setup terminal
    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).map_err(|e| e.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| e.to_string())?;

    // Create app and run
    let mut app = App::new().await?;
    let res = run_app(&mut terminal, &mut app).await;

    // Shutdown background task
    app.shutdown();

    // Restore terminal
    disable_raw_mode().map_err(|e| e.to_string())?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    ).map_err(|e| e.to_string())?;
    terminal.show_cursor().map_err(|e| e.to_string())?;

    if let Err(err) = res {
        println!("{}", err);
    }

    Ok(())
}

async fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<(), String> {
    
    loop {
        // Handle messages from background task
        app.handle_messages();

        // Handle user input with immediate navigation response
        if event::poll(Duration::from_millis(16)).map_err(|e| e.to_string())? { // ~60fps polling
            if let Event::Key(key) = event::read().map_err(|e| e.to_string())? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.next();
                        // Immediate redraw for navigation - no data refresh needed
                        terminal.draw(|f| ui(f, app)).map_err(|e| e.to_string())?;
                        continue;
                    },
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.previous();
                        // Immediate redraw for navigation - no data refresh needed
                        terminal.draw(|f| ui(f, app)).map_err(|e| e.to_string())?;
                        continue;
                    },
                    KeyCode::Char('r') => {
                        // Manual refresh - the background task will pick this up automatically
                        // We could add a force refresh mechanism if needed
                    },
                    KeyCode::Char(' ') => {
                        app.toggle_auto_refresh();
                    },
                    KeyCode::Char('s') => {
                        if let Some(container) = app.containers.get(app.selected_index) {
                            let container_id = container.id.clone();
                            let container_state = container.state.clone();
                            tokio::spawn(async move {
                                if container_state == "running" {
                                    let _ = stop_container(&container_id).await;
                                } else {
                                    let _ = start_container(&container_id).await;
                                }
                            });
                        }
                    },
                    KeyCode::Char('d') => {
                        if let Some(container) = app.containers.get(app.selected_index) {
                            if container.state != "running" {
                                let container_id = container.id.clone();
                                tokio::spawn(async move {
                                    let _ = delete_container(&container_id).await;
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Regular redraw (data updates come from background task)
        terminal.draw(|f| ui(f, app)).map_err(|e| e.to_string())?;
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

        let status_color = if container.state == "running" {
            Color::Green
        } else {
            Color::Red
        };

        Row::new(vec![
            Cell::from(container.id.chars().take(12).collect::<String>()),
            Cell::from(container.name.clone()),
            Cell::from(container.status.clone()).style(Style::default().fg(status_color)),
            Cell::from(format!("{:.1}%", container.cpu_percent)).style(Style::default().fg(cpu_color)),
            Cell::from(container.memory_usage.clone()),
            Cell::from(format!("{:.1}%", container.memory_percent)).style(Style::default().fg(mem_color)),
            Cell::from(container.image.clone()),
        ])
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(12),     // ID - fixed width
            Constraint::Fill(1),        // Name - flexible, equal share
            Constraint::Fill(1),        // Status - flexible, equal share  
            Constraint::Length(8),      // CPU % - fixed width
            Constraint::Length(25),     // Memory - fixed width
            Constraint::Length(8),      // Mem % - fixed width
            Constraint::Fill(1),        // Image - flexible, equal share
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

    // Status/Help bar - use actual last update time from app state
    let last_update_time = chrono::DateTime::<chrono::Utc>::from(
        std::time::SystemTime::now() - app.last_update.elapsed()
    );
    let help_text = format!(
        "Last Update: {} | {}",
        last_update_time.format("%H:%M:%S"),
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

async fn is_docker_available() -> bool {
    match Docker::connect_with_socket_defaults() {
        Ok(docker) => {
            docker.version().await.is_ok()
        }
        Err(_) => false,
    }
}

async fn get_container_stats() -> Result<Vec<ContainerInfo>, String> {
    let docker = Docker::connect_with_socket_defaults().map_err(|e| e.to_string())?;
    
    // Get all containers (running and stopped)
    let options = Some(ListContainersOptions::<String> {
        all: true,
        ..Default::default()
    });
    
    let containers = docker.list_containers(options).await.map_err(|e| e.to_string())?;
    let mut container_infos = Vec::new();
    
    // Collect stats for running containers
    let mut stats_map = HashMap::new();
    for container in &containers {
        if let Some(id) = &container.id {
            let state = get_container_state(container);
            if state == "running" {
                if let Ok((cpu, mem_usage, mem_percent)) = get_container_resource_stats(&docker, id).await {
                    stats_map.insert(id.clone(), (cpu, mem_usage, mem_percent));
                }
            }
        }
    }
    
    // Build container info list
    for container in containers {
        if let Some(ref id) = container.id {
            let name = extract_container_name(&container);
            let status = container.status.clone().unwrap_or_else(|| "Unknown".to_string());
            let image = container.image.clone().unwrap_or_else(|| "Unknown".to_string());
            let ports = format_ports(&container);
            let state = get_container_state(&container);
            
            let (cpu_percent, memory_usage, memory_percent) = stats_map
                .get(id)
                .cloned()
                .unwrap_or((0.0, "N/A".to_string(), 0.0));

            container_infos.push(ContainerInfo {
                id: id.clone(),
                name,
                status,
                cpu_percent,
                memory_usage,
                memory_percent,
                image,
                ports,
                state,
            });
        }
    }

    Ok(container_infos)
}

fn get_container_state(container: &ContainerSummary) -> String {
    container.state.as_deref().unwrap_or("unknown").to_string()
}

fn extract_container_name(container: &ContainerSummary) -> String {
    container.names
        .as_ref()
        .and_then(|names| names.first())
        .map(|name| name.trim_start_matches('/').to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

fn format_ports(container: &ContainerSummary) -> String {
    container.ports
        .as_ref()
        .map(|ports| {
            ports.iter()
                .filter_map(|port| {
                    // private_port is u16, public_port is Option<u16>, typ is Option<PortTypeEnum>
                    let private_port = port.private_port;
                    let public_port = port.public_port;
                    let port_type = &port.typ;
                    
                    if let Some(port_type_enum) = port_type {
                        Some(format!("{}:{}->{}/{}", 
                            public_port.map_or("".to_string(), |p| p.to_string()),
                            private_port,
                            private_port,
                            port_type_enum.to_string()
                        ))
                    } else {
                        Some(format!("{}:{}->{}/tcp", 
                            public_port.map_or("".to_string(), |p| p.to_string()),
                            private_port,
                            private_port
                        ))
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|| "".to_string())
}

async fn get_container_resource_stats(
    docker: &Docker, 
    container_id: &str
) -> Result<(f64, String, f64), String> {
    let options = Some(StatsOptions {
        stream: false,
        one_shot: true,
    });
    
    let mut stats_stream = docker.stats(container_id, options);
    
    if let Some(stats_result) = stats_stream.next().await {
        let stats = stats_result.map_err(|e| e.to_string())?;
        
        // Calculate CPU percentage using cached previous values
        let cpu_percent = {
            let current_cpu_total = stats.cpu_stats.cpu_usage.total_usage;
            let current_system_cpu = stats.cpu_stats.system_cpu_usage.unwrap_or(0);
            let current_time = Instant::now();
            let number_cpus = stats.cpu_stats.online_cpus.unwrap_or_else(|| {
                stats.cpu_stats.cpu_usage.percpu_usage.as_ref().map(|v| v.len() as u64).unwrap_or(1)
            }) as f64;
            
            let mut previous_stats_map = PREVIOUS_STATS.lock().unwrap();
            
            let cpu_percent = if let Some(prev) = previous_stats_map.get(container_id) {
                let cpu_delta = current_cpu_total.saturating_sub(prev.cpu_total) as f64;
                let system_delta = current_system_cpu.saturating_sub(prev.system_cpu) as f64;
                let time_delta = current_time.duration_since(prev.timestamp).as_secs_f64();
                
                if system_delta > 0.0 && time_delta > 0.0 {
                    (cpu_delta / system_delta) * number_cpus * 100.0
                } else {
                    0.0
                }
            } else {
                0.0 // First measurement, no previous data
            };
            
            // Update the cache with current values
            previous_stats_map.insert(container_id.to_string(), PreviousStats {
                cpu_total: current_cpu_total,
                system_cpu: current_system_cpu,
                timestamp: current_time,
            });
            
            cpu_percent
        };
        
        // Calculate memory usage and percentage - Fixed to handle direct MemoryStats struct
        let (memory_usage_str, memory_percent) = {
            let memory_stats = &stats.memory_stats;
            let usage = memory_stats.usage.unwrap_or(0);
            let limit = memory_stats.limit.unwrap_or(1);
            
            let usage_mb = usage as f64 / 1024.0 / 1024.0;
            let limit_mb = limit as f64 / 1024.0 / 1024.0;
            let percent = if limit > 0 { (usage as f64 / limit as f64) * 100.0 } else { 0.0 };
            
            if usage > 0 && limit > 0 {
                (format!("{:.1}MB / {:.1}MB", usage_mb, limit_mb), percent)
            } else {
                ("N/A".to_string(), 0.0)
            }
        };
        
        Ok((cpu_percent, memory_usage_str, memory_percent))
    } else {
        Ok((0.0, "N/A".to_string(), 0.0))
    }
}

async fn start_container(container_id: &str) -> Result<(), String> {
    let docker = Docker::connect_with_socket_defaults().map_err(|e| e.to_string())?;
    docker.start_container(container_id, None::<StartContainerOptions<String>>).await.map_err(|e| e.to_string())?;
    Ok(())
}

async fn stop_container(container_id: &str) -> Result<(), String> {
    let docker = Docker::connect_with_socket_defaults().map_err(|e| e.to_string())?;
    docker.stop_container(container_id, None::<StopContainerOptions>).await.map_err(|e| e.to_string())?;
    Ok(())
}

async fn delete_container(container_id: &str) -> Result<(), String> {
    let docker = Docker::connect_with_socket_defaults().map_err(|e| e.to_string())?;
    let options = Some(RemoveContainerOptions {
        force: false,
        v: true,
        link: false,
    });
    docker.remove_container(container_id, options).await.map_err(|e| e.to_string())?;
    Ok(())
}
