use std::sync::Arc;

// Shared Docker client - create once, reuse everywhere
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
    
    fn clone(&self) -> Self {
        Self {
            docker: Arc::clone(&self.docker),
        }
    }
}

// Optimized stats collection with shared client and bulk mutex operations
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
    
    // Separate running containers and prepare parallel stats collection
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
    
    // Collect stats in parallel with individual timeouts
    let stats_futures: Vec<_> = running_containers.iter()
        .map(|id| {
            let docker_clone = Arc::clone(docker);
            let id_clone = id.clone();
            tokio::spawn(async move {
                match get_container_resource_stats_fast(&docker_clone, &id_clone).await {
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

// Optimized individual stats collection
async fn get_container_resource_stats_fast(
    docker: &Docker, 
    container_id: &str
) -> Result<(f64, String, f64), String> {
    // Use a shorter timeout for individual requests
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
    
    // Bulk mutex operation - get all previous stats at once
    let (cpu_percent, memory_usage_str, memory_percent) = {
        let current_cpu_total = stats.cpu_stats.cpu_usage.total_usage;
        let current_system_cpu = stats.cpu_stats.system_cpu_usage.unwrap_or(0);
        let current_time = Instant::now();
        let number_cpus = stats.cpu_stats.online_cpus.unwrap_or_else(|| {
            stats.cpu_stats.cpu_usage.percpu_usage.as_ref().map(|v| v.len() as u64).unwrap_or(1)
        }) as f64;
        
        // Single mutex lock for read and write
        let cpu_percent = {
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
        
        // Calculate memory usage
        let memory_stats = &stats.memory_stats;
        let usage = memory_stats.usage.unwrap_or(0);
        let limit = memory_stats.limit.unwrap_or(1);
        
        let usage_mb = usage as f64 / 1024.0 / 1024.0;
        let limit_mb = limit as f64 / 1024.0 / 1024.0;
        let percent = if limit > 0 { (usage as f64 / limit as f64) * 100.0 } else { 0.0 };
        
        let memory_usage_str = if usage > 0 && limit > 0 {
            format!("{:.1} / {:.1}", usage_mb, limit_mb)
        } else {
            "N/A".to_string()
        };
        
        (cpu_percent, memory_usage_str, percent)
    };
    
    Ok((cpu_percent, memory_usage_str, memory_percent))
}

// Helper function to build container info list
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

// Modified background task to use shared client
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
