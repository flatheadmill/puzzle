use std::cell::RefCell;
use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use textwrap::Options;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug)]
pub struct TextArea {
    text: String,
    cursor_pos: usize,
    wrap_cache: RefCell<Option<WrapCache>>,
    preferred_col: Option<usize>,
    kill_buffer: String,
    style: Style,
    cursor_style: Style,
}

#[derive(Debug, Clone)]
struct WrapCache {
    width: u16,
    lines: Vec<Range<usize>>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TextAreaState {
    pub scroll: u16,
}

impl TextArea {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor_pos: 0,
            wrap_cache: RefCell::new(None),
            preferred_col: None,
            kill_buffer: String::new(),
            style: Style::default(),
            cursor_style: Style::default(),
        }
    }

    pub fn set_style(&mut self, style: Style) {
        self.style = style;
    }

    pub fn set_cursor_style(&mut self, style: Style) {
        self.cursor_style = style;
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn lines(&self) -> Vec<&str> {
        if self.text.is_empty() {
            return vec![""];
        }
        self.text.lines().collect()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor_pos = 0;
        self.wrap_cache.replace(None);
        self.preferred_col = None;
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        if width == 0 {
            return 1;
        }
        self.wrapped_lines(width).len().max(1) as u16
    }

    pub fn cursor_pos_on_screen(&self, area: Rect, state: &TextAreaState) -> Option<(u16, u16)> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let lines = self.wrapped_lines(area.width);
        let scroll = self.effective_scroll(area.height, &lines, state.scroll);
        let idx = Self::wrapped_line_index(&lines, self.cursor_pos)?;
        let range = &lines[idx];
        let col = self.text[range.start..self.cursor_pos].width() as u16;
        let row = idx.saturating_sub(scroll as usize) as u16;
        if row >= area.height {
            return None;
        }
        Some((area.x + col, area.y + row))
    }

    // --- Text editing ---

    fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor_pos, s);
        self.cursor_pos += s.len();
        self.invalidate();
    }

    fn delete_range(&mut self, range: Range<usize>) {
        if range.start >= range.end {
            return;
        }
        let start = range.start.min(self.text.len());
        let end = range.end.min(self.text.len());
        self.text.drain(start..end);
        if self.cursor_pos > start {
            self.cursor_pos = if self.cursor_pos >= end {
                self.cursor_pos - (end - start)
            } else {
                start
            };
        }
        self.invalidate();
    }

    fn delete_backward(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        let prev = self.prev_grapheme(self.cursor_pos);
        self.delete_range(prev..self.cursor_pos);
    }

    fn delete_forward(&mut self) {
        if self.cursor_pos >= self.text.len() {
            return;
        }
        let next = self.next_grapheme(self.cursor_pos);
        self.delete_range(self.cursor_pos..next);
    }

    fn delete_backward_word(&mut self) {
        let target = self.beginning_of_previous_word();
        let range = target..self.cursor_pos;
        self.kill_buffer = self.text[range.clone()].to_string();
        self.delete_range(range);
    }

    fn delete_forward_word(&mut self) {
        let target = self.end_of_next_word();
        if target > self.cursor_pos {
            let range = self.cursor_pos..target;
            self.kill_buffer = self.text[range.clone()].to_string();
            self.delete_range(range);
        }
    }

    fn kill_to_end_of_line(&mut self) {
        let eol = self.end_of_current_line();
        if self.cursor_pos == eol {
            if eol < self.text.len() {
                self.kill_buffer = self.text[self.cursor_pos..self.cursor_pos + 1].to_string();
                self.delete_range(self.cursor_pos..self.cursor_pos + 1);
            }
        } else {
            self.kill_buffer = self.text[self.cursor_pos..eol].to_string();
            self.delete_range(self.cursor_pos..eol);
        }
    }

    fn kill_to_beginning_of_line(&mut self) {
        let bol = self.beginning_of_current_line();
        if bol < self.cursor_pos {
            self.kill_buffer = self.text[bol..self.cursor_pos].to_string();
            self.delete_range(bol..self.cursor_pos);
        }
    }

    fn yank(&mut self) {
        if !self.kill_buffer.is_empty() {
            let text = self.kill_buffer.clone();
            self.insert_str(&text);
        }
    }

    // --- Cursor movement ---

    fn move_left(&mut self) {
        self.cursor_pos = self.prev_grapheme(self.cursor_pos);
        self.preferred_col = None;
    }

    fn move_right(&mut self) {
        self.cursor_pos = self.next_grapheme(self.cursor_pos);
        self.preferred_col = None;
    }

    fn move_to_beginning_of_line(&mut self) {
        self.cursor_pos = self.beginning_of_current_line();
        self.preferred_col = None;
    }

    fn move_to_end_of_line(&mut self) {
        self.cursor_pos = self.end_of_current_line();
        self.preferred_col = None;
    }

    fn move_word_left(&mut self) {
        self.cursor_pos = self.beginning_of_previous_word();
        self.preferred_col = None;
    }

    fn move_word_right(&mut self) {
        self.cursor_pos = self.end_of_next_word();
        self.preferred_col = None;
    }

    fn move_up(&mut self) {
        let info = {
            let cache_ref = self.wrap_cache.borrow();
            cache_ref.as_ref().and_then(|cache| {
                let lines = &cache.lines;
                let idx = Self::wrapped_line_index(lines, self.cursor_pos)?;
                let cur_range = &lines[idx];
                let target_col = self
                    .preferred_col
                    .unwrap_or_else(|| self.text[cur_range.start..self.cursor_pos].width());
                if idx > 0 {
                    let prev = &lines[idx - 1];
                    Some((target_col, Some((prev.start, prev.end))))
                } else {
                    Some((target_col, None))
                }
            })
        };
        if let Some((target_col, dest)) = info {
            match dest {
                Some((start, end)) => {
                    if self.preferred_col.is_none() {
                        self.preferred_col = Some(target_col);
                    }
                    self.move_to_display_col(start, end, target_col);
                    return;
                }
                None => {
                    self.cursor_pos = 0;
                    self.preferred_col = None;
                    return;
                }
            }
        }
        self.cursor_pos = 0;
        self.preferred_col = None;
    }

    fn move_down(&mut self) {
        let info = {
            let cache_ref = self.wrap_cache.borrow();
            cache_ref.as_ref().and_then(|cache| {
                let lines = &cache.lines;
                let idx = Self::wrapped_line_index(lines, self.cursor_pos)?;
                let cur_range = &lines[idx];
                let target_col = self
                    .preferred_col
                    .unwrap_or_else(|| self.text[cur_range.start..self.cursor_pos].width());
                if idx + 1 < lines.len() {
                    let next = &lines[idx + 1];
                    Some((target_col, Some((next.start, next.end))))
                } else {
                    Some((target_col, None))
                }
            })
        };
        if let Some((target_col, dest)) = info {
            match dest {
                Some((start, end)) => {
                    if self.preferred_col.is_none() {
                        self.preferred_col = Some(target_col);
                    }
                    self.move_to_display_col(start, end, target_col);
                    return;
                }
                None => {
                    self.cursor_pos = self.text.len();
                    self.preferred_col = None;
                    return;
                }
            }
        }
        self.cursor_pos = self.text.len();
        self.preferred_col = None;
    }

    // --- Input dispatch ---

    pub fn input(&mut self, event: KeyEvent) {
        if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return;
        }

        let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);
        let alt = event.modifiers.contains(KeyModifiers::ALT);

        match event.code {
            KeyCode::Backspace if ctrl || alt => self.delete_backward_word(),
            KeyCode::Backspace => self.delete_backward(),
            KeyCode::Delete if ctrl || alt => self.delete_forward_word(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Left if ctrl || alt => self.move_word_left(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right if ctrl || alt => self.move_word_right(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Home => self.move_to_beginning_of_line(),
            KeyCode::End => self.move_to_end_of_line(),
            KeyCode::Char('a') if ctrl => self.move_to_beginning_of_line(),
            KeyCode::Char('e') if ctrl => self.move_to_end_of_line(),
            KeyCode::Char('k') if ctrl => self.kill_to_end_of_line(),
            KeyCode::Char('u') if ctrl => self.kill_to_beginning_of_line(),
            KeyCode::Char('y') if ctrl => self.yank(),
            KeyCode::Char('h') if ctrl => self.delete_backward(),
            KeyCode::Char('d') if ctrl => self.delete_forward(),
            KeyCode::Char('w') if ctrl => self.delete_backward_word(),
            KeyCode::Char('b') if ctrl => self.move_left(),
            KeyCode::Char('f') if ctrl => self.move_right(),
            KeyCode::Char('b') if alt => self.move_word_left(),
            KeyCode::Char('f') if alt => self.move_word_right(),
            KeyCode::Char('d') if alt => self.delete_forward_word(),
            KeyCode::Char(c) if !ctrl && !alt => {
                if !c.is_ascii_control() {
                    self.insert_str(&c.to_string());
                }
            }
            _ => {}
        }
    }

    // --- Grapheme helpers ---

    fn prev_grapheme(&self, pos: usize) -> usize {
        if pos == 0 {
            return 0;
        }
        let mut gc = unicode_segmentation::GraphemeCursor::new(pos, self.text.len(), false);
        match gc.prev_boundary(&self.text, 0) {
            Ok(Some(b)) => b,
            _ => pos.saturating_sub(1),
        }
    }

    fn next_grapheme(&self, pos: usize) -> usize {
        if pos >= self.text.len() {
            return self.text.len();
        }
        let mut gc = unicode_segmentation::GraphemeCursor::new(pos, self.text.len(), false);
        match gc.next_boundary(&self.text, 0) {
            Ok(Some(b)) => b,
            _ => pos.saturating_add(1).min(self.text.len()),
        }
    }

    // --- Line helpers ---

    fn beginning_of_current_line(&self) -> usize {
        self.text[..self.cursor_pos]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn end_of_current_line(&self) -> usize {
        self.text[self.cursor_pos..]
            .find('\n')
            .map(|i| i + self.cursor_pos)
            .unwrap_or(self.text.len())
    }

    // --- Word helpers ---

    fn beginning_of_previous_word(&self) -> usize {
        let prefix = &self.text[..self.cursor_pos];
        let trimmed_end = prefix.trim_end().len();
        if trimmed_end == 0 {
            return 0;
        }
        prefix[..trimmed_end]
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0)
    }

    fn end_of_next_word(&self) -> usize {
        let suffix = &self.text[self.cursor_pos..];
        let skip_ws = suffix
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(suffix.len());
        let after_ws = &suffix[skip_ws..];
        let word_end = after_ws
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after_ws.len());
        self.cursor_pos + skip_ws + word_end
    }

    // --- Wrapping ---

    fn invalidate(&mut self) {
        self.wrap_cache.replace(None);
        self.preferred_col = None;
    }

    fn wrapped_lines(&self, width: u16) -> Vec<Range<usize>> {
        {
            let cache = self.wrap_cache.borrow();
            if let Some(c) = cache.as_ref() {
                if c.width == width {
                    return c.lines.clone();
                }
            }
        }
        let lines = wrap_ranges(&self.text, width as usize);
        self.wrap_cache.replace(Some(WrapCache {
            width,
            lines: lines.clone(),
        }));
        lines
    }

    fn wrapped_line_index(lines: &[Range<usize>], pos: usize) -> Option<usize> {
        let idx = lines.partition_point(|r| r.start <= pos);
        if idx == 0 {
            None
        } else {
            Some(idx - 1)
        }
    }

    fn move_to_display_col(&mut self, line_start: usize, line_end: usize, target_col: usize) {
        let end = line_end.min(self.text.len());
        let mut width_so_far = 0usize;
        for (i, g) in self.text[line_start..end].grapheme_indices(true) {
            let gw = g.width();
            if width_so_far + gw > target_col {
                self.cursor_pos = line_start + i;
                return;
            }
            width_so_far += gw;
        }
        self.cursor_pos = end;
    }

    fn effective_scroll(&self, area_height: u16, lines: &[Range<usize>], current_scroll: u16) -> u16 {
        let total = lines.len() as u16;
        if area_height >= total {
            return 0;
        }
        let cursor_line = Self::wrapped_line_index(lines, self.cursor_pos).unwrap_or(0) as u16;
        let max_scroll = total.saturating_sub(area_height);
        let mut scroll = current_scroll.min(max_scroll);
        if cursor_line < scroll {
            scroll = cursor_line;
        } else if cursor_line >= scroll + area_height {
            scroll = cursor_line + 1 - area_height;
        }
        scroll
    }

    // --- Rendering ---

    pub fn render(&self, area: Rect, buf: &mut Buffer, state: &mut TextAreaState) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let lines = self.wrapped_lines(area.width);
        let scroll = self.effective_scroll(area.height, &lines, state.scroll);
        state.scroll = scroll;

        let start = scroll as usize;
        let end = (scroll as usize + area.height as usize).min(lines.len());

        for row in 0..area.height {
            buf.set_style(Rect::new(area.x, area.y + row, area.width, 1), self.style);
        }

        for (row, idx) in (start..end).enumerate() {
            let range = &lines[idx];
            let line_end = range.end.min(self.text.len());
            let line_text = &self.text[range.start..line_end];
            let y = area.y + row as u16;
            buf.set_string(area.x, y, line_text, self.style);
        }

        if let Some(cursor_idx) = Self::wrapped_line_index(&lines, self.cursor_pos) {
            if cursor_idx >= start && cursor_idx < end {
                let range = &lines[cursor_idx];
                let col = self.text[range.start..self.cursor_pos].width() as u16;
                let row = (cursor_idx - start) as u16;
                let x = area.x + col;
                let y = area.y + row;
                if x < area.x + area.width {
                    let cell = &mut buf[(x, y)];
                    cell.set_style(self.cursor_style);
                }
            }
        }
    }
}

