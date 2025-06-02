// Cache container list to avoid repeated Docker API calls for basic info
#[derive(Debug, Clone)]
struct ContainerCache {
    containers: Vec<ContainerSummary>,
    last_updated: Instant,
    cache_duration: Duration,
}

impl ContainerCache {
    fn new() -> Self {
        Self {
            containers: Vec::new(),
            last_updated: Instant::now() - Duration::from_secs(60), // Force initial fetch
            cache_duration: Duration::from_secs(5), // Cache container list for 5 seconds
        }
    }
    
    fn is_expired(&self) -> bool {
        self.last_updated.elapsed() > self.cache_duration
    }
    
    fn update(&mut self, containers: Vec<ContainerSummary>) {
        self.containers = containers;
        self.last_updated = Instant::now();
    }
}

// Skip stats collection for containers that haven't changed state
static CONTAINER_STATE_CACHE: std::sync::LazyLock<Arc<Mutex<HashMap<String, (String, Instant)>>>> = 
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));

async fn get_container_stats_with_caching(docker_client: &SharedDockerClient) -> Result<Vec<ContainerInfo>, String> {
    static CONTAINER_CACHE: std::sync::LazyLock<Arc<Mutex<ContainerCache>>> = 
        std::sync::LazyLock::new(|| Arc::new(Mutex::new(ContainerCache::new())));
    
    let docker = &docker_client.docker;
    
    // Check if we need to refresh container list
    let containers = {
        let mut cache = CONTAINER_CACHE.lock().unwrap();
        if cache.is_expired() {
            // Fetch new container list
            let containers_future = docker.list_containers(Some(ListContainersOptions::<String> {
                all: true,
                ..Default::default()
            }));
            
            let new_containers = tokio::time::timeout(Duration::from_secs(2), containers_future)
                .await
                .map_err(|_| "Container list timeout".to_string())?
                .map_err(|e| e.to_string())?;
            
            cache.update(new_containers.clone());
            new_containers
        } else {
            // Use cached container list
            cache.containers.clone()
        }
    };
    
    // Only collect stats for containers that are running and haven't been checked recently
    let mut containers_needing_stats = Vec::new();
    let mut state_cache = CONTAINER_STATE_CACHE.lock().unwrap();
    
    for container in &containers {
        if let Some(id) = &container.id {
            let current_state = get_container_state(container);
            let needs_stats = if current_state == "running" {
                // Check if we've collected stats recently for this container
                match state_cache.get(id) {
                    Some((cached_state, last_check)) if cached_state == "running" => {
                        // Only refresh stats every 2 seconds for running containers
                        last_check.elapsed() > Duration::from_secs(2)
                    }
                    _ => true, // New container or state changed
                }
            } else {
                false // Don't collect stats for stopped containers
            };
            
            if needs_stats {
                containers_needing_stats.push(id.clone());
            }
            
            // Update state cache
            state_cache.insert(id.clone(), (current_state, Instant::now()));
        }
    }
    
    drop(state_cache); // Release lock early
    
    // Collect stats only for containers that need it
    let stats_futures: Vec<_> = containers_needing_stats.iter()
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
    
    // Wait for stats collection with timeout
    let stats_results = tokio::time::timeout(Duration::from_millis(1500), futures::future::join_all(stats_futures))
        .await
        .unwrap_or_else(|_| Vec::new()); // Return empty vec on timeout
    
    // Use cached stats for containers we didn't refresh
    static STATS_CACHE: std::sync::LazyLock<Arc<Mutex<HashMap<String, (f64, String, f64, Instant)>>>> = 
        std::sync::LazyLock::new(|| Arc::new(Mutex::new(HashMap::new())));
    
    let mut stats_map = HashMap::new();
    let mut stats_cache = STATS_CACHE.lock().unwrap();
    
    // Add fresh stats
    for result in stats_results {
        if let Ok(Some((id, stats))) = result {
            stats_cache.insert(id.clone(), (stats.0, stats.1.clone(), stats.2, Instant::now()));
            stats_map.insert(id, stats);
        }
    }
    
    // Use cached stats for containers we didn't update (if cache is recent)
    for container in &containers {
        if let Some(id) = &container.id {
            if !stats_map.contains_key(id) {
                if let Some((cpu, mem_usage, mem_percent, timestamp)) = stats_cache.get(id) {
                    if timestamp.elapsed() < Duration::from_secs(5) { // Use cache if less than 5 seconds old
                        stats_map.insert(id.clone(), (*cpu, mem_usage.clone(), *mem_percent));
                    }
                }
            }
        }
    }
    
    build_container_info_list(containers, stats_map)
}
