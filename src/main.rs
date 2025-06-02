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

#[derive(Debug)]
enum BackgroundCommand {
    SetAutoRefresh(bool),
    ForceRefresh,
    Shutdown,
}

// NEW: Shared Docker client struct
#[derive(Clone)]
struct SharedDockerClient {
    docker: Arc<Docker>,
}

impl SharedDockerClient {
    async fn new() -> Result<Self, String> {
        let docker = Docker::connect_with_socket_defaults().map_err(|e| e.to_string())?;
        Ok(Self {
            docker: Arc::new(docker),
        })
    }
}

struct App {
    containers: Vec<ContainerInfo>,
    selected_index: usize,
    last_update: Instant,
    error_message: Option<String>,
    auto_refresh: bool,
    receiver: mpsc::UnboundedReceiver<AppMessage>,
    background_sender: mpsc::UnboundedSender<BackgroundCommand>,
    shutdown_sender: tokio::sync::oneshot::Sender<()>,
}

impl App {
    async fn new() -> Result<App, String> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (bg_tx, bg_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        
        // Spawn background task for stats collection
        let stats_tx = tx.clone();
        tokio::spawn(async move {
            stats_collection_task_optimized(stats_tx, bg_rx, shutdown_rx).await;
        });

        Ok(App {
            containers: Vec::new(),
            selected_index: 0,
            last_update: Instant::now(),
            error_message: None,
            auto_refresh: true,
            receiver: rx,
            background_sender: bg_tx,
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
        let _ = self.background_sender.send(BackgroundCommand::SetAutoRefresh(self.auto_refresh));
    }

    fn force_refresh(&mut self) {
        let _ = self.background_sender.send(BackgroundCommand::ForceRefresh);
    }

    fn handle_messages(&mut self) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                AppMessage::ContainerData(containers) => {
                    self.containers = containers;
                    self.error_message = None;
                    self.last_update = Instant::now();
                    
                    if self.selected_index >= self.containers.len() && !self.containers.is_empty() {
                        self.selected_index = self.containers.len() - 1;
                    }
                }
                AppMessage::Error(error) => {
                    self.error_message = Some(error);
                    self.last_update = Instant::now();
                }
                AppMessage::Shutdown => {
                    break;
                }
            }
        }
    }

    fn shutdown(self) {
        let _ = self.background_sender.send(BackgroundCommand::Shutdown);
        let _ = self.shutdown_sender.send(());
    }
}

