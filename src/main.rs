use anyhow::Result;
use chrono::{DateTime, Local};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use local_ip_address::local_ip;
use ratatui::{
    layout::Margin,
    prelude::*,
    widgets::{Block, Borders, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use regex::Regex;
use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind,RefreshKind, System};

// --- Data Structures ---

struct Puzzle {
    name: String,
    ip: String,
    #[allow(dead_code)]
    last_seen: DateTime<Local>,
}

struct App {
    // System Stats
    cpu_usage: f32,
    ram_usage: u64,
    total_ram: u64,
    uptime: u64,
    ip_address: String,
    hostname: String,

    // App Data
    logs: Vec<String>,
    puzzles: HashMap<String, Puzzle>,
    clients: HashSet<String>,
    
    // Status
    server_ready: bool,

    // UI State
    scroll_position: usize,
    should_quit: bool,
}

impl App {
    fn new() -> Self {
        let ip = local_ip().map(|ip| ip.to_string()).unwrap_or_else(|_| "Unknown".to_string());
        let hostname = System::host_name().unwrap_or_else(|| "Unknown".to_string());

        Self {
            cpu_usage: 0.0,
            ram_usage: 0,
            total_ram: 0,
            uptime: 0,
            ip_address: ip,
            hostname,
            logs: Vec::new(),
            puzzles: HashMap::new(),
            clients: HashSet::new(),
            server_ready: false, 
            scroll_position: 0,
            should_quit: false,
        }
    }

    // Unified function to handle logs from both stdout and stderr
    fn process_log(&mut self, raw_line: String, is_stderr: bool) {
        // 1. Check Regexes (on the raw line)
        
        // Status check
        if raw_line.contains("Serving at port 8080") {
            self.server_ready = true;
        }

        // Puzzle Dict: {'name': 'patchpanel', ... 'ip': '127.0.0.1'}
        // Re-compiled here for simplicity, or could be static/lazy_static
        let puzzle_dict_regex = Regex::new(r"\{'name':\s*'([^']+)',.*?'ip':\s*'([^']+)'").unwrap();
        
        // Puzzle Registration fallback
        let puzzle_reg_regex = Regex::new(r"Registering new puzzle\s+(\w+)").unwrap();
        
        // HTTP Client: 172.25.208.1 - - [Date]
        let http_client_regex = Regex::new(r"^(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})\s+-\s+-").unwrap();
        
        // UDP Client
        let msg_client_regex = Regex::new(r"Received message from \('(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})',").unwrap();

        // -- Parsing Logic --
        if let Some(caps) = puzzle_dict_regex.captures(&raw_line) {
            let name = caps.get(1).map_or("?", |m| m.as_str()).to_string();
            let ip = caps.get(2).map_or("?", |m| m.as_str()).to_string();
            self.puzzles.insert(name.clone(), Puzzle { name, ip, last_seen: Local::now() });
        } else if let Some(caps) = puzzle_reg_regex.captures(&raw_line) {
            let name = caps.get(1).map_or("?", |m| m.as_str()).to_string();
            self.puzzles.entry(name.clone()).or_insert(Puzzle {
                name,
                ip: "Waiting...".to_string(),
                last_seen: Local::now()
            });
        }

        if let Some(caps) = http_client_regex.captures(&raw_line) {
            if let Some(ip) = caps.get(1) {
                self.clients.insert(ip.as_str().to_string());
            }
        } else if let Some(caps) = msg_client_regex.captures(&raw_line) {
            if let Some(ip) = caps.get(1) {
                self.clients.insert(ip.as_str().to_string());
            }
        }

        // 2. Store Log (Add prefix if stderr)
        let display_line = if is_stderr {
            format!("[STDERR] {}", raw_line)
        } else {
            raw_line
        };
        
        self.logs.push(display_line);

        // 3. Auto-Scroll
        if self.logs.len() > 10 {
            self.scroll_position = self.logs.len() - 10;
        } else {
            self.scroll_position = 0;
        }
    }

    fn scroll_up(&mut self) {
        if self.scroll_position > 0 {
            self.scroll_position -= 1;
        }
    }

    fn scroll_down(&mut self) {
        let max_scroll = self.logs.len().saturating_sub(10);
        if self.scroll_position < max_scroll {
            self.scroll_position += 1;
        }
    }

    fn scroll_page_up(&mut self) {
        self.scroll_position = self.scroll_position.saturating_sub(10);
    }

    fn scroll_page_down(&mut self) {
        let max_scroll = self.logs.len().saturating_sub(10);
        self.scroll_position = (self.scroll_position + 10).min(max_scroll);
    }

    fn scroll_to_top(&mut self) {
        self.scroll_position = 0;
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll_position = self.logs.len().saturating_sub(10);
    }
}

fn main() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let app = Arc::new(Mutex::new(App::new()));
    let app_clone = app.clone();
    let child_process: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let child_process_clone = child_process.clone();

    thread::spawn(move || {
        // IMPORTANT: "-u" forces unbuffered output so we see logs immediately
        let mut child = Command::new("python3")
            .arg("-u") 
            .arg("server.py")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to start python script");

        let stdout = child.stdout.take().expect("Failed to capture stdout");
        let stderr = child.stderr.take().expect("Failed to capture stderr");

        *child_process_clone.lock().unwrap() = Some(child);

        let reader = BufReader::new(stdout);
        let stderr_reader = BufReader::new(stderr);

        // Spawn Stderr Thread
        let app_stderr = app_clone.clone();
        thread::spawn(move || {
            for line in stderr_reader.lines() {
                if let Ok(l) = line {
                    let mut app = app_stderr.lock().unwrap();
                    app.process_log(l, true); // Process as stderr
                }
            }
        });

        // Main Stdout Loop
        for line in reader.lines() {
            if let Ok(l) = line {
                let mut app = app_clone.lock().unwrap();
                app.process_log(l, false); // Process as stdout
            }
        }
    });

    // UI Loop
    let mut sys = System::new_with_specifics(
        RefreshKind::nothing().with_cpu(CpuRefreshKind::everything()).with_memory(MemoryRefreshKind::everything()),
    );

    loop {
        sys.refresh_cpu_all();
        sys.refresh_memory();
        
        {
            let mut app = app.lock().unwrap();
            app.cpu_usage = sys.global_cpu_usage();
            app.ram_usage = sys.used_memory() / 1024 / 1024;
            app.total_ram = sys.total_memory() / 1024 / 1024;
            app.uptime = System::uptime();
            
            if app.should_quit {
                break;
            }
        }

        terminal.draw(|f| ui(f, &app.lock().unwrap()))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                let mut app = app.lock().unwrap();
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.should_quit = true
                    }
                    KeyCode::Up => app.scroll_up(),
                    KeyCode::Down => app.scroll_down(),
                    KeyCode::PageUp => app.scroll_page_up(),
                    KeyCode::PageDown => app.scroll_page_down(),
                    KeyCode::Home => app.scroll_to_top(),
                    KeyCode::End => app.scroll_to_bottom(),
                    _ => {}
                }
            }
        }
    }

    // Cleanup
    {
        let mut child_opt = child_process.lock().unwrap();
        if let Some(mut child) = child_opt.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

fn ui(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),  // Header
            Constraint::Min(10),    // Main
            Constraint::Length(12), // Logs
        ])
        .split(f.area());

    // Header
    let status_color = if app.server_ready { Color::Green } else { Color::Yellow };
    let status_text = if app.server_ready { "ONLINE" } else { "STARTING" };

    let uptime_str = format!("{}s", app.uptime);
    let info_text = format!(
        " Host: {} | IP: {} | Uptime: {} | CPU: {:.1}% | RAM: {}/{} MB | Status: {} ",
        app.hostname, app.ip_address, uptime_str, app.cpu_usage, app.ram_usage, app.total_ram, status_text
    );
    
    let header = Paragraph::new(info_text)
        .block(Block::default().borders(Borders::ALL).title(" System Monitor "))
        .style(Style::default().fg(Color::Black).bg(status_color).add_modifier(Modifier::BOLD))
        .alignment(Alignment::Center);
    f.render_widget(header, chunks[0]);

    // Main Content
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    // Puzzles
    let puzzle_items: Vec<ListItem> = app.puzzles.values()
        .map(|p| {
            ListItem::new(format!("🧩 {} ({})", p.name, p.ip))
                .style(Style::default().fg(Color::Green))
        })
        .collect();
    
    let puzzle_list = List::new(puzzle_items)
        .block(Block::default().borders(Borders::ALL).title(format!(" Active Puzzles ({}) ", app.puzzles.len())));
    f.render_widget(puzzle_list, main_chunks[0]);

    // Clients
    let client_items: Vec<ListItem> = app.clients.iter()
        .map(|ip| ListItem::new(format!("💻 Client: {}", ip)).style(Style::default().fg(Color::Blue)))
        .collect();

    let client_list = List::new(client_items)
        .block(Block::default().borders(Borders::ALL).title(format!(" Connected Clients ({}) ", app.clients.len())));
    f.render_widget(client_list, main_chunks[1]);

    // Logs
    let log_window_height = chunks[2].height as usize - 2;
    let logs_to_show: Vec<ListItem> = app.logs.iter()
        .skip(app.scroll_position)
        .take(log_window_height)
        .map(|s| ListItem::new(s.as_str()).style(Style::default().fg(Color::Gray)))
        .collect();

    let logs_block = List::new(logs_to_show)
        .block(Block::default().borders(Borders::ALL).title(" Server Logs "));
    f.render_widget(logs_block, chunks[2]);
    
    let scrollbar = Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("↑"))
        .end_symbol(Some("↓"));
    let mut scroll_state = ScrollbarState::new(app.logs.len()).position(app.scroll_position);
    
    f.render_stateful_widget(
        scrollbar,
        chunks[2].inner(Margin { vertical: 1, horizontal: 0 }),
        &mut scroll_state,
    );
}