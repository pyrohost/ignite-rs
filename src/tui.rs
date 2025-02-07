use std::{collections::HashMap, io::{self, Stdout}, time::Duration};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem},
    Terminal,
};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub enum NodeStatus {
    Waiting,
    Building,
    Pushing,
    Activating,
    Done,
    Failed(String),
    RolledBack,
}

impl NodeStatus {
    pub fn color(&self) -> Color {
        match self {
            NodeStatus::Waiting => Color::Yellow,
            NodeStatus::Building => Color::Blue,
            NodeStatus::Pushing => Color::Cyan,
            NodeStatus::Activating => Color::Magenta,
            NodeStatus::Done => Color::Green,
            NodeStatus::Failed(_) => Color::Red,
            NodeStatus::RolledBack => Color::Yellow,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            NodeStatus::Waiting => "Waiting",
            NodeStatus::Building => "Building",
            NodeStatus::Pushing => "Pushing",
            NodeStatus::Activating => "Activating",
            NodeStatus::Done => "Done",
            NodeStatus::Failed(_) => "Failed",
            NodeStatus::RolledBack => "Rolled Back",
        }
    }
}

#[derive(Debug)]
pub struct NodeState {
    pub name: String,
    pub status: NodeStatus,
    pub logs: Vec<String>,
    pub progress: f32,
    pub eval_cache_hit: bool,
    pub activation_start: Option<std::time::Instant>,
    pub deployment_start: std::time::Instant,
}

pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    pub(crate) nodes: HashMap<String, NodeState>,
    status_rx: mpsc::Receiver<(String, NodeStatus)>,
    log_rx: mpsc::Receiver<(String, String)>,
    progress_rx: mpsc::Receiver<(String, f32)>,
    status_tx: mpsc::Sender<(String, NodeStatus)>,
    log_tx: mpsc::Sender<(String, String)>,
    progress_tx: mpsc::Sender<(String, f32)>,
    selected_node: Option<String>,
    log_scroll: usize,
}

impl Tui {
    pub fn new() -> io::Result<Self> {
        // Set up terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        // Set up channels
        let (status_tx, status_rx) = mpsc::channel(100);
        let (log_tx, log_rx) = mpsc::channel(100);
        let (progress_tx, progress_rx) = mpsc::channel(100);

        Ok(Self {
            terminal,
            nodes: HashMap::new(),
            status_rx,
            log_rx,
            progress_rx,
            status_tx,
            log_tx,
            progress_tx,
            selected_node: None,
            log_scroll: 0,
        })
    }

    pub fn status_sender(&self) -> mpsc::Sender<(String, NodeStatus)> {
        self.status_tx.clone()
    }

    pub fn log_sender(&self) -> mpsc::Sender<(String, String)> {
        self.log_tx.clone()
    }

    pub fn progress_sender(&self) -> mpsc::Sender<(String, f32)> {
        self.progress_tx.clone()
    }

    pub fn update_cache_hit(&mut self, node_name: &str, is_cached: bool) {
        if let Some(node) = self.nodes.get_mut(node_name) {
            node.eval_cache_hit = is_cached;
        }
    }

    pub fn set_activation_start(&mut self, node_name: &str) {
        if let Some(node) = self.nodes.get_mut(node_name) {
            node.activation_start = Some(std::time::Instant::now());
        }
    }

    pub fn update_status(&mut self, node_name: &str, status: NodeStatus) {
        if let Some(node) = self.nodes.get_mut(node_name) {
            node.status = status;
        }
    }

    pub fn add_node(&mut self, name: String) {
        self.nodes.insert(
            name.clone(),
            NodeState {
                name,
                status: NodeStatus::Waiting,
                logs: Vec::new(),
                progress: 0.0,
                eval_cache_hit: false,
                activation_start: None,
                deployment_start: std::time::Instant::now(),
            },
        );
        if self.selected_node.is_none() {
            self.selected_node = Some(self.nodes.keys().next().unwrap().clone());
        }
    }

    pub fn format_duration(duration: std::time::Duration) -> String {
        let secs = duration.as_secs();
        if secs < 60 {
            format!("{}s", secs)
        } else {
            format!("{}m {}s", secs / 60, secs % 60)
        }
    }

