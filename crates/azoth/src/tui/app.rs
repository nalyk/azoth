//! AppState + the biased `tokio::select!` main loop.
//!
//! Channel sizing matches draft_plan § MED-3: bounded everywhere, biased
//! branch priority so Ctrl+C / keyboard input never starves under fast
//! model streaming.

use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use tokio::sync::mpsc;

use azoth_core::adapter::{MockAdapter, ProviderAdapter, ProviderProfile};

use super::render;

#[derive(Debug, Clone)]
pub enum InputEvent {
    Key(KeyEvent),
    Resize,
}

#[derive(Debug, Clone)]
pub enum ModelEvent {
    Chunk(String),
    Done,
}

#[derive(Debug, Clone)]
pub enum ToolEvent {
    Started(String),
    Finished(String),
}

#[derive(Debug, Clone)]
pub enum AuthorityEvent {
    ApprovalRequested(String),
    ApprovalResolved,
}

pub struct AppState {
    pub input_buffer: String,
    pub transcript: Vec<String>,
    pub status: String,
    pub ctx_pct: u8,
    pub dirty: bool,
    pub should_quit: bool,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            input_buffer: String::new(),
            transcript: vec!["azoth · ready".to_string()],
            status: "mock adapter".to_string(),
            ctx_pct: 0,
            dirty: true,
            should_quit: false,
        }
    }

    pub fn handle_input(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::Key(key) => self.handle_key(key),
            InputEvent::Resize => self.dirty = true,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
            | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            (KeyCode::Enter, _) => {
                if !self.input_buffer.is_empty() {
                    let line = std::mem::take(&mut self.input_buffer);
                    self.transcript.push(format!("> {line}"));
                    self.transcript.push("(mock) hello from azoth".to_string());
                    self.dirty = true;
                }
            }
            (KeyCode::Backspace, _) => {
                self.input_buffer.pop();
                self.dirty = true;
            }
            (KeyCode::Char(c), _) => {
                self.input_buffer.push(c);
                self.dirty = true;
            }
            _ => {}
        }
    }

    pub fn handle_model(&mut self, ev: ModelEvent) {
        match ev {
            ModelEvent::Chunk(s) => {
                if let Some(last) = self.transcript.last_mut() {
                    last.push_str(&s);
                } else {
                    self.transcript.push(s);
                }
                self.dirty = true;
            }
            ModelEvent::Done => self.dirty = true,
        }
    }

    pub fn handle_tool(&mut self, ev: ToolEvent) {
        match ev {
            ToolEvent::Started(name) => self.transcript.push(format!("  ▸ {name}")),
            ToolEvent::Finished(name) => self.transcript.push(format!("  ✓ {name}")),
        }
        self.dirty = true;
    }

    pub fn handle_authority(&mut self, ev: AuthorityEvent) {
        match ev {
            AuthorityEvent::ApprovalRequested(summary) => {
                self.transcript.push(format!("⧗ approval: {summary}"))
            }
            AuthorityEvent::ApprovalResolved => self.transcript.push("✓ approval resolved".into()),
        }
        self.dirty = true;
    }
}

pub async fn run_app() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (input_tx, mut input_rx) = mpsc::channel::<InputEvent>(128);
    let (_model_tx, mut model_rx) = mpsc::channel::<ModelEvent>(64);
    let (_tool_tx, mut tool_rx) = mpsc::channel::<ToolEvent>(32);
    let (_auth_tx, mut auth_rx) = mpsc::channel::<AuthorityEvent>(8);

    // Dedicated input task — prevents the keyboard reader from being starved
    // by model streaming in the main select loop.
    tokio::spawn(async move {
        let mut events = EventStream::new();
        while let Some(Ok(ev)) = events.next().await {
            let to_send = match ev {
                TermEvent::Key(k) => Some(InputEvent::Key(k)),
                TermEvent::Resize(_, _) => Some(InputEvent::Resize),
                _ => None,
            };
            if let Some(e) = to_send {
                if input_tx.send(e).await.is_err() {
                    break;
                }
            }
        }
    });

    // Wire a mock adapter just to exercise the trait; not yet driving turns
    // from the TUI.
    let _mock: Box<dyn ProviderAdapter> = Box::new(MockAdapter::echo(
        ProviderProfile::anthropic_default("claude-sonnet-4-6"),
    ));

    let mut state = AppState::new();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(200));

    loop {
        tokio::select! {
            biased;

            Some(ev) = input_rx.recv() => state.handle_input(ev),
            Some(ev) = auth_rx.recv() => state.handle_authority(ev),
            Some(ev) = tool_rx.recv() => state.handle_tool(ev),
            Some(ev) = model_rx.recv() => state.handle_model(ev),
            _ = ticker.tick() => {}
            else => break,
        }

        if state.dirty {
            terminal.draw(|f| render::frame(f, &state))?;
            state.dirty = false;
        }
        if state.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    terminal.backend_mut().execute(LeaveAlternateScreen)?;
    Ok(())
}
