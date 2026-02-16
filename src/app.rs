use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;

use crate::model::ConversationEntry;

pub struct App {
    pub entries: Vec<ConversationEntry>,
    pub list_state: ListState,
    pub follow: bool,
    pub should_quit: bool,
}

impl App {
    pub fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        Self {
            entries: Vec::new(),
            list_state,
            follow: true,
            should_quit: false,
        }
    }

    pub fn push_entry(&mut self, entry: ConversationEntry) {
        self.entries.push(entry);
        if self.follow {
            self.scroll_to_bottom();
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
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