fn wrap_ranges(text: &str, width: usize) -> Vec<Range<usize>> {
    if text.is_empty() {
        return vec![0..0];
    }
    let opts = Options::new(width).wrap_algorithm(textwrap::WrapAlgorithm::FirstFit);
    let mut result: Vec<Range<usize>> = Vec::new();
    let mut offset = 0;
    for logical_line in text.split('\n') {
        if logical_line.is_empty() {
            result.push(offset..offset);
            offset += 1; // skip the \n
            continue;
        }
        let wrapped = textwrap::wrap(logical_line, &opts);
        for cow in &wrapped {
            match cow {
                std::borrow::Cow::Borrowed(slice) => {
                    let start = unsafe { slice.as_ptr().offset_from(text.as_ptr()) as usize };
                    let end = start + slice.len();
                    result.push(start..end);
                }
                std::borrow::Cow::Owned(owned) => {
                    let start = offset;
                    let mut end = start;
                    let mut owned_chars = owned.chars().peekable();
                    while let Some(oc) = owned_chars.next() {
                        if end >= text.len() {
                            break;
                        }
                        let src = text[end..].chars().next().unwrap();
                        if oc == src {
                            end += src.len_utf8();
                        }
                    }
                    result.push(start..end);
                }
            }
        }
        offset += logical_line.len() + 1; // +1 for the \n
    }
    if result.is_empty() {
        result.push(0..0);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_textarea() {
        let ta = TextArea::new();
        assert_eq!(ta.text(), "");
        assert!(ta.is_empty());
        assert_eq!(ta.desired_height(80), 1);
    }

    #[test]
    fn insert_and_cursor() {
        let mut ta = TextArea::new();
        ta.insert_str("hello");
        assert_eq!(ta.text(), "hello");
        assert_eq!(ta.cursor_pos, 5);
        ta.move_left();
        assert_eq!(ta.cursor_pos, 4);
        ta.move_right();
        assert_eq!(ta.cursor_pos, 5);
    }

    #[test]
    fn delete_backward_and_forward() {
        let mut ta = TextArea::new();
        ta.insert_str("abcde");
        ta.cursor_pos = 3;
        ta.delete_backward();
        assert_eq!(ta.text(), "abde");
        assert_eq!(ta.cursor_pos, 2);
        ta.delete_forward();
        assert_eq!(ta.text(), "abe");
        assert_eq!(ta.cursor_pos, 2);
    }

    #[test]
    fn wrapping_basic() {
        let ta = TextArea::new();
        let ranges = wrap_ranges("hello world", 5);
        assert_eq!(ranges.len(), 2);
        assert_eq!(&"hello world"[ranges[0].clone()], "hello");
        assert_eq!(&"hello world"[ranges[1].clone()], "world");
        let _ = ta;
    }

    #[test]
    fn wrapping_with_newlines() {
        let ranges = wrap_ranges("ab\ncd", 10);
        assert_eq!(ranges.len(), 2);
        assert_eq!(&"ab\ncd"[ranges[0].clone()], "ab");
        assert_eq!(&"ab\ncd"[ranges[1].clone()], "cd");
    }

    #[test]
    fn cursor_movement_across_wrapped_lines() {
        let mut ta = TextArea::new();
        ta.insert_str("hello world foo");
        // Force wrap at width 5
        let _ = ta.wrapped_lines(5);
        ta.cursor_pos = 3; // in "hello"
        ta.move_down();
        // Should be on second visual line
        assert!(ta.cursor_pos >= 6);
        ta.move_up();
        assert!(ta.cursor_pos <= 5);
    }

    #[test]
    fn kill_and_yank() {
        let mut ta = TextArea::new();
        ta.insert_str("hello world");
        ta.cursor_pos = 5;
        ta.kill_to_end_of_line();
        assert_eq!(ta.text(), "hello");
        assert_eq!(ta.kill_buffer, " world");
        ta.yank();
        assert_eq!(ta.text(), "hello world");
    }

    #[test]
    fn word_navigation() {
        let mut ta = TextArea::new();
        ta.insert_str("one two three");
        ta.cursor_pos = 0;
        ta.move_word_right();
        assert_eq!(ta.cursor_pos, 3);
        ta.move_word_right();
        assert_eq!(ta.cursor_pos, 7);
        ta.move_word_left();
        assert_eq!(ta.cursor_pos, 4);
    }
}
