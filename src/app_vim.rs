//! Desktop event adapter. Printable Vim commands come from Text, never both
//! Text and Key, so one physical key cannot execute an operator twice.
use super::*;
use simptxt::vim::{self, Action, ExCommand, InsertPlan, Mode, Operator};

impl EditorApp {
    pub(super) fn cancel_vim_input(&mut self) {
        if self.vim.mode() == Mode::Visual {
            self.anchor = None;
        }
        self.vim.reset();
        self.composition.clear();
        if self.vim_enabled {
            self.cursor = vim::normal_cursor(&self.doc, self.cursor);
        }
    }

    pub(super) fn vim_event(&mut self, event: &egui::Event, ctx: &egui::Context) -> bool {
        if !self.vim_enabled || !self.editor_focus || self.modal_open() {
            return false;
        }
        let insert = self.vim.mode() == Mode::Insert;
        match event {
            egui::Event::Key {
                key: Key::Escape,
                pressed: true,
                ..
            } => {
                let action = self.vim.handle(vim::Key::Escape);
                self.apply_vim_action(action, ctx);
                true
            }
            egui::Event::Text(_)
            | egui::Event::Paste(_)
            | egui::Event::Ime(_)
            | egui::Event::Cut
                if insert =>
            {
                false
            }
            egui::Event::Text(text) => {
                if !self.ready {
                    self.status = "Vim editing is available when loading finishes".into();
                    return true;
                }
                // Text input is normally one key. Never interpret a pasted or
                // very large synthetic string as an unbounded macro.
                if text.len() > 64 {
                    self.vim.reset();
                    self.anchor = None;
                    self.status =
                        "Long text is not a Vim command; enter Insert mode to insert it".into();
                    return true;
                }
                for (offset, c) in text.char_indices() {
                    let action = self.vim.handle(vim::Key::Char(c));
                    self.apply_vim_action(action, ctx);
                    if self.modal_open()
                        || !self.editor_focus
                        || matches!(action, Action::Message(_))
                    {
                        break;
                    }
                    if self.vim.mode() == Mode::Insert {
                        let rest = &text[offset + c.len_utf8()..];
                        if !rest.is_empty() {
                            self.insert(rest);
                        }
                        break;
                    }
                }
                true
            }
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                if insert {
                    return false;
                }
                let key = match key {
                    Key::R if modifiers.ctrl && !modifiers.alt && !modifiers.shift => {
                        Some(vim::Key::CtrlR)
                    }
                    Key::ArrowLeft if !modifiers.command && !modifiers.alt => Some(vim::Key::Left),
                    Key::ArrowRight if !modifiers.command && !modifiers.alt => {
                        Some(vim::Key::Right)
                    }
                    Key::ArrowUp if !modifiers.command && !modifiers.alt => Some(vim::Key::Up),
                    Key::ArrowDown if !modifiers.command && !modifiers.alt => Some(vim::Key::Down),
                    Key::Enter => Some(vim::Key::Enter),
                    Key::Backspace => Some(vim::Key::Backspace),
                    _ => None,
                };
                if let Some(key) = key {
                    let action = self.vim.handle(key);
                    self.apply_vim_action(action, ctx);
                }
                // Ordinary application shortcuts run before the editor adapter.
                // All other Key events must not fall into the non-Vim editor.
                true
            }
            egui::Event::Paste(_) => {
                self.status =
                    "Enter Insert mode for clipboard paste; p uses the Vim register".into();
                true
            }
            egui::Event::Ime(_) => {
                // Never treat committed IME text or DeleteSurrounding as Normal
                // commands, and never leave stale preedit text on the screen.
                self.composition.clear();
                true
            }
            egui::Event::Copy if self.vim.mode() == Mode::Visual => {
                match vim::visual_range(&self.doc, self.anchor.unwrap_or(self.cursor), self.cursor)
                {
                    Ok(range) => {
                        let text = String::from_utf8(self.doc.read(range))
                            .expect("validated UTF-8 document");
                        ctx.copy_text(text);
                    }
                    Err(e) => self.status = e.to_string(),
                }
                true
            }
            egui::Event::Cut => {
                self.status =
                    "Use d in Visual mode to cut, or enter Insert mode for clipboard cut".into();
                true
            }
            _ => false,
        }
    }

    fn vim_problem(&mut self, problem: impl std::fmt::Display) {
        self.vim.reset();
        self.anchor = None;
        self.status = problem.to_string();
    }

    fn apply_vim_action(&mut self, action: Action, ctx: &egui::Context) {
        // Preview buffers must never be edited or yanked as if fully loaded.
        if !self.ready
            && !matches!(
                action,
                Action::None | Action::Normal { .. } | Action::Move { .. } | Action::Message(_)
            )
        {
            self.vim_problem("Vim editing is available when loading finishes");
            return;
        }
        match action {
            Action::None | Action::PassThrough => {}
            Action::Message(message) => self.status = message.into(),
            Action::Normal { from_insert } => {
                self.composition.clear();
                self.anchor = None;
                let range = vim::line_range(&self.doc, self.doc.line_of_offset(self.cursor));
                let cursor = if from_insert && self.cursor > range.start {
                    view::previous(&self.doc, self.cursor)
                } else {
                    self.cursor
                };
                self.move_to(vim::normal_cursor(&self.doc, cursor), false);
            }
            Action::StartVisual => {
                self.cursor = vim::normal_cursor(&self.doc, self.cursor);
                self.anchor = Some(self.cursor);
                self.ensure_visible();
            }
            Action::EndVisual => {
                self.anchor = None;
            }
            Action::Move { motion, count } => {
                match vim::motion_target(&self.doc, self.cursor, motion, count) {
                    Ok(cursor) => self.move_to(cursor, self.vim.mode() == Mode::Visual),
                    Err(e) => self.vim_problem(e),
                }
            }
            Action::Insert(kind) => {
                match vim::insert_plan(&self.doc, self.cursor, kind, self.newline()) {
                    Ok(plan) => self.apply_vim_insert(plan, false),
                    Err(e) => self.vim_problem(e),
                }
            }
            Action::Operate {
                operator,
                motion,
                count,
            } => match vim::operator_range(&self.doc, self.cursor, operator, motion, count) {
                Ok(plan) => self.apply_vim_operator(
                    operator,
                    plan.range,
                    plan.register_range,
                    plan.linewise,
                ),
                Err(e) => self.vim_problem(e),
            },
            Action::OperateVisual(operator) => {
                match vim::visual_range(&self.doc, self.anchor.unwrap_or(self.cursor), self.cursor)
                {
                    Ok(range) => self.apply_vim_operator(operator, range.clone(), range, false),
                    Err(e) => self.vim_problem(e),
                }
            }
            Action::DeleteChars(count) => {
                match vim::delete_chars_range(&self.doc, self.cursor, count) {
                    Ok(range) => {
                        self.apply_vim_operator(Operator::Delete, range.clone(), range, false)
                    }
                    Err(e) => self.vim_problem(e),
                }
            }
            Action::Paste { before, count } => {
                match vim::paste_plan(
                    &self.doc,
                    self.cursor,
                    &self.vim_register,
                    before,
                    count,
                    self.newline(),
                ) {
                    Ok(plan) => self.apply_vim_insert(plan, true),
                    Err(e) => self.vim_problem(e),
                }
            }
            Action::Undo(count) | Action::Redo(count) => {
                let redo = matches!(action, Action::Redo(_));
                let mut changed = false;
                for _ in 0..count.min(vim::MAX_COUNT) {
                    let success = if redo {
                        self.doc.redo()
                    } else {
                        self.doc.undo()
                    };
                    if !success {
                        break;
                    }
                    changed = true;
                }
                self.anchor = None;
                if changed {
                    self.changed();
                }
                self.cursor = vim::normal_cursor(&self.doc, self.cursor);
                self.ensure_visible();
            }
            Action::Ex(command) => match command {
                ExCommand::Write => self.save(ctx),
                ExCommand::Quit => self.request(Pending::CloseTab, ctx),
                ExCommand::ForceQuit => {
                    if self.work.is_some() || self.dialog_pending {
                        self.status = "Finish or cancel the file operation before closing".into();
                    } else {
                        self.perform(Pending::CloseTab, ctx);
                    }
                }
                ExCommand::WriteQuit => {
                    if self.work.is_some() || self.dialog_pending {
                        self.status =
                            "Finish or cancel the file operation before saving and closing".into();
                    } else {
                        // Pending is intentionally retained on cancellation or
                        // failure. Only a successful, current-revision save
                        // causes poll() to execute the close.
                        self.pending = Some(Pending::CloseTab);
                        self.continue_after_save = true;
                        self.save(ctx);
                    }
                }
            },
        }
    }

    fn apply_vim_insert(&mut self, plan: InsertPlan, normal: bool) {
        if !plan.text.is_empty()
            && let Err(e) = self.doc.replace(plan.offset..plan.offset, &plan.text)
        {
            self.vim_problem(e);
            return;
        }
        self.anchor = None;
        self.cursor = plan.cursor;
        if !plan.text.is_empty() {
            self.changed();
        }
        if normal {
            self.cursor = vim::normal_cursor(&self.doc, self.cursor);
        }
        self.ensure_visible();
    }

    fn apply_vim_operator(
        &mut self,
        operator: Operator,
        range: Range<usize>,
        register_range: Range<usize>,
        linewise: bool,
    ) {
        // Helpers already enforce the limit. This defensive check also protects
        // future direct callers from allocating an entire large-file selection.
        if range.len() > vim::EDIT_LIMIT || register_range.len() > vim::EDIT_LIMIT {
            self.vim_problem(vim::VimError::EditLimit);
            return;
        }
        let text =
            String::from_utf8(self.doc.read(register_range)).expect("validated UTF-8 document");
        if operator != Operator::Yank
            && !range.is_empty()
            && let Err(e) = self.doc.replace(range.clone(), "")
        {
            self.vim_problem(e);
            return;
        }
        // Empty x/d motions must not overwrite a useful register.
        if !text.is_empty() || linewise {
            self.vim_register = vim::Register { text, linewise };
        }
        self.cursor = range.start;
        self.anchor = None;
        if operator != Operator::Yank && !range.is_empty() {
            self.changed();
        }
        if operator == Operator::Change {
            self.vim.set_mode(Mode::Insert);
        } else {
            self.vim.set_mode(Mode::Normal);
            self.cursor = vim::normal_cursor(&self.doc, self.cursor);
        }
        self.ensure_visible();
    }
}
