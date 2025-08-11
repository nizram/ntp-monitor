use eframe::egui;
use chrono::{DateTime, TimeZone, Utc, Local, FixedOffset};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH, Instant};
use tokio::net::UdpSocket;
use tokio::time::timeout;

const CONFIG_FILE: &str = "ntp_servers.ini";

#[derive(Clone)]
struct NtpServerInfo {
    name: String,
    ip: String,
    current_time: Option<SystemTime>,
    time_diff: Option<Duration>,
    stratum: Option<u8>,
    drift: Option<f64>,
    last_updated: Option<SystemTime>,
    error: Option<String>,
}

impl Default for NtpServerInfo {
    fn default() -> Self {
        Self {
            name: String::new(),
            ip: String::new(),
            current_time: None,
            time_diff: None,
            stratum: None,
            drift: None,
            last_updated: None,
            error: None,
        }
    }
}

#[derive(Clone)]
struct Settings {
    update_frequency_seconds: u64,
    timezone: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            update_frequency_seconds: 10,
            timezone: "UTC".to_string(),
        }
    }
}

#[derive(PartialEq)]
enum AppState {
    ServerInput,
    Monitoring,
    Settings,
}

struct NtpMonitorApp {
    state: AppState,
    server_input: String,
    servers: Arc<Mutex<Vec<NtpServerInfo>>>,
    runtime: Option<tokio::runtime::Runtime>,
    settings: Settings,
    temp_settings: Settings, // For editing in settings dialog
    last_update_time: Arc<Mutex<Option<Instant>>>,
    monitoring_handle: Option<std::thread::JoinHandle<()>>,
    stop_monitoring: Arc<Mutex<bool>>,
}

impl Default for NtpMonitorApp {
    fn default() -> Self {
        let mut app = Self {
            state: AppState::ServerInput,
            server_input: String::new(),
            servers: Arc::new(Mutex::new(Vec::new())),
            runtime: Some(tokio::runtime::Runtime::new().unwrap()),
            settings: Settings::default(),
            temp_settings: Settings::default(),
            last_update_time: Arc::new(Mutex::new(None)),
            monitoring_handle: None,
            stop_monitoring: Arc::new(Mutex::new(false)),
        };
        
        // Load servers from INI file on startup
        app.load_servers_from_ini();
        app
    }
}

impl eframe::App for NtpMonitorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        match self.state {
            AppState::ServerInput => {
                self.show_server_input_dialog(ctx);
            }
            AppState::Monitoring => {
                self.show_main_window(ctx);
            }
            AppState::Settings => {
                self.show_settings_dialog(ctx);
            }
        }
        
        // Request repaint every second to update times and countdown
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Save servers to INI file on exit
        self.save_servers_to_ini();
        // Stop monitoring thread
        self.stop_monitoring();
    }
}

