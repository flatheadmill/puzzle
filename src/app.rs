// Application state and input handling. Two modes: Scroll for reading
// transcripts with vim-style navigation (j/k, G/g, PageUp/PageDown), and
// Input for the REPL prompt. RunState tracks whether claude --print is in
// flight — while running, input mode only allows Escape and Ctrl-C.
//
// Follow mode auto-scrolls to the bottom when new entries arrive from the
// tailer. It disables when the user scrolls manually (any j/k/arrow) and
// re-enables on G or End. This mirrors the behavior of tail -f in a terminal.
//
// Known issue: cursor_pos is treated as a character index but String::insert
// and String::remove use byte offsets. Non-ASCII input can panic. This gets
// fixed when tui-textarea replaces the hand-rolled input (step 007).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;

use crate::model::{ContentBlock, ConversationEntry};

#[derive(PartialEq)]
pub enum Mode {
    Scroll,
    Input,
}

#[derive(PartialEq)]
pub enum RunState {
    Idle,
    Running,
}

pub struct App {
    pub entries: Vec<ConversationEntry>,
    pub list_state: ListState,
    pub follow: bool,
    pub should_quit: bool,
    pub mode: Mode,
    pub run_state: RunState,
    pub input: String,
    pub cursor_pos: usize,
    pub repl_mode: bool,
}

impl App {
    pub fn new(repl_mode: bool) -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            entries: Vec::new(),
            list_state,
            follow: true,
            should_quit: false,
            mode: if repl_mode { Mode::Input } else { Mode::Scroll },
            run_state: RunState::Idle,
            input: String::new(),
            cursor_pos: 0,
            repl_mode,
        }
    }

    pub fn push_entry(&mut self, entry: ConversationEntry) {
        self.entries.push(entry);
        if self.follow {
            self.scroll_to_bottom();
        }
    }

    pub fn submit_input(&mut self) -> Option<String> {
        if self.input.trim().is_empty() {
            return None;
        }
        let prompt = self.input.clone();
        self.input.clear();
        self.cursor_pos = 0;
        Some(prompt)
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::Input => self.handle_input_key(key),
            Mode::Scroll => self.handle_scroll_key(key),
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent) {
        if self.run_state == RunState::Running {
            // only allow escape and ctrl-c while running
            match key.code {
                KeyCode::Esc => self.mode = Mode::Scroll,
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Esc => self.mode = Mode::Scroll,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            KeyCode::Enter => {
                // submit is handled by the caller checking submit_input
                // we just signal by leaving the input as-is
            }
            KeyCode::Char(c) => {
                self.input.insert(self.cursor_pos, c);
                self.cursor_pos += 1;
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
                self.cursor_pos = self.cursor_pos.saturating_sub(1);
            }
            KeyCode::Right => {
                self.cursor_pos = (self.cursor_pos + 1).min(self.input.len());
            }
            KeyCode::Home => {
                self.cursor_pos = 0;
            }
            KeyCode::End => {
                self.cursor_pos = self.input.len();
            }
            _ => {}
        }
    }

    fn handle_scroll_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            KeyCode::Char('i') if self.repl_mode => {
                self.mode = Mode::Input;
            }
            KeyCode::Enter => {
                self.toggle_tool_results();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll_down(1);
                self.follow = false;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.scroll_up(1);
                self.follow = false;
            }
            KeyCode::PageDown => {
                self.scroll_down(20);
                self.follow = false;
            }
            KeyCode::PageUp => {
                self.scroll_up(20);
                self.follow = false;
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.scroll_to_bottom();
                self.follow = true;
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.list_state.select(Some(0));
                self.follow = false;
            }
            _ => {}
        }
    }

    // Toggles all tool results in the selected entry at once. The current
    // selection model is entry-level (ListState), so there is no way to target
    // an individual tool result within a multi-result entry. Per-block toggling
    // would require sub-entry selection, a larger change for later.
    fn toggle_tool_results(&mut self) {
        if let Some(idx) = self.list_state.selected() {
            if let Some(entry) = self.entries.get_mut(idx) {
                for block in &mut entry.blocks {
                    if let ContentBlock::ToolResult { collapsed, .. } = block {
                        *collapsed = !*collapsed;
                    }
                }
            }
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let current = self.list_state.selected().unwrap_or(0);
        let max = self.entries.len().saturating_sub(1);
        let next = (current + n).min(max);
        self.list_state.select(Some(next));
    }

    fn scroll_up(&mut self, n: usize) {
        let current = self.list_state.selected().unwrap_or(0);
        let next = current.saturating_sub(n);
        self.list_state.select(Some(next));
    }

    fn scroll_to_bottom(&mut self) {
        if !self.entries.is_empty() {
            self.list_state
                .select(Some(self.entries.len().saturating_sub(1)));
        }
    }
}
