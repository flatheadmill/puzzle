// Application state and input handling. Two modes: Scroll for reading
// transcripts with vim-style navigation (j/k, G/g, PageUp/PageDown), and
// Input for the REPL prompt. RunState tracks whether claude --print is in
// flight — while running, input mode only allows Escape and Ctrl-C.
//
// Input uses tui-textarea, which handles cursor movement, word boundaries,
// selection, clipboard paste, and unicode correctly. The app intercepts
// Escape and Ctrl-C before delegating to the textarea, and Enter is handled
// by the event loop in main.rs to trigger submission. The textarea renders
// itself as a ratatui widget — the app configures it on construction and
// styles it dynamically in the draw closure based on mode and run state.
//
// Follow mode auto-scrolls to the bottom when new entries arrive from the
// tailer. It disables when the user scrolls manually (any j/k/arrow) and
// re-enables on G or End. This mirrors the behavior of tail -f in a terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::ListState;
use tui_textarea::{Input, TextArea};

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

/// An approval request from Wicket waiting for the operator's decision.
pub struct PendingApproval {
    pub tool_name: String,
    pub input_summary: String,
}

pub enum ApprovalDecision {
    Allow,
    Deny,
}

pub struct App {
    pub entries: Vec<ConversationEntry>,
    pub list_state: ListState,
    pub follow: bool,
    pub should_quit: bool,
    pub mode: Mode,
    pub run_state: RunState,
    pub textarea: TextArea<'static>,
    pub repl_mode: bool,
    /// Shown in the prompt bar when a remote target is active. The border
    /// goes yellow and the label appears as [yolo] so the operator always
    /// knows where the next prompt will run.
    pub target_label: Option<&'static str>,
    /// When set, an approval dialog is visible and keyboard input routes
    /// to it instead of the normal handlers. The decision clears this and
    /// sets approval_decision for the event loop to act on.
    pub pending_approval: Option<PendingApproval>,
    pub approval_decision: Option<ApprovalDecision>,
}

fn new_textarea() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_cursor_line_style(Style::default());
    textarea.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    textarea
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
            textarea: new_textarea(),
            repl_mode,
            target_label: None,
            pending_approval: None,
            approval_decision: None,
        }
    }

    pub fn push_entry(&mut self, entry: ConversationEntry) {
        let block_count = entry.blocks.len();
        let kind = match entry.kind {
            crate::model::EntryKind::User => "user",
            crate::model::EntryKind::Assistant => "assistant",
        };
        tracing::info!(
            kind, blocks = block_count,
            total = self.entries.len() + 1,
            "push_entry to UI"
        );
        self.entries.push(entry);
        if self.follow {
            self.scroll_to_bottom();
        }
    }

    pub fn submit_input(&mut self) -> Option<String> {
        let content = self.textarea.lines().join("\n");
        if content.trim().is_empty() {
            return None;
        }
        self.textarea = new_textarea();
        Some(content)
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Approval dialog takes priority over all other input.
        if self.pending_approval.is_some() {
            match key.code {
                KeyCode::Char('y') => {
                    self.pending_approval = None;
                    self.approval_decision = Some(ApprovalDecision::Allow);
                }
                KeyCode::Char('n') => {
                    self.pending_approval = None;
                    self.approval_decision = Some(ApprovalDecision::Deny);
                }
                _ => {}
            }
            return;
        }
        match self.mode {
            Mode::Input => self.handle_input_key(key),
            Mode::Scroll => self.handle_scroll_key(key),
        }
    }

    // In input mode, Escape and Ctrl-C are intercepted before reaching the
    // textarea. Everything else delegates to tui-textarea via Input::from,
    // which handles cursor movement, word boundaries, selection, clipboard,
    // backspace, delete, home, end — all the things the hand-rolled handler
    // used to do character by character.
    fn handle_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Scroll;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            _ => {
                if self.run_state != RunState::Running {
                    self.textarea.input(Input::from(key));
                }
            }
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