impl NtpMonitorApp {
    fn show_server_input_dialog(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(50.0);
                ui.heading("NTP Server Monitor");
                ui.add_space(20.0);
                
                // Show if servers were loaded from INI
                let servers = self.servers.lock().unwrap();
                if !servers.is_empty() {
                    ui.label(format!("Loaded {} servers from {}", servers.len(), CONFIG_FILE));
                    ui.add_space(10.0);
                    
                    if ui.button("Use Loaded Servers").clicked() {
                        drop(servers);
                        self.state = AppState::Monitoring;
                        self.start_monitoring_task();
                        return;
                    }
                    ui.add_space(10.0);
                    ui.label("Or add new servers below:");
                }
                drop(servers);
                
                ui.label("Enter NTP servers (one per line, name or IP address):");
                ui.add_space(10.0);
                
                ui.add_sized(
                    [400.0, 200.0],
                    egui::TextEdit::multiline(&mut self.server_input)
                        .hint_text("Example:\ntime.google.com\npool.ntp.org\n129.6.15.28"),
                );
                
                ui.add_space(20.0);
                
                if ui.button("Start Monitoring").clicked() && !self.server_input.trim().is_empty() {
                    self.parse_and_start_monitoring();
                }
            });
        });
    }

    fn show_main_window(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("NTP Server Monitor");
                ui.add_space(20.0);
                
                if ui.button("Add More Servers").clicked() {
                    self.stop_monitoring();
                    self.state = AppState::ServerInput;
                    self.server_input.clear();
                    return;
                }
                
                if ui.button("Settings").clicked() {
                    self.temp_settings = self.settings.clone();
                    self.state = AppState::Settings;
                }
                
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.show_countdown(ui);
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(10.0);
            
            egui::ScrollArea::vertical().show(ui, |ui| {
                let servers = self.servers.lock().unwrap();
                
                // Table header
                egui::Grid::new("ntp_table")
                    .striped(true)
                    .min_col_width(120.0)
                    .show(ui, |ui| {
                        ui.strong("Server Name");
                        ui.strong("IP Address");
                        ui.strong("Current Time");
                        ui.strong("Time Diff (ms)");
                        ui.strong("Stratum");
                        ui.strong("Drift (ppm)");
                        ui.strong("Status");
                        ui.end_row();
                        
                        for server in servers.iter() {
                            ui.label(&server.name);
                            ui.label(&server.ip);
                            
                            // Current time
                            if let Some(time) = server.current_time {
                                let datetime = format_system_time_with_timezone(time, &self.settings.timezone);
                                ui.label(datetime);
                            } else {
                                ui.label("N/A");
                            }
                            
                            // Time difference
                            if let Some(diff) = server.time_diff {
                                let diff_ms = diff.as_millis() as i64;
                                let color = if diff_ms.abs() > 100 {
                                    egui::Color32::RED
                                } else if diff_ms.abs() > 50 {
                                    egui::Color32::YELLOW
                                } else {
                                    egui::Color32::GREEN
                                };
                                ui.colored_label(color, format!("{:+}", diff_ms));
                            } else {
                                ui.label("N/A");
                            }
                            
                            // Stratum
                            if let Some(stratum) = server.stratum {
                                ui.label(stratum.to_string());
                            } else {
                                ui.label("N/A");
                            }
                            
                            // Drift
                            if let Some(drift) = server.drift {
                                ui.label(format!("{:.2}", drift));
                            } else {
                                ui.label("N/A");
                            }
                            
                            // Status
                            if let Some(error) = &server.error {
                                ui.colored_label(egui::Color32::RED, error);
                            } else if server.last_updated.is_some() {
                                ui.colored_label(egui::Color32::GREEN, "Online");
                            } else {
                                ui.label("Connecting...");
                            }
                            
                            ui.end_row();
                        }
                    });
            });
        });
    }

    fn show_settings_dialog(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(50.0);
                ui.heading("Settings");
                ui.add_space(30.0);
                
                ui.horizontal(|ui| {
                    ui.label("Update frequency (seconds):");
                    ui.add(egui::Slider::new(&mut self.temp_settings.update_frequency_seconds, 5..=300)
                        .text("seconds"));
                });
                
                ui.add_space(20.0);
                ui.label(format!("Current: Every {} seconds", self.temp_settings.update_frequency_seconds));
                
                ui.add_space(30.0);
                
                ui.horizontal(|ui| {
                    ui.label("Timezone:");
                    egui::ComboBox::from_label("")
                        .selected_text(&self.temp_settings.timezone)
                        .show_ui(ui, |ui| {
                            let timezones = get_common_timezones();
                            for tz in timezones {
                                ui.selectable_value(&mut self.temp_settings.timezone, tz.clone(), tz);
                            }
                        });
                });
                
                ui.add_space(20.0);
                ui.label(format!("Selected: {}", self.temp_settings.timezone));
                
                ui.add_space(40.0);
                
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        self.settings = self.temp_settings.clone();
                        self.restart_monitoring_with_new_settings();
                        self.state = AppState::Monitoring;
                    }
                    
                    if ui.button("Cancel").clicked() {
                        self.temp_settings = self.settings.clone();
                        self.state = AppState::Monitoring;
                    }
                });
            });
        });
    }

    fn show_countdown(&self, ui: &mut egui::Ui) {
        // Show current time in selected timezone
        let now = SystemTime::now();
        let current_time = format_system_time_with_timezone(now, &self.settings.timezone);
        ui.label(format!("Current time ({}): {}", self.settings.timezone, current_time));
        
        ui.add_space(10.0);
        
        // Show countdown
        if let Some(last_update) = *self.last_update_time.lock().unwrap() {
            let elapsed = last_update.elapsed().as_secs();
            let remaining = self.settings.update_frequency_seconds.saturating_sub(elapsed);
            
            if remaining > 0 {
                ui.label(format!("Next update in: {}s", remaining));
            } else {
                ui.label("Updating...");
            }
        } else {
            ui.label("Initializing...");
        }
    }

    fn parse_and_start_monitoring(&mut self) {
        let server_lines: Vec<String> = self.server_input
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let mut servers = Vec::new();
        for line in server_lines {
            servers.push(NtpServerInfo {
                name: line.clone(),
                ip: resolve_hostname(&line).unwrap_or_else(|| line.clone()),
                ..Default::default()
            });
        }

        *self.servers.lock().unwrap() = servers;
        self.state = AppState::Monitoring;
        self.start_monitoring_task();
    }

    fn stop_monitoring(&mut self) {
        *self.stop_monitoring.lock().unwrap() = true;
        if let Some(handle) = self.monitoring_handle.take() {
            let _ = handle.join();
        }
        *self.stop_monitoring.lock().unwrap() = false;
    }

    fn restart_monitoring_with_new_settings(&mut self) {
        self.stop_monitoring();
        self.start_monitoring_task();
    }

    fn start_monitoring_task(&mut self) {
        let servers_clone = Arc::clone(&self.servers);
        let last_update_clone = Arc::clone(&self.last_update_time);
        let stop_monitoring_clone = Arc::clone(&self.stop_monitoring);
        let settings = self.settings.clone();
        let rt = self.runtime.as_ref().unwrap().handle().clone();
        
        let handle = thread::spawn(move || {
            rt.block_on(async {
                loop {
                    // Check if we should stop
                    if *stop_monitoring_clone.lock().unwrap() {
                        break;
                    }

                    let servers_to_check = {
                        let servers = servers_clone.lock().unwrap();
                        servers.clone()
                    };
                    
                    let mut updated_servers = Vec::new();
                    
                    for mut server in servers_to_check {
                        // Check stop flag again in case it was set during processing
                        if *stop_monitoring_clone.lock().unwrap() {
                            return;
                        }

                        match query_ntp_server(&server.ip).await {
                            Ok((time, stratum)) => {
                                let local_time = SystemTime::now();
                                let time_diff = if time > local_time {
                                    time.duration_since(local_time).unwrap()
                                } else {
                                    local_time.duration_since(time).unwrap()
                                };
                                
                                // Simple drift calculation (change in time difference over time)
                                let drift = if let (Some(last_time), Some(last_diff)) = 
                                    (server.last_updated, server.time_diff) {
                                    if let Ok(time_elapsed) = local_time.duration_since(last_time) {
                                        let time_elapsed_secs = time_elapsed.as_secs_f64();
                                        let current_diff_secs = time_diff.as_secs_f64();
                                        let last_diff_secs = last_diff.as_secs_f64();
                                        let diff_change = current_diff_secs - last_diff_secs;
                                        if time_elapsed_secs > 0.0 {
                                            Some((diff_change / time_elapsed_secs) * 1_000_000.0) // ppm
                                        } else {
                                            server.drift
                                        }
                                    } else {
                                        server.drift
                                    }
                                } else {
                                    None
                                };
                                
                                server.current_time = Some(time);
                                server.time_diff = Some(time_diff);
                                server.stratum = Some(stratum);
                                server.drift = drift;
                                server.last_updated = Some(local_time);
                                server.error = None;
                            }
                            Err(e) => {
                                server.error = Some(e);
                                server.current_time = None;
                                server.time_diff = None;
                                server.stratum = None;
                            }
                        }
                        updated_servers.push(server);
                    }
                    
                    *servers_clone.lock().unwrap() = updated_servers;
                    *last_update_clone.lock().unwrap() = Some(Instant::now());
                    
                    // Sleep for the configured duration, but check stop flag periodically
                    let sleep_duration = Duration::from_secs(settings.update_frequency_seconds);
                    let check_interval = Duration::from_millis(500);
                    let mut remaining = sleep_duration;
                    
                    while remaining > Duration::ZERO {
                        if *stop_monitoring_clone.lock().unwrap() {
                            return;
                        }
                        
                        let sleep_time = std::cmp::min(remaining, check_interval);
                        tokio::time::sleep(sleep_time).await;
                        remaining = remaining.saturating_sub(sleep_time);
                    }
                }
            });
        });
        
        self.monitoring_handle = Some(handle);
    }

    fn load_servers_from_ini(&mut self) {
        if let Ok(contents) = std::fs::read_to_string(CONFIG_FILE) {
            let mut servers = Vec::new();
            let mut current_section = "";
            
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                    continue;
                }
                
                if line.starts_with('[') && line.ends_with(']') {
                    current_section = &line[1..line.len()-1];
                    continue;
                }
                
                if let Some((key, value)) = line.split_once('=') {
                    let key = key.trim();
                    let value = value.trim();
                    
                    if current_section == "servers" && key.starts_with("server") {
                        servers.push(NtpServerInfo {
                            name: value.to_string(),
                            ip: resolve_hostname(value).unwrap_or_else(|| value.to_string()),
                            ..Default::default()
                        });
                    } else if current_section == "settings" {
                        if key == "update_frequency_seconds" {
                            if let Ok(freq_val) = value.parse::<u64>() {
                                self.settings.update_frequency_seconds = freq_val.clamp(5, 300);
                            }
                        } else if key == "timezone" {
                            self.settings.timezone = value.to_string();
                        }
                    }
                }
            }
            
            if !servers.is_empty() {
                *self.servers.lock().unwrap() = servers;
            }
            
            // Update temp_settings after loading
            self.temp_settings = self.settings.clone();
        }
    }

    fn save_servers_to_ini(&self) {
        let servers = self.servers.lock().unwrap();
        if servers.is_empty() {
            return;
        }

        let mut content = String::new();
        
        // Save servers
        content.push_str("[servers]\n");
        for (i, server) in servers.iter().enumerate() {
            content.push_str(&format!("server{} = {}\n", i + 1, server.name));
        }
        
        // Save settings
        content.push_str("\n[settings]\n");
        content.push_str(&format!("update_frequency_seconds = {}\n", self.settings.update_frequency_seconds));
        content.push_str(&format!("timezone = {}\n", self.settings.timezone));
        
        if let Err(e) = std::fs::write(CONFIG_FILE, content) {
            eprintln!("Failed to save servers to {}: {}", CONFIG_FILE, e);
        }
    }
}

