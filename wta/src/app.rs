use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use std::collections::HashMap;
use std::io;
use tokio::sync::mpsc;

use crate::ui;

// --- State types ---

#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

#[derive(Debug, Clone)]
pub enum ChatMessage {
    User(String),
    Agent(String),
    ToolCall {
        id: String,
        title: String,
        status: String,
    },
    Plan(Vec<PlanEntry>),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct PlanEntry {
    pub content: String,
    pub status: PlanEntryStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone)]
pub struct PermOption {
    pub id: String,
    pub name: String,
    pub kind: String,
}

pub struct PermissionState {
    pub description: String,
    pub options: Vec<PermOption>,
    pub selected: usize,
    pub responder: tokio::sync::oneshot::Sender<String>,
}

// --- Events ---

pub enum AppEvent {
    Key(KeyEvent),
    Resize(u16, u16), // terminal resize (handled by ratatui)
    AgentConnected { name: String, session_id: String },
    AgentError(String),
    AgentMessageChunk(String),
    AgentMessageEnd,
    ToolCall {
        id: String,
        title: String,
        status: String,
    },
    ToolCallUpdate {
        id: String,
        status: String,
    },
    Plan(Vec<PlanEntry>),
    PermissionRequest {
        description: String,
        options: Vec<PermOption>,
        responder: tokio::sync::oneshot::Sender<String>,
    },
}

// --- App ---

pub struct App {
    pub state: ConnectionState,
    pub agent_name: String,
    pub session_id: String,
    pub messages: Vec<ChatMessage>,
    pub input: String,
    pub cursor_pos: usize,
    pub tool_calls: HashMap<String, (String, String)>, // id -> (title, status)
    pub permission: Option<PermissionState>,
    pub scroll_offset: usize,
    pub agent_streaming: bool,
    pub should_quit: bool,
    prompt_tx: mpsc::UnboundedSender<String>,
}

impl App {
    pub fn new(prompt_tx: mpsc::UnboundedSender<String>) -> Self {
        Self {
            state: ConnectionState::Connecting,
            agent_name: String::new(),
            session_id: String::new(),
            messages: Vec::new(),
            input: String::new(),
            cursor_pos: 0,
            tool_calls: HashMap::new(),
            permission: None,
            scroll_offset: 0,
            agent_streaming: false,
            should_quit: false,
            prompt_tx,
        }
    }

    pub async fn run(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        mut event_rx: mpsc::UnboundedReceiver<AppEvent>,
    ) -> Result<()> {
        loop {
            terminal.draw(|frame| ui::render(frame, self))?;

            if let Some(event) = event_rx.recv().await {
                self.handle_event(event);
            } else {
                break; // All senders dropped
            }

            if self.should_quit {
                break;
            }
        }
        Ok(())
    }

    fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Key(key) => self.handle_key(key),
            AppEvent::Resize(_, _) => {} // ratatui handles resize
            AppEvent::AgentConnected { name, session_id } => {
                self.agent_name = name;
                self.session_id = session_id;
                self.state = ConnectionState::Connected;
            }
            AppEvent::AgentError(msg) => {
                self.state = ConnectionState::Failed(msg.clone());
                self.messages.push(ChatMessage::Error(msg));
            }
            AppEvent::AgentMessageChunk(text) => {
                self.agent_streaming = true;
                // Append to last agent message or create new one
                if let Some(ChatMessage::Agent(ref mut s)) = self.messages.last_mut() {
                    s.push_str(&text);
                } else {
                    self.messages.push(ChatMessage::Agent(text));
                }
                self.scroll_to_bottom();
            }
            AppEvent::AgentMessageEnd => {
                self.agent_streaming = false;
            }
            AppEvent::ToolCall { id, title, status } => {
                self.tool_calls
                    .insert(id.clone(), (title.clone(), status.clone()));
                self.messages.push(ChatMessage::ToolCall { id, title, status });
                self.scroll_to_bottom();
            }
            AppEvent::ToolCallUpdate { id, status } => {
                if let Some(entry) = self.tool_calls.get_mut(&id) {
                    entry.1 = status.clone();
                }
                // Update in-place in messages
                for msg in &mut self.messages {
                    if let ChatMessage::ToolCall {
                        id: ref mid,
                        status: ref mut s,
                        ..
                    } = msg
                    {
                        if mid == &id {
                            *s = status.clone();
                        }
                    }
                }
            }
            AppEvent::Plan(entries) => {
                self.messages.push(ChatMessage::Plan(entries));
                self.scroll_to_bottom();
            }
            AppEvent::PermissionRequest {
                description,
                options,
                responder,
            } => {
                self.permission = Some(PermissionState {
                    description,
                    options,
                    selected: 0,
                    responder,
                });
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // If permission modal is showing, route keys there
        if let Some(ref mut perm) = self.permission {
            match key.code {
                KeyCode::Up => {
                    if perm.selected > 0 {
                        perm.selected -= 1;
                    }
                }
                KeyCode::Down => {
                    if perm.selected < perm.options.len().saturating_sub(1) {
                        perm.selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let option_id = perm.options[perm.selected].id.clone();
                    // Take ownership to send
                    if let Some(perm) = self.permission.take() {
                        let _ = perm.responder.send(option_id);
                    }
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    // Quick allow: find first allow option
                    if let Some(idx) = perm
                        .options
                        .iter()
                        .position(|o| o.kind.contains("allow"))
                    {
                        let option_id = perm.options[idx].id.clone();
                        if let Some(perm) = self.permission.take() {
                            let _ = perm.responder.send(option_id);
                        }
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    // Quick deny: find first reject option
                    if let Some(idx) = perm
                        .options
                        .iter()
                        .position(|o| o.kind.contains("reject"))
                    {
                        let option_id = perm.options[idx].id.clone();
                        if let Some(perm) = self.permission.take() {
                            let _ = perm.responder.send(option_id);
                        }
                    }
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.agent_streaming {
                    // TODO: send cancel to agent
                    self.agent_streaming = false;
                } else {
                    self.should_quit = true;
                }
            }
            KeyCode::Enter => {
                if !self.input.is_empty() && self.state == ConnectionState::Connected {
                    let text = self.input.clone();
                    self.input.clear();
                    self.cursor_pos = 0;
                    self.messages.push(ChatMessage::User(text.clone()));
                    self.scroll_to_bottom();
                    let _ = self.prompt_tx.send(text);
                }
            }
            KeyCode::Backspace => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                    self.input.remove(self.cursor_pos);
                }
            }
            KeyCode::Delete => {
                if self.cursor_pos < self.input.len() {
                    self.input.remove(self.cursor_pos);
                }
            }
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    self.cursor_pos -= 1;
                }
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input.len() {
                    self.cursor_pos += 1;
                }
            }
            KeyCode::Home => {
                self.cursor_pos = 0;
            }
            KeyCode::End => {
                self.cursor_pos = self.input.len();
            }
            KeyCode::PageUp => {
                self.scroll_offset = self.scroll_offset.saturating_add(10);
            }
            KeyCode::PageDown => {
                self.scroll_offset = self.scroll_offset.saturating_sub(10);
            }
            KeyCode::Char(c) => {
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += 1;
            }
            _ => {}
        }
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }
}