// MODIFIED: Optimized background task with shared Docker client
async fn stats_collection_task_optimized(
    sender: mpsc::UnboundedSender<AppMessage>,
    mut command_receiver: mpsc::UnboundedReceiver<BackgroundCommand>,
    mut shutdown_receiver: tokio::sync::oneshot::Receiver<()>
) {
    // Create shared Docker client once
    let docker_client = match SharedDockerClient::new().await {
        Ok(client) => client,
        Err(e) => {
            let _ = sender.send(AppMessage::Error(format!("Failed to connect to Docker: {}", e)));
            return;
        }
    };
    
    let mut last_refresh = Instant::now();
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    let mut auto_refresh = true;
    
    // Get initial data
    match get_container_stats_optimized(&docker_client).await {
        Ok(containers) => {
            let _ = sender.send(AppMessage::ContainerData(containers));
        }
        Err(e) => {
            let _ = sender.send(AppMessage::Error(format!("Error: {}", e)));
        }
    }
    last_refresh = Instant::now();
    
    loop {
        tokio::select! {
            _ = &mut shutdown_receiver => {
                let _ = sender.send(AppMessage::Shutdown);
                break;
            }
            command = command_receiver.recv() => {
                match command {
                    Some(BackgroundCommand::SetAutoRefresh(enabled)) => {
                        auto_refresh = enabled;
                    }
                    Some(BackgroundCommand::ForceRefresh) => {
                        match get_container_stats_optimized(&docker_client).await {
                            Ok(containers) => {
                                if sender.send(AppMessage::ContainerData(containers)).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                if sender.send(AppMessage::Error(format!("Error: {}", e))).is_err() {
                                    break;
                                }
                            }
                        }
                        last_refresh = Instant::now();
                    }
                    Some(BackgroundCommand::Shutdown) | None => {
                        let _ = sender.send(AppMessage::Shutdown);
                        break;
                    }
                }
            }
            _ = interval.tick() => {
                if auto_refresh && last_refresh.elapsed() >= Duration::from_secs(1) {
                    match get_container_stats_optimized(&docker_client).await {
                        Ok(containers) => {
                            if sender.send(AppMessage::ContainerData(containers)).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            if sender.send(AppMessage::Error(format!("Error: {}", e))).is_err() {
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

// NEW: Optimized stats collection function
async fn get_container_stats_optimized(docker_client: &SharedDockerClient) -> Result<Vec<ContainerInfo>, String> {
    let docker = &docker_client.docker;
    
    // Get all containers with timeout
    let containers_future = docker.list_containers(Some(ListContainersOptions::<String> {
        all: true,
        ..Default::default()
    }));
    
    let containers = tokio::time::timeout(Duration::from_secs(3), containers_future)
        .await
        .map_err(|_| "Container list timeout".to_string())?
        .map_err(|e| e.to_string())?;
    
    // Collect running container IDs for parallel stats collection
    let running_containers: Vec<_> = containers.iter()
        .filter_map(|container| {
            container.id.as_ref().and_then(|id| {
                if get_container_state(container) == "running" {
                    Some(id.clone())
                } else {
                    None
                }
            })
        })
        .collect();
    
    // Collect stats in parallel with individual timeouts using spawn for true parallelism
    let stats_futures: Vec<_> = running_containers.iter()
        .map(|id| {
            let docker_clone = Arc::clone(docker);
            let id_clone = id.clone();
            tokio::spawn(async move {
                match get_container_resource_stats_optimized(&docker_clone, &id_clone).await {
                    Ok(stats) => Some((id_clone, stats)),
                    Err(_) => None,
                }
            })
        })
        .collect();
    
    // Wait for all stats with overall timeout
    let stats_timeout = tokio::time::timeout(Duration::from_secs(2), futures::future::join_all(stats_futures));
    let stats_results = match stats_timeout.await {
        Ok(results) => results,
        Err(_) => {
            // Timeout occurred, return containers without stats
            return build_container_info_list(containers, HashMap::new());
        }
    };
    
    // Build stats map from successful results
    let mut stats_map = HashMap::new();
    for result in stats_results {
        if let Ok(Some((id, stats))) = result {
            stats_map.insert(id, stats);
        }
    }
    
    build_container_info_list(containers, stats_map)
}

// MODIFIED: Optimized individual stats collection with timeout
async fn get_container_resource_stats_optimized(
    docker: &Docker, 
    container_id: &str
) -> Result<(f64, String, f64), String> {
    let options = Some(StatsOptions {
        stream: false,
        one_shot: true,
    });
    
    let mut stats_stream = docker.stats(container_id, options);
    
    // Individual timeout per container
    let stats_future = stats_stream.next();
    let stats = tokio::time::timeout(Duration::from_millis(800), stats_future)
        .await
        .map_err(|_| "Individual stats timeout".to_string())?
        .ok_or("No stats received".to_string())?
        .map_err(|e| e.to_string())?;
    
    // Calculate CPU percentage with single mutex operation
    let cpu_percent = {
        let current_cpu_total = stats.cpu_stats.cpu_usage.total_usage;
        let current_system_cpu = stats.cpu_stats.system_cpu_usage.unwrap_or(0);
        let current_time = Instant::now();
        let number_cpus = stats.cpu_stats.online_cpus.unwrap_or_else(|| {
            stats.cpu_stats.cpu_usage.percpu_usage.as_ref().map(|v| v.len() as u64).unwrap_or(1)
        }) as f64;
        
        // Single mutex lock for read and write
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
            0.0
        };
        
        // Update immediately while we have the lock
        previous_stats_map.insert(container_id.to_string(), PreviousStats {
            cpu_total: current_cpu_total,
            system_cpu: current_system_cpu,
            timestamp: current_time,
        });
        
        cpu_percent
    };
    
    // Calculate memory usage and percentage
    let (memory_usage_str, memory_percent) = {
        let memory_stats = &stats.memory_stats;
        let usage = memory_stats.usage.unwrap_or(0);
        let limit = memory_stats.limit.unwrap_or(1);
        
        let usage_mb = usage as f64 / 1024.0 / 1024.0;
        let limit_mb = limit as f64 / 1024.0 / 1024.0;
        let percent = if limit > 0 { (usage as f64 / limit as f64) * 100.0 } else { 0.0 };
        
        if usage > 0 && limit > 0 {
            (format!("{:.1} / {:.1}", usage_mb, limit_mb), percent)
        } else {
            ("N/A".to_string(), 0.0)
        }
    };
    
    Ok((cpu_percent, memory_usage_str, memory_percent))
}

// NEW: Helper function to build container info list
fn build_container_info_list(
    containers: Vec<ContainerSummary>, 
    stats_map: HashMap<String, (f64, String, f64)>
) -> Result<Vec<ContainerInfo>, String> {
    let mut container_infos = Vec::new();
    
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

// MODIFIED: Update container operation functions to use shared client
async fn start_container_optimized(container_id: &str, docker_client: &SharedDockerClient) -> Result<(), String> {
    docker_client.docker.start_container(container_id, None::<StartContainerOptions<String>>).await.map_err(|e| e.to_string())?;
    Ok(())
}

async fn stop_container_optimized(container_id: &str, docker_client: &SharedDockerClient) -> Result<(), String> {
    docker_client.docker.stop_container(container_id, None::<StopContainerOptions>).await.map_err(|e| e.to_string())?;
    Ok(())
}

async fn remove_container_optimized(container_id: &str, docker_client: &SharedDockerClient) -> Result<(), String> {
    let options = Some(RemoveContainerOptions {
        force: false,
        v: true,
        link: false,
    });
    docker_client.docker.remove_container(container_id, options).await.map_err(|e| e.to_string())?;
    Ok(())
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
        if event::poll(Duration::from_millis(32)).map_err(|e| e.to_string())? {
            if let Event::Key(key) = event::read().map_err(|e| e.to_string())? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.next();
                        terminal.draw(|f| ui(f, app)).map_err(|e| e.to_string())?;
                        continue;
                    },
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.previous();
                        terminal.draw(|f| ui(f, app)).map_err(|e| e.to_string())?;
                        continue;
                    },
                    KeyCode::Char('r') => {
                        app.force_refresh();
                    },
                    KeyCode::Char(' ') => {
                        app.toggle_auto_refresh();
                    },
                    KeyCode::Char('s') => {
                        if let Some(container) = app.containers.get(app.selected_index) {
                            let container_id = container.id.clone();
                            let container_state = container.state.clone();
                            tokio::spawn(async move {
                                // Note: For full optimization, you'd want to pass the shared client here too
                                // For now, keeping the original behavior for simplicity
                                if container_state == "running" {
                                    let _ = stop_container(&container_id).await;
                                } else {
                                    let _ = start_container(&container_id).await;
                                }
                            });
                        }
                    },
                    KeyCode::Char('x') => {
                        if let Some(container) = app.containers.get(app.selected_index) {
                            if container.state != "running" {
                                let container_id = container.id.clone();
                                tokio::spawn(async move {
                                    let _ = remove_container(&container_id).await;
                                });
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        terminal.draw(|f| ui(f, app)).map_err(|e| e.to_string())?;
    }
}

// Keep all the existing UI and helper functions unchanged
fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(6),
            Constraint::Length(3),
        ])
        .split(f.size());

    let selected_style = Style::default().add_modifier(Modifier::REVERSED);
    let header_cells = ["ID", "Name", "Status", "CPU %", "Memory (MB)", "Mem %", "Image"]
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
            Constraint::Length(12),
            Constraint::Fill(3),
            Constraint::Fill(2),
            Constraint::Length(8),
            Constraint::Length(20),
            Constraint::Length(8),
            Constraint::Fill(2),
        ]
    )
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("Containers"))
        .highlight_style(selected_style)
        .highlight_symbol(">> ");

    let mut state = TableState::default();
    state.select(Some(app.selected_index));
    f.render_stateful_widget(table, chunks[0], &mut state);

    if let Some(container) = app.containers.get(app.selected_index) {
        let info_text = vec![
            Line::from(vec![Span::styled("Full ID: ", Style::default().fg(Color::Yellow)), Span::raw(&container.id)]),
            Line::from(vec![Span::styled("Full Image: ", Style::default().fg(Color::Yellow)), Span::raw(&container.image)]),
            Line::from(vec![Span::styled("Ports: ", Style::default().fg(Color::Yellow)), Span::raw(&container.ports)]),
        ];

        let info_paragraph = Paragraph::new(info_text)
            .block(Block::default().borders(Borders::ALL).title("Details"));
        f.render_widget(info_paragraph, chunks[1]);
    }

    let last_update_time = chrono::DateTime::<chrono::Utc>::from(
        std::time::SystemTime::now() - app.last_update.elapsed()
    );
    let help_text = format!(
        "Last Update: {} | Auto-refresh: {} || q/ESC: Quit | ↑↓/jk: Navigate | r: Refresh | Space: Toggle auto-refresh | s: Start/Stop | x: Remove stopped",
        last_update_time.format("%H:%M:%S"),
        if app.auto_refresh {
            "ON "
        } else {
            "OFF"
        }
    );

    let help = Paragraph::new(help_text)
        .style(Style::default().fg(Color::Cyan))
        .block(Block::default().borders(Borders::ALL));

    f.render_widget(help, chunks[2]);

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

// Keep original functions for backward compatibility (used in container operations)
async fn get_container_stats() -> Result<Vec<ContainerInfo>, String> {
    let docker_client = SharedDockerClient::new().await?;
    get_container_stats_optimized(&docker_client).await
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
    get_container_resource_stats_optimized(docker, container_id).await
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

async fn remove_container(container_id: &str) -> Result<(), String> {
    let docker = Docker::connect_with_socket_defaults().map_err(|e| e.to_string())?;
    let options = Some(RemoveContainerOptions {
        force: false,
        v: true,
        link: false,
    });
    docker.remove_container(container_id, options).await.map_err(|e| e.to_string())?;
    Ok(())
}