async fn query_ntp_server(server: &str) -> Result<(SystemTime, u8), String> {
    let socket = UdpSocket::bind("0.0.0.0:0").await
        .map_err(|e| format!("Failed to bind socket: {}", e))?;
    
    let addr = format!("{}:123", server)
        .parse::<SocketAddr>()
        .map_err(|_| "Invalid server address".to_string())?;
    
    // NTP packet structure (simplified)
    let mut packet = [0u8; 48];
    packet[0] = 0x1B; // LI=0, VN=3, Mode=3
    
    timeout(Duration::from_secs(5), socket.send_to(&packet, addr)).await
        .map_err(|_| "Request timeout".to_string())?
        .map_err(|e| format!("Send error: {}", e))?;
    
    let mut response = [0u8; 48];
    timeout(Duration::from_secs(5), socket.recv_from(&mut response)).await
        .map_err(|_| "Response timeout".to_string())?
        .map_err(|e| format!("Receive error: {}", e))?;
    
    // Extract transmit timestamp (bytes 40-47)
    let transmit_timestamp = u64::from_be_bytes([
        response[40], response[41], response[42], response[43],
        response[44], response[45], response[46], response[47],
    ]);
    
    // Convert NTP timestamp to SystemTime
    let ntp_epoch_offset = 2208988800u64; // Seconds between 1900 and 1970
    let unix_timestamp = transmit_timestamp >> 32; // Get seconds part
    let unix_timestamp = unix_timestamp.saturating_sub(ntp_epoch_offset);
    
    let system_time = UNIX_EPOCH + Duration::from_secs(unix_timestamp);
    
    // Extract stratum (byte 1)
    let stratum = response[1];
    
    Ok((system_time, stratum))
}