    pub async fn run(mut self) -> io::Result<()> {
        loop {
            // Handle events
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    match key.code {
                        KeyCode::Char('q') => break,
                        KeyCode::Down => {
                            if let Some(current) = &self.selected_node {
                                let keys: Vec<_> = self.nodes.keys().cloned().collect();
                                if let Some(pos) = keys.iter().position(|x| x == current) {
                                    if pos < keys.len() - 1 {
                                        self.selected_node = Some(keys[pos + 1].clone());
                                        self.log_scroll = 0; // Reset scroll on node change
                                    }
                                }
                            }
                        }
                        KeyCode::Up => {
                            if let Some(current) = &self.selected_node {
                                let keys: Vec<_> = self.nodes.keys().cloned().collect();
                                if let Some(pos) = keys.iter().position(|x| x == current) {
                                    if pos > 0 {
                                        self.selected_node = Some(keys[pos - 1].clone());
                                        self.log_scroll = 0; // Reset scroll on node change
                                    }
                                }
                            }
                        }
                        KeyCode::PageUp => {
                            if let Some(selected) = &self.selected_node {
                                if let Some(_) = self.nodes.get(selected) {
                                    if self.log_scroll > 0 {
                                        self.log_scroll = self.log_scroll.saturating_sub(10);
                                    }
                                }
                            }
                        }
                        KeyCode::PageDown => {
                            if let Some(selected) = &self.selected_node {
                                if let Some(node) = self.nodes.get(selected) {
                                    if self.log_scroll + 10 < node.logs.len() {
                                        self.log_scroll += 10;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Handle status updates
            while let Ok((name, status)) = self.status_rx.try_recv() {
                if let Some(node) = self.nodes.get_mut(&name) {
                    node.status = status;
                }
            }

            // Handle log updates
            while let Ok((name, log)) = self.log_rx.try_recv() {
                if let Some(node) = self.nodes.get_mut(&name) {
                    node.logs.push(log);
                }
            }

            // Handle progress updates
            while let Ok((name, progress)) = self.progress_rx.try_recv() {
                if let Some(node) = self.nodes.get_mut(&name) {
                    node.progress = progress;
                }
            }

            // Draw UI
            self.terminal.draw(|f| {
                let size = f.area();
                let chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(30),
                        Constraint::Percentage(70),
                    ])
                    .split(size);

                // Draw status list
                let items: Vec<ListItem> = self
                    .nodes
                    .values()
                    .map(|node| {
                        let status_style = Style::default()
                            .fg(node.status.color())
                            .add_modifier(Modifier::BOLD);

                        let status_text = match &node.status {
                            NodeStatus::Failed(err) => format!("{}: {}", node.status.as_str(), err),
                            _ => node.status.as_str().to_string(),
                        };

                        let progress_text = if node.progress > 0.0 {
                            format!(" [{:.0}%]", node.progress * 100.0)
                        } else {
                            String::new()
                        };

                        let now = std::time::Instant::now();
                        let duration_text = match node.status {
                            NodeStatus::Done | NodeStatus::Failed(_) => {
                                format!(" ({})", Self::format_duration(now - node.deployment_start))
                            }
                            _ => {
                                if let Some(start) = node.activation_start {
                                    format!(" (activating: {})", Self::format_duration(now - start))
                                } else {
                                    format!(" ({})", Self::format_duration(now - node.deployment_start))
                                }
                            }
                        };

                        let cache_text = if node.eval_cache_hit {
                            " [cache hit]"
                        } else {
                            ""
                        };

                        let line = Line::from(vec![
                            format!("{}: ", node.name).into(),
                            ratatui::text::Span::styled(status_text, status_style),
                            ratatui::text::Span::raw(progress_text),
                            ratatui::text::Span::raw(duration_text),
                            ratatui::text::Span::styled(
                                cache_text,
                                Style::default().fg(Color::Green),
                            ),
                        ]);

                        ListItem::new(vec![line])
                    })
                    .collect();

                let block = Block::default()
                    .title("Deployment Status")
                    .borders(Borders::ALL);

                let list = List::new(items)
                    .block(block)
                    .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

                f.render_widget(list, chunks[0]);

                // Draw logs with scrolling
                if let Some(selected) = &self.selected_node {
                    if let Some(node) = self.nodes.get(selected) {
                        let logs: Vec<ListItem> = node
                            .logs
                            .iter()
                            .skip(self.log_scroll)
                            .map(|log| ListItem::new(Line::from(log.as_str())))
                            .collect();

                        let block = Block::default()
                            .title(format!("Logs: {} (PgUp/PgDn to scroll)", selected))
                            .borders(Borders::ALL);

                        let log_list = List::new(logs).block(block);
                        f.render_widget(log_list, chunks[1]);
                    }
                }
            })?;

            // Check if all nodes are done
            if self.nodes.values().all(|node| {
                matches!(node.status, NodeStatus::Done | NodeStatus::Failed(_))
            }) {
                break;
            }
        }

        // Restore terminal
        disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        self.terminal.show_cursor()?;

        Ok(())
    }
}