fn resolve_hostname(hostname: &str) -> Option<String> {
    if hostname.parse::<IpAddr>().is_ok() {
        return Some(hostname.to_string());
    }
    
    format!("{}:123", hostname)
        .to_socket_addrs()
        .ok()?
        .next()
        .map(|addr| addr.ip().to_string())
}

fn format_system_time(time: SystemTime) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let secs = duration.as_secs();
            let hours = (secs / 3600) % 24;
            let minutes = (secs % 3600) / 60;
            let seconds = secs % 60;
            format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
        }
        Err(_) => "Invalid".to_string(),
    }
}

fn format_system_time_with_timezone(time: SystemTime, timezone: &str) -> String {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let timestamp = duration.as_secs() as i64;
            let datetime = DateTime::<Utc>::from_timestamp(timestamp, 0);
            
            if let Some(dt) = datetime {
                match timezone {
                    "UTC" => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                    "Local" => {
                        let local_dt = dt.with_timezone(&Local);
                        local_dt.format("%Y-%m-%d %H:%M:%S %Z").to_string()
                    },
                    "EST" => {
                        let est = FixedOffset::west_opt(5 * 3600).unwrap();
                        let est_dt = dt.with_timezone(&est);
                        est_dt.format("%Y-%m-%d %H:%M:%S EST").to_string()
                    },
                    "PST" => {
                        let pst = FixedOffset::west_opt(8 * 3600).unwrap();
                        let pst_dt = dt.with_timezone(&pst);
                        pst_dt.format("%Y-%m-%d %H:%M:%S PST").to_string()
                    },
                    "MST" => {
                        let mst = FixedOffset::west_opt(7 * 3600).unwrap();
                        let mst_dt = dt.with_timezone(&mst);
                        mst_dt.format("%Y-%m-%d %H:%M:%S MST").to_string()
                    },
                    "CST" => {
                        let cst = FixedOffset::west_opt(6 * 3600).unwrap();
                        let cst_dt = dt.with_timezone(&cst);
                        cst_dt.format("%Y-%m-%d %H:%M:%S CST").to_string()
                    },
                    "GMT" => dt.format("%Y-%m-%d %H:%M:%S GMT").to_string(),
                    "CET" => {
                        let cet = FixedOffset::east_opt(1 * 3600).unwrap();
                        let cet_dt = dt.with_timezone(&cet);
                        cet_dt.format("%Y-%m-%d %H:%M:%S CET").to_string()
                    },
                    "JST" => {
                        let jst = FixedOffset::east_opt(9 * 3600).unwrap();
                        let jst_dt = dt.with_timezone(&jst);
                        jst_dt.format("%Y-%m-%d %H:%M:%S JST").to_string()
                    },
                    _ => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                }
            } else {
                "Invalid".to_string()
            }
        }
        Err(_) => "Invalid".to_string(),
    }
}

fn get_common_timezones() -> Vec<String> {
    vec![
        "UTC".to_string(),
        "Local".to_string(),
        "GMT".to_string(),
        "EST".to_string(),
        "CST".to_string(),
        "MST".to_string(),
        "PST".to_string(),
        "CET".to_string(),
        "JST".to_string(),
    ]
}

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 600.0])
            .with_title("NTP Server Monitor"),
        // Let eframe choose the best available renderer
        ..Default::default()
    };
    
    eframe::run_native(
        "NTP Server Monitor",
        options,
        Box::new(|_cc| Box::new(NtpMonitorApp::default())),
    )
}