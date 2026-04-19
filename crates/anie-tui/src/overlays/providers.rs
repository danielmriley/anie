use std::{
    collections::HashMap,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, Widget, Wrap,
    },
};
use tokio::sync::mpsc;

use anie_auth::CredentialStore;
use anie_config::{
    CliOverrides, ConfigMutator, find_project_config, global_config_path, load_config_with_paths,
    preferred_write_target,
};
use anie_provider::{ApiKind, CostPerMillion, Model, ModelInfo};
use anie_providers_builtin::{ModelDiscoveryRequest, builtin_models, discover_models};

use crate::{ModelPickerAction, ModelPickerPane, Spinner};

/// A configured provider shown in the management screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEntry {
    pub name: String,
    pub provider_type: ProviderType,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub default_model: String,
    pub has_credential: bool,
    pub is_default: bool,
}

/// High-level provider classification for rendering and actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderType {
    Local,
    ApiKey,
    Custom,
}

/// Result of a user-initiated provider test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestResult {
    Pending,
    Success { latency_ms: u64 },
    Failed { error: String },
}

/// Actions returned by the provider management screen.
///
/// The `ConfigChanged` variant is large because it carries a full `Model`;
/// kept inline to avoid boxing on a cold path (user-initiated action).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderManagementAction {
    Continue,
    Close,
    ConfigChanged {
        provider: Option<String>,
        model: Option<String>,
        resolved_model: Option<Model>,
        message: String,
    },
}

// Plan 02 replaces this enum with a trait-object-based overlay shape.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
enum ProviderManagementMode {
    Table,
    ActionMenu {
        selected: usize,
    },
    ConfirmDelete,
    EditApiKey {
        input: TextField,
    },
    Busy {
        message: String,
    },
    PickingModel {
        entry: ProviderEntry,
        picker: ModelPickerPane,
    },
    Status {
        message: String,
        is_error: bool,
    },
}

#[derive(Debug)]
enum WorkerEvent {
    TestCompleted {
        row_index: usize,
        provider: String,
        result: TestResult,
    },
    ApiKeySaved {
        provider: String,
        result: Result<(), String>,
    },
    ModelsDiscovered {
        entry: ProviderEntry,
        result: Result<Vec<ModelInfo>, String>,
    },
}

#[derive(Debug, Clone, Copy)]
enum ActionItem {
    TestConnection,
    ViewModels,
    EditApiKey,
    SetAsDefault,
    DeleteProvider,
}

use crate::widgets::{TextField, centered_rect, footer_line};

/// Provider-management overlay widget.
pub struct ProviderManagementScreen {
    providers: Vec<ProviderEntry>,
    selected: usize,
    mode: ProviderManagementMode,
    credential_store: CredentialStore,
    test_results: HashMap<usize, TestResult>,
    worker_tx: mpsc::UnboundedSender<WorkerEvent>,
    worker_rx: mpsc::UnboundedReceiver<WorkerEvent>,
    spinner: Spinner,
    write_target: PathBuf,
}

impl crate::overlay::OverlayScreen for ProviderManagementScreen {
    fn dispatch_key(&mut self, key: KeyEvent) -> crate::overlay::OverlayOutcome {
        crate::overlay::OverlayOutcome::ProviderManagement(self.handle_key(key))
    }

    fn dispatch_tick(&mut self) -> crate::overlay::OverlayOutcome {
        crate::overlay::OverlayOutcome::ProviderManagement(self.handle_tick())
    }

    fn dispatch_render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.render(frame, area);
    }
}

impl ProviderManagementScreen {
    /// Create a management screen using the global config and default credential store.
    pub fn new() -> Result<Self> {
        let cwd = std::env::current_dir().context("failed to determine current directory")?;
        let write_target =
            preferred_write_target(&cwd).context("home directory is not available")?;
        let project_config_path = find_project_config(&cwd);
        let credential_store = CredentialStore::new();
        Self::with_paths(write_target, project_config_path, credential_store)
    }

    fn with_paths(
        write_target: PathBuf,
        project_config_path: Option<PathBuf>,
        credential_store: CredentialStore,
    ) -> Result<Self> {
        let providers = load_provider_entries(
            &write_target,
            project_config_path.as_deref(),
            &credential_store,
        )?;
        let (worker_tx, worker_rx) = mpsc::unbounded_channel();
        Ok(Self {
            providers,
            selected: 0,
            mode: ProviderManagementMode::Table,
            credential_store,
            test_results: HashMap::new(),
            worker_tx,
            worker_rx,
            spinner: Spinner::new(),
            write_target,
        })
    }

    /// Handle keyboard input.
    pub fn handle_key(&mut self, key: KeyEvent) -> ProviderManagementAction {
        match &mut self.mode {
            ProviderManagementMode::Table => self.handle_table_key(key),
            ProviderManagementMode::ActionMenu { .. } => self.handle_action_menu_key(key),
            ProviderManagementMode::ConfirmDelete => self.handle_confirm_delete_key(key),
            ProviderManagementMode::EditApiKey { .. } => self.handle_edit_key_input(key),
            ProviderManagementMode::Busy { .. } => self.handle_busy_key(key),
            ProviderManagementMode::PickingModel { .. } => self.handle_model_picker_key(key),
            ProviderManagementMode::Status { .. } => {
                self.mode = ProviderManagementMode::Table;
                ProviderManagementAction::Continue
            }
        }
    }

    /// Poll background workers.
    pub fn handle_tick(&mut self) -> ProviderManagementAction {
        let mut action = ProviderManagementAction::Continue;
        while let Ok(event) = self.worker_rx.try_recv() {
            action = self.handle_worker_event(event);
            if !matches!(action, ProviderManagementAction::Continue) {
                return action;
            }
        }
        action
    }

    /// Render the management screen.
    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        Clear.render(area, frame.buffer_mut());
        let popup = centered_rect(area, 92, 82, 26, 18);
        let block = Block::default()
            .title(Line::from(vec![Span::styled(
                " Configured Providers ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(popup);
        block.render(popup, frame.buffer_mut());

        let spinner_frame = self.spinner.tick().to_string();
        match &self.mode {
            ProviderManagementMode::Table => self.render_table(frame, inner),
            ProviderManagementMode::ActionMenu { selected } => {
                self.render_action_menu(frame, inner, *selected)
            }
            ProviderManagementMode::ConfirmDelete => self.render_status_panel(
                frame,
                inner,
                "Delete Provider",
                &format!(
                    "Delete provider '{}' and remove its credential?",
                    self.selected_provider_name().unwrap_or_default()
                ),
                Color::Red,
                footer_line("[y] Delete   [n/Esc] Cancel"),
            ),
            ProviderManagementMode::EditApiKey { input } => {
                self.render_edit_api_key(frame, inner, input)
            }
            ProviderManagementMode::Busy { message } => self.render_status_panel(
                frame,
                inner,
                "Working",
                &format!("{spinner_frame} {message}"),
                Color::Cyan,
                footer_line("[Esc] Back"),
            ),
            ProviderManagementMode::PickingModel { picker, .. } => {
                self.render_model_picker(frame, inner, picker, &spinner_frame)
            }
            ProviderManagementMode::Status { message, is_error } => self.render_status_panel(
                frame,
                inner,
                if *is_error { "Error" } else { "Done" },
                message,
                if *is_error { Color::Red } else { Color::Green },
                footer_line("[Any key] Continue"),
            ),
        }
    }

    #[cfg(test)]
    fn new_for_tests(entries: Vec<ProviderEntry>) -> Self {
        Self::new_for_tests_with(
            entries,
            CredentialStore::with_config("anie-test", None).without_native_keyring(),
            PathBuf::from("/tmp/config.toml"),
        )
    }

    #[cfg(test)]
    fn new_for_tests_with(
        entries: Vec<ProviderEntry>,
        credential_store: CredentialStore,
        write_target: PathBuf,
    ) -> Self {
        let (worker_tx, worker_rx) = mpsc::unbounded_channel();
        Self {
            providers: entries,
            selected: 0,
            mode: ProviderManagementMode::Table,
            credential_store,
            test_results: HashMap::new(),
            worker_tx,
            worker_rx,
            spinner: Spinner::new(),
            write_target,
        }
    }

    fn handle_worker_event(&mut self, event: WorkerEvent) -> ProviderManagementAction {
        match event {
            WorkerEvent::TestCompleted {
                row_index,
                provider,
                result,
            } => {
                self.test_results.insert(row_index, result.clone());
                self.mode = match result {
                    TestResult::Success { latency_ms } => ProviderManagementMode::Status {
                        message: format!("Provider '{provider}' is healthy ({latency_ms}ms)."),
                        is_error: false,
                    },
                    TestResult::Failed { error } => ProviderManagementMode::Status {
                        message: format!("Provider '{provider}' failed validation: {error}"),
                        is_error: true,
                    },
                    TestResult::Pending => ProviderManagementMode::Table,
                };
            }
            WorkerEvent::ApiKeySaved { provider, result } => match result {
                Ok(()) => {
                    if let Some(entry) = self
                        .providers
                        .iter_mut()
                        .find(|entry| entry.name == provider)
                    {
                        entry.has_credential = true;
                    }
                    if let Some(index) = self.provider_index(&provider) {
                        self.test_results.remove(&index);
                    }
                    self.mode = ProviderManagementMode::Status {
                        message: format!("Saved a new API key for '{provider}'."),
                        is_error: false,
                    };
                }
                Err(error) => {
                    self.mode = ProviderManagementMode::Status {
                        message: error,
                        is_error: true,
                    };
                }
            },
            WorkerEvent::ModelsDiscovered { entry, result } => match result {
                Ok(models) => {
                    if let ProviderManagementMode::PickingModel {
                        entry: active_entry,
                        picker,
                    } = &mut self.mode
                        && active_entry.name == entry.name
                    {
                        picker.set_models(models);
                    } else {
                        self.mode = ProviderManagementMode::PickingModel {
                            picker: ModelPickerPane::new(
                                models,
                                entry.name.clone(),
                                entry.default_model.clone(),
                                None,
                            ),
                            entry,
                        };
                    }
                }
                Err(error) => {
                    if let ProviderManagementMode::PickingModel {
                        entry: active_entry,
                        picker,
                    } = &mut self.mode
                        && active_entry.name == entry.name
                    {
                        picker.set_loading(false);
                        picker.set_error(Some(error));
                    } else {
                        self.mode = ProviderManagementMode::Status {
                            message: format!("Could not load models: {error}"),
                            is_error: true,
                        };
                    }
                }
            },
        }
        ProviderManagementAction::Continue
    }

    fn handle_table_key(&mut self, key: KeyEvent) -> ProviderManagementAction {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) | (KeyModifiers::NONE, KeyCode::Char('q')) => {
                ProviderManagementAction::Close
            }
            (KeyModifiers::NONE, KeyCode::Up) | (KeyModifiers::NONE, KeyCode::Char('k')) => {
                self.selected = self.selected.saturating_sub(1);
                ProviderManagementAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Down) | (KeyModifiers::NONE, KeyCode::Char('j')) => {
                self.selected = (self.selected + 1).min(self.providers.len().saturating_sub(1));
                ProviderManagementAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                self.mode = ProviderManagementMode::ActionMenu { selected: 0 };
                ProviderManagementAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Char('t')) => {
                self.spawn_test_for_selected();
                ProviderManagementAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Char('d')) => {
                self.mode = ProviderManagementMode::ConfirmDelete;
                ProviderManagementAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Char('e')) => {
                self.mode = ProviderManagementMode::EditApiKey {
                    input: TextField::masked(),
                };
                ProviderManagementAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Char('s')) => self.set_selected_as_default(),
            _ => ProviderManagementAction::Continue,
        }
    }

    fn handle_action_menu_key(&mut self, key: KeyEvent) -> ProviderManagementAction {
        let items = action_items();
        let ProviderManagementMode::ActionMenu { selected } = &mut self.mode else {
            return ProviderManagementAction::Continue;
        };

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.mode = ProviderManagementMode::Table;
            }
            (KeyModifiers::NONE, KeyCode::Up) | (KeyModifiers::NONE, KeyCode::Char('k')) => {
                *selected = selected.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Down) | (KeyModifiers::NONE, KeyCode::Char('j')) => {
                *selected = (*selected + 1).min(items.len().saturating_sub(1));
            }
            (KeyModifiers::NONE, KeyCode::Enter) => match items.get(*selected).copied() {
                Some(ActionItem::TestConnection) => {
                    self.mode = ProviderManagementMode::Table;
                    self.spawn_test_for_selected();
                }
                Some(ActionItem::ViewModels) => {
                    self.mode = ProviderManagementMode::Busy {
                        message: format!(
                            "Loading models for '{}'…",
                            self.selected_provider_name().unwrap_or_default()
                        ),
                    };
                    self.spawn_model_discovery_for_selected();
                }
                Some(ActionItem::EditApiKey) => {
                    self.mode = ProviderManagementMode::EditApiKey {
                        input: TextField::masked(),
                    };
                }
                Some(ActionItem::SetAsDefault) => {
                    self.mode = ProviderManagementMode::Table;
                    return self.set_selected_as_default();
                }
                Some(ActionItem::DeleteProvider) => {
                    self.mode = ProviderManagementMode::ConfirmDelete;
                }
                None => {}
            },
            _ => {}
        }
        ProviderManagementAction::Continue
    }

    fn handle_confirm_delete_key(&mut self, key: KeyEvent) -> ProviderManagementAction {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('y')) => self.delete_selected_provider(),
            (KeyModifiers::NONE, KeyCode::Char('n')) | (KeyModifiers::NONE, KeyCode::Esc) => {
                self.mode = ProviderManagementMode::Table;
                ProviderManagementAction::Continue
            }
            _ => ProviderManagementAction::Continue,
        }
    }

    fn handle_edit_key_input(&mut self, key: KeyEvent) -> ProviderManagementAction {
        let ProviderManagementMode::EditApiKey { input } = &mut self.mode else {
            return ProviderManagementAction::Continue;
        };

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.mode = ProviderManagementMode::Table;
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                let api_key = input.trimmed();
                if api_key.is_empty() {
                    self.mode = ProviderManagementMode::Status {
                        message: "API key cannot be empty.".to_string(),
                        is_error: true,
                    };
                } else {
                    self.spawn_save_api_key(api_key);
                }
            }
            _ => input.handle_edit_key(key),
        }
        ProviderManagementAction::Continue
    }

    fn handle_busy_key(&mut self, key: KeyEvent) -> ProviderManagementAction {
        if matches!(
            (key.modifiers, key.code),
            (KeyModifiers::NONE, KeyCode::Esc)
        ) {
            self.mode = ProviderManagementMode::Table;
        }
        ProviderManagementAction::Continue
    }

    fn handle_model_picker_key(&mut self, key: KeyEvent) -> ProviderManagementAction {
        let action = match &mut self.mode {
            ProviderManagementMode::PickingModel { picker, .. } => picker.handle_key(key),
            _ => return ProviderManagementAction::Continue,
        };

        match action {
            ModelPickerAction::Continue => {}
            ModelPickerAction::Cancelled => {
                self.mode = ProviderManagementMode::ActionMenu { selected: 1 };
            }
            ModelPickerAction::Refresh => {
                let entry = match &mut self.mode {
                    ProviderManagementMode::PickingModel { entry, picker } => {
                        picker.set_loading(true);
                        picker.set_error(None);
                        entry.clone()
                    }
                    _ => return ProviderManagementAction::Continue,
                };
                self.spawn_model_discovery(entry);
            }
            ModelPickerAction::Selected(model_info) => {
                let entry = match &self.mode {
                    ProviderManagementMode::PickingModel { entry, .. } => entry.clone(),
                    _ => return ProviderManagementAction::Continue,
                };
                return self.apply_selected_model(entry, model_info);
            }
        }
        ProviderManagementAction::Continue
    }

    fn render_table(&self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(1)])
            .split(area);

        let header = Row::new(vec![
            Cell::from("Provider"),
            Cell::from("Type"),
            Cell::from("Model"),
            Cell::from("Key"),
            Cell::from("Status"),
        ])
        .style(Style::default().add_modifier(Modifier::BOLD));

        let rows = self
            .providers
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let status = match self.test_results.get(&index) {
                    Some(TestResult::Pending) => "testing…".to_string(),
                    Some(TestResult::Success { latency_ms }) => format!("ok ({latency_ms}ms)"),
                    Some(TestResult::Failed { error }) => truncate_text(error, 16),
                    None => "untested".to_string(),
                };
                let style = if index == self.selected {
                    Style::default().bg(Color::DarkGray).fg(Color::White)
                } else {
                    Style::default()
                };
                Row::new(vec![
                    Cell::from(if entry.is_default {
                        format!("* {}", entry.name)
                    } else {
                        entry.name.clone()
                    }),
                    Cell::from(match entry.provider_type {
                        ProviderType::Local => "Local",
                        ProviderType::ApiKey => "API",
                        ProviderType::Custom => "Custom",
                    }),
                    Cell::from(entry.default_model.clone()),
                    Cell::from(if entry.has_credential { "●" } else { "─" }),
                    Cell::from(status),
                ])
                .style(style)
            })
            .collect::<Vec<_>>();

        Table::new(
            rows,
            [
                Constraint::Length(18),
                Constraint::Length(8),
                Constraint::Length(22),
                Constraint::Length(4),
                Constraint::Min(12),
            ],
        )
        .header(header)
        .column_spacing(1)
        .render(chunks[0], frame.buffer_mut());

        Paragraph::new(footer_line(
            "[↑↓] Navigate   [Enter] Actions   [t] Test   [e] Edit Key   [s] Set Default   [d] Delete   [q] Close",
        ))
        .wrap(Wrap { trim: false })
        .render(chunks[1], frame.buffer_mut());
    }

    fn render_action_menu(&self, frame: &mut Frame<'_>, area: Rect, selected: usize) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(area);
        Paragraph::new(format!(
            "Actions for '{}'",
            self.selected_provider_name().unwrap_or_default()
        ))
        .alignment(Alignment::Center)
        .render(chunks[0], frame.buffer_mut());

        let items = action_items()
            .iter()
            .map(|item| {
                ListItem::new(match item {
                    ActionItem::TestConnection => "Test connection",
                    ActionItem::ViewModels => "View models",
                    ActionItem::EditApiKey => "Edit API key",
                    ActionItem::SetAsDefault => "Set as default",
                    ActionItem::DeleteProvider => "Delete provider",
                })
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        state.select(Some(selected));
        frame.render_stateful_widget(
            List::new(items).highlight_symbol("› ").highlight_style(
                Style::default()
                    .fg(Color::White)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            chunks[1],
            &mut state,
        );
        Paragraph::new(footer_line("[↑↓] Navigate   [Enter] Select   [Esc] Back"))
            .render(chunks[2], frame.buffer_mut());
    }

    fn render_edit_api_key(&self, frame: &mut Frame<'_>, area: Rect, input: &TextField) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);
        Paragraph::new(format!(
            "Update the API key for '{}'.",
            self.selected_provider_name().unwrap_or_default()
        ))
        .render(chunks[0], frame.buffer_mut());
        let block = Block::default().borders(Borders::ALL).title("API Key");
        let inner = block.inner(chunks[1]);
        block.render(chunks[1], frame.buffer_mut());
        Paragraph::new(input.render_value())
            .style(Style::default().fg(Color::White))
            .render(inner, frame.buffer_mut());
        frame.set_cursor_position((
            inner.x + input.cursor_x().min(inner.width.saturating_sub(1)),
            inner.y,
        ));
        Paragraph::new(footer_line("[Enter] Save   [Esc] Cancel"))
            .render(chunks[2], frame.buffer_mut());
    }

    fn render_model_picker(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        picker: &ModelPickerPane,
        spinner_frame: &str,
    ) {
        let cursor = picker.render(area, frame.buffer_mut(), spinner_frame);
        frame.set_cursor_position(cursor);
    }

    fn render_status_panel(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        title: &str,
        body: &str,
        color: Color,
        footer: Line<'static>,
    ) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area);
        Paragraph::new(Line::from(vec![Span::styled(
            title.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )]))
        .alignment(Alignment::Center)
        .render(chunks[0], frame.buffer_mut());
        Paragraph::new(body)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .render(chunks[1], frame.buffer_mut());
        Paragraph::new(footer)
            .alignment(Alignment::Center)
            .render(chunks[2], frame.buffer_mut());
    }

    fn selected_provider_name(&self) -> Option<&str> {
        self.providers
            .get(self.selected)
            .map(|entry| entry.name.as_str())
    }

    fn selected_entry(&self) -> Option<&ProviderEntry> {
        self.providers.get(self.selected)
    }

    fn provider_index(&self, provider: &str) -> Option<usize> {
        self.providers
            .iter()
            .position(|entry| entry.name == provider)
    }

    fn replace_provider_entries(&mut self, providers: Vec<ProviderEntry>) {
        self.providers = providers;
        self.selected = self.selected.min(self.providers.len().saturating_sub(1));
        self.test_results.clear();
    }

    fn spawn_test_for_selected(&mut self) {
        let row_index = self.selected;
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.test_results.insert(row_index, TestResult::Pending);
        self.mode = ProviderManagementMode::Busy {
            message: format!("Testing '{}'…", entry.name),
        };

        let tx = self.worker_tx.clone();
        let credential_store = self.credential_store.clone();
        if tokio::runtime::Handle::try_current().is_err() {
            let _ = tx.send(WorkerEvent::TestCompleted {
                row_index,
                provider: entry.name,
                result: TestResult::Failed {
                    error: "provider testing requires an async runtime".to_string(),
                },
            });
            return;
        }

        tokio::spawn(async move {
            let result = test_provider(entry.clone(), credential_store).await;
            let _ = tx.send(WorkerEvent::TestCompleted {
                row_index,
                provider: entry.name,
                result,
            });
        });
    }

    fn spawn_save_api_key(&mut self, api_key: String) {
        let Some(provider_name) = self.selected_provider_name().map(str::to_string) else {
            return;
        };
        self.mode = ProviderManagementMode::Busy {
            message: format!("Saving API key for '{provider_name}'…"),
        };
        let tx = self.worker_tx.clone();
        let credential_store = self.credential_store.clone();
        if tokio::runtime::Handle::try_current().is_err() {
            let _ = tx.send(WorkerEvent::ApiKeySaved {
                provider: provider_name,
                result: Err("saving API keys requires an async runtime".to_string()),
            });
            return;
        }

        tokio::spawn(async move {
            let result = credential_store
                .set(&provider_name, &api_key)
                .map_err(|error| format!("failed to save credential: {error}"));
            let _ = tx.send(WorkerEvent::ApiKeySaved {
                provider: provider_name,
                result,
            });
        });
    }

    fn spawn_model_discovery_for_selected(&mut self) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.spawn_model_discovery(entry);
    }

    fn spawn_model_discovery(&self, entry: ProviderEntry) {
        let tx = self.worker_tx.clone();
        let credential_store = self.credential_store.clone();
        if tokio::runtime::Handle::try_current().is_err() {
            let _ = tx.send(WorkerEvent::ModelsDiscovered {
                entry,
                result: Err("model discovery requires an async runtime".to_string()),
            });
            return;
        }

        tokio::spawn(async move {
            let result = discover_models_for_entry(&entry, &credential_store).await;
            let _ = tx.send(WorkerEvent::ModelsDiscovered { entry, result });
        });
    }

    fn apply_selected_model(
        &mut self,
        entry: ProviderEntry,
        model_info: ModelInfo,
    ) -> ProviderManagementAction {
        match self.write_selected_model(&entry, &model_info) {
            Ok((provider, model_id, resolved_model)) => {
                for provider_entry in &mut self.providers {
                    provider_entry.is_default = provider_entry.name == provider;
                    if provider_entry.name == provider {
                        provider_entry.default_model = model_id.clone();
                    }
                }
                self.mode = ProviderManagementMode::Status {
                    message: format!(
                        "Saved default model for '{}' to {}.",
                        provider,
                        self.write_target.display()
                    ),
                    is_error: false,
                };
                ProviderManagementAction::ConfigChanged {
                    provider: Some(provider),
                    model: Some(model_id),
                    resolved_model: Some(resolved_model),
                    message: format!("Saved configuration to {}", self.write_target.display()),
                }
            }
            Err(error) => {
                self.mode = ProviderManagementMode::Status {
                    message: format!("Could not update default model: {error}"),
                    is_error: true,
                };
                ProviderManagementAction::Continue
            }
        }
    }

    fn set_selected_as_default(&mut self) -> ProviderManagementAction {
        let Some(entry) = self.selected_entry().cloned() else {
            return ProviderManagementAction::Continue;
        };
        match self.write_default_selection(&entry.name, &entry.default_model) {
            Ok((provider, model)) => {
                for provider_entry in &mut self.providers {
                    provider_entry.is_default = provider_entry.name == provider;
                }
                self.mode = ProviderManagementMode::Status {
                    message: format!(
                        "Set '{}' as the default provider in {}.",
                        provider,
                        self.write_target.display()
                    ),
                    is_error: false,
                };
                ProviderManagementAction::ConfigChanged {
                    provider: Some(provider),
                    model: Some(model),
                    resolved_model: None,
                    message: format!("Saved configuration to {}", self.write_target.display()),
                }
            }
            Err(error) => {
                self.mode = ProviderManagementMode::Status {
                    message: format!("Could not update default provider: {error}"),
                    is_error: true,
                };
                ProviderManagementAction::Continue
            }
        }
    }

    fn delete_selected_provider(&mut self) -> ProviderManagementAction {
        let Some(entry) = self.selected_entry().cloned() else {
            return ProviderManagementAction::Continue;
        };

        let outcome = (|| -> Result<(Option<String>, Option<String>)> {
            self.credential_store.delete(&entry.name)?;
            let mut mutator = ConfigMutator::load_or_create(&self.write_target)?;
            mutator.remove_provider(&entry.name);
            mutator.save()?;

            let remaining = self
                .providers
                .iter()
                .filter(|provider| provider.name != entry.name)
                .cloned()
                .collect::<Vec<_>>();
            self.replace_provider_entries(remaining);

            if let Some(new_default) = self.providers.first().cloned() {
                let (provider, model) =
                    self.write_default_selection(&new_default.name, &new_default.default_model)?;
                for provider_entry in &mut self.providers {
                    provider_entry.is_default = provider_entry.name == provider;
                }
                Ok((Some(provider), Some(model)))
            } else {
                Ok((None, None))
            }
        })();

        match outcome {
            Ok((provider, model)) => {
                self.mode = ProviderManagementMode::Status {
                    message: format!("Deleted provider '{}'.", entry.name),
                    is_error: false,
                };
                ProviderManagementAction::ConfigChanged {
                    provider,
                    model,
                    resolved_model: None,
                    message: format!("Saved configuration to {}", self.write_target.display()),
                }
            }
            Err(error) => {
                self.mode = ProviderManagementMode::Status {
                    message: format!("Could not delete provider '{}': {error}", entry.name),
                    is_error: true,
                };
                ProviderManagementAction::Continue
            }
        }
    }

    fn write_default_selection(&self, provider: &str, model: &str) -> Result<(String, String)> {
        let mut mutator = ConfigMutator::load_or_create(&self.write_target)?;
        mutator.set_default_model(provider, model);
        mutator.save()?;
        Ok((provider.to_string(), model.to_string()))
    }

    fn write_selected_model(
        &self,
        entry: &ProviderEntry,
        model_info: &ModelInfo,
    ) -> Result<(String, String, Model)> {
        let resolved_model = model_info_to_provider_model(entry, model_info);
        let mut mutator = ConfigMutator::load_or_create(&self.write_target)?;
        if !matches!(entry.provider_type, ProviderType::ApiKey) {
            mutator.upsert_provider(
                &entry.name,
                Some(resolved_model.base_url.as_str()),
                Some(resolved_model.api),
            );
            mutator.upsert_provider_model(&entry.name, &resolved_model);
        }
        mutator.set_default_model(&entry.name, &resolved_model.id);
        mutator.save()?;
        Ok((
            entry.name.clone(),
            resolved_model.id.clone(),
            resolved_model,
        ))
    }
}

fn action_items() -> &'static [ActionItem] {
    &[
        ActionItem::TestConnection,
        ActionItem::ViewModels,
        ActionItem::EditApiKey,
        ActionItem::SetAsDefault,
        ActionItem::DeleteProvider,
    ]
}

fn load_provider_entries(
    write_target: &std::path::Path,
    project_config_path: Option<&std::path::Path>,
    credential_store: &CredentialStore,
) -> Result<Vec<ProviderEntry>> {
    let global_config = global_config_path();
    let config = load_config_with_paths(
        global_config.as_deref(),
        project_config_path,
        CliOverrides::default(),
    )?;
    let has_any_config = global_config
        .as_deref()
        .is_some_and(std::path::Path::exists)
        || project_config_path.is_some_and(std::path::Path::exists)
        || write_target.is_file();

    let stored_credentials = credential_store.list_providers();
    let mut provider_names = config.providers.keys().cloned().collect::<Vec<_>>();
    if has_any_config && !provider_names.contains(&config.model.provider) {
        provider_names.push(config.model.provider.clone());
    }
    for provider in stored_credentials {
        if !provider_names.contains(&provider) {
            provider_names.push(provider);
        }
    }
    provider_names.sort();
    provider_names.dedup();

    let builtin_catalog = builtin_models();
    let mut entries = provider_names
        .into_iter()
        .map(|name| {
            let provider_config = config.providers.get(&name);
            let builtin_default = builtin_catalog.iter().find(|model| model.provider == name);
            let default_model = if has_any_config && config.model.provider == name {
                config.model.id.clone()
            } else {
                provider_config
                    .and_then(|provider| provider.models.first())
                    .map(|model| model.id.clone())
                    .or_else(|| builtin_default.map(|model| model.id.clone()))
                    .unwrap_or_else(|| "unknown".to_string())
            };
            let base_url = provider_config
                .and_then(|provider| provider.base_url.clone())
                .or_else(|| builtin_default.map(|model| model.base_url.clone()));
            let provider_type = classify_provider_type(&name, base_url.as_deref());
            ProviderEntry {
                has_credential: credential_store.get(&name).is_some(),
                is_default: has_any_config && config.model.provider == name,
                name,
                provider_type,
                base_url,
                api_key_env: provider_config.and_then(|provider| provider.api_key_env.clone()),
                default_model,
            }
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| (!entry.is_default, entry.name.clone()));
    Ok(entries)
}

fn classify_provider_type(provider: &str, base_url: Option<&str>) -> ProviderType {
    let provider = provider.to_ascii_lowercase();
    let is_local = matches!(provider.as_str(), "ollama" | "lmstudio" | "vllm")
        || base_url.is_some_and(is_local_base_url);
    if is_local {
        ProviderType::Local
    } else if matches!(provider.as_str(), "openai" | "anthropic") {
        ProviderType::ApiKey
    } else {
        ProviderType::Custom
    }
}

fn is_local_base_url(base_url: &str) -> bool {
    let base_url = base_url.to_ascii_lowercase();
    base_url.starts_with("http://localhost")
        || base_url.starts_with("https://localhost")
        || base_url.starts_with("http://127.0.0.1")
        || base_url.starts_with("https://127.0.0.1")
        || base_url.starts_with("http://[::1]")
        || base_url.starts_with("https://[::1]")
}

async fn test_provider(entry: ProviderEntry, credential_store: CredentialStore) -> TestResult {
    let started = Instant::now();
    let model = model_for_entry(&entry);
    let api_key =
        resolve_provider_api_key(&entry.name, entry.api_key_env.as_deref(), &credential_store);
    match validate_provider_connection(&model, api_key.as_deref()).await {
        Ok(()) => TestResult::Success {
            latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        },
        Err(error) => TestResult::Failed { error },
    }
}

fn model_for_entry(entry: &ProviderEntry) -> Model {
    let api = if entry.name == "anthropic" {
        ApiKind::AnthropicMessages
    } else {
        ApiKind::OpenAICompletions
    };
    let base_url = entry
        .base_url
        .clone()
        .unwrap_or_else(|| match entry.name.as_str() {
            "anthropic" => "https://api.anthropic.com".to_string(),
            "openai" => "https://api.openai.com/v1".to_string(),
            _ => "http://localhost:11434/v1".to_string(),
        });
    Model {
        id: entry.default_model.clone(),
        name: entry.default_model.clone(),
        provider: entry.name.clone(),
        api,
        base_url,
        context_window: 32_768,
        max_tokens: 8_192,
        supports_reasoning: false,
        reasoning_capabilities: None,
        supports_images: false,
        cost_per_million: CostPerMillion::zero(),
    }
}

fn model_info_to_provider_model(entry: &ProviderEntry, info: &ModelInfo) -> Model {
    let model = model_for_entry(entry);
    let mut resolved = info.to_model(model.api, &model.base_url);
    resolved.provider = entry.name.clone();
    resolved
}

fn resolve_provider_api_key(
    provider_name: &str,
    api_key_env: Option<&str>,
    credential_store: &CredentialStore,
) -> Option<String> {
    credential_store.get(provider_name).or_else(|| {
        api_key_env
            .and_then(|name| std::env::var(name).ok())
            .or_else(|| {
                default_provider_env(provider_name).and_then(|name| std::env::var(name).ok())
            })
    })
}

fn default_provider_env(provider_name: &str) -> Option<&'static str> {
    match provider_name {
        "openai" => Some("OPENAI_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        _ => None,
    }
}

async fn discover_models_for_entry(
    entry: &ProviderEntry,
    credential_store: &CredentialStore,
) -> Result<Vec<ModelInfo>, String> {
    let model = model_for_entry(entry);
    let request = ModelDiscoveryRequest {
        provider_name: entry.name.clone(),
        api: model.api,
        base_url: model.base_url,
        api_key: resolve_provider_api_key(
            &entry.name,
            entry.api_key_env.as_deref(),
            credential_store,
        ),
        headers: HashMap::new(),
    };
    discover_models(&request)
        .await
        .map_err(|error| error.to_string())
}

async fn validate_provider_connection(model: &Model, api_key: Option<&str>) -> Result<(), String> {
    match model.api {
        ApiKind::AnthropicMessages => {
            validate_anthropic_endpoint(&model.base_url, model.id.as_str(), api_key).await
        }
        _ => validate_openai_compatible_endpoint(&model.base_url, api_key).await,
    }
}

async fn validate_openai_compatible_endpoint(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| format!("failed to create HTTP client: {error}"))?;
    let url = format!(
        "{}/models",
        normalize_openai_base_url(base_url).trim_end_matches('/')
    );
    let mut request = client.get(url);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }

    let response = request
        .send()
        .await
        .map_err(|error| format!("connection failed: {error}"))?;
    if response.status().is_success() {
        return Ok(());
    }
    Err(format!(
        "endpoint returned {} {}",
        response.status().as_u16(),
        response.status().canonical_reason().unwrap_or("error")
    ))
}

async fn validate_anthropic_endpoint(
    base_url: &str,
    model_id: &str,
    api_key: Option<&str>,
) -> Result<(), String> {
    let api_key = api_key.ok_or_else(|| "Anthropic API key is required.".to_string())?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| format!("failed to create HTTP client: {error}"))?;
    let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));
    let response = client
        .post(url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": model_id,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "ping"}]
        }))
        .send()
        .await
        .map_err(|error| format!("connection failed: {error}"))?;
    if response.status().is_success() {
        return Ok(());
    }
    Err(format!(
        "endpoint returned {} {}",
        response.status().as_u16(),
        response.status().canonical_reason().unwrap_or("error")
    ))
}

fn normalize_openai_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let truncated = text
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_default: bool) -> ProviderEntry {
        ProviderEntry {
            name: name.to_string(),
            provider_type: ProviderType::ApiKey,
            base_url: Some("https://api.example.com/v1".to_string()),
            api_key_env: None,
            default_model: "model".to_string(),
            has_credential: true,
            is_default,
        }
    }

    #[test]
    fn table_navigation_moves_selection() {
        let mut screen = ProviderManagementScreen::new_for_tests(vec![
            entry("openai", true),
            entry("anthropic", false),
        ]);
        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(screen.selected, 1);
        screen.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(screen.selected, 0);
    }

    #[test]
    fn enter_opens_action_menu() {
        let mut screen = ProviderManagementScreen::new_for_tests(vec![entry("openai", true)]);
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            screen.mode,
            ProviderManagementMode::ActionMenu { .. }
        ));
    }

    #[test]
    fn test_action_marks_provider_pending() {
        let mut screen = ProviderManagementScreen::new_for_tests(vec![entry("openai", true)]);
        screen.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(screen.test_results.get(&0), Some(&TestResult::Pending));
    }

    #[test]
    fn test_result_keyed_by_row_index() {
        let mut screen = ProviderManagementScreen::new_for_tests(vec![
            entry("openai", true),
            entry("anthropic", false),
        ]);
        screen.selected = 1;
        screen.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert_eq!(screen.test_results.get(&1), Some(&TestResult::Pending));
        assert!(!screen.test_results.contains_key(&0));
    }

    #[test]
    fn delete_confirmation_cancels_with_n() {
        let mut screen = ProviderManagementScreen::new_for_tests(vec![entry("openai", true)]);
        screen.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(matches!(screen.mode, ProviderManagementMode::ConfirmDelete));
        screen.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(screen.mode, ProviderManagementMode::Table));
        assert_eq!(screen.providers.len(), 1);
    }

    #[test]
    fn edit_key_opens_masked_input() {
        let mut screen = ProviderManagementScreen::new_for_tests(vec![entry("openai", true)]);
        screen.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        let ProviderManagementMode::EditApiKey { input } = &screen.mode else {
            panic!("expected edit api key mode");
        };
        assert!(input.masked);
    }

    #[test]
    fn set_default_returns_config_changed_action() {
        let mut screen = ProviderManagementScreen::new_for_tests(vec![entry("openai", true)]);
        let action = screen.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(matches!(
            action,
            ProviderManagementAction::Continue | ProviderManagementAction::ConfigChanged { .. }
        ));
    }

    #[test]
    fn escape_closes_screen() {
        let mut screen = ProviderManagementScreen::new_for_tests(vec![entry("openai", true)]);
        let action = screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(action, ProviderManagementAction::Close);
    }

    #[test]
    fn test_provider_transitions_to_busy_then_status() {
        let mut screen = ProviderManagementScreen::new_for_tests(vec![entry("openai", true)]);
        screen.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(matches!(screen.mode, ProviderManagementMode::Busy { .. }));
        assert_eq!(screen.test_results.get(&0), Some(&TestResult::Pending));

        // Outside a tokio runtime, spawn_test_for_selected synchronously
        // sends a TestCompleted(Failed) event; handle_tick processes it.
        let action = screen.handle_tick();
        assert_eq!(action, ProviderManagementAction::Continue);
        assert!(matches!(
            screen.mode,
            ProviderManagementMode::Status { is_error: true, .. }
        ));
        assert!(matches!(
            screen.test_results.get(&0),
            Some(TestResult::Failed { .. })
        ));
    }

    #[test]
    fn list_reload_clears_test_results() {
        let mut screen = ProviderManagementScreen::new_for_tests(vec![entry("openai", true)]);
        screen.test_results.insert(0, TestResult::Pending);
        screen.replace_provider_entries(vec![entry("anthropic", false)]);
        assert!(screen.test_results.is_empty());
    }

    #[test]
    fn delete_provider_removes_row() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let write_target = tempdir.path().join("config.toml");
        let json_fallback = tempdir.path().join("auth.json");
        let credential_store =
            CredentialStore::with_config("anie-test", Some(json_fallback)).without_native_keyring();
        let mut screen = ProviderManagementScreen::new_for_tests_with(
            vec![entry("openai", true), entry("anthropic", false)],
            credential_store,
            write_target,
        );

        screen.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(matches!(screen.mode, ProviderManagementMode::ConfirmDelete));

        let action = screen.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(matches!(
            action,
            ProviderManagementAction::ConfigChanged { .. }
        ));
        assert_eq!(screen.providers.len(), 1);
        assert_eq!(screen.providers[0].name, "anthropic");
    }

    #[test]
    fn deleting_a_row_clears_test_results() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let write_target = tempdir.path().join("config.toml");
        let json_fallback = tempdir.path().join("auth.json");
        let credential_store =
            CredentialStore::with_config("anie-test", Some(json_fallback)).without_native_keyring();
        let mut screen = ProviderManagementScreen::new_for_tests_with(
            vec![entry("openai", true), entry("anthropic", false)],
            credential_store,
            write_target,
        );
        screen
            .test_results
            .insert(1, TestResult::Success { latency_ms: 12 });

        screen.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        let _ = screen.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

        assert!(screen.test_results.is_empty());
    }

    #[tokio::test]
    async fn edit_api_key_stores_new_key_via_credential_store() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let json_fallback = tempdir.path().join("auth.json");
        let credential_store =
            CredentialStore::with_config("anie-test", Some(json_fallback.clone()))
                .without_native_keyring();
        let mut screen = ProviderManagementScreen::new_for_tests_with(
            vec![entry("openai", true)],
            credential_store.clone(),
            tempdir.path().join("config.toml"),
        );

        screen.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        for c in "sk-test".chars() {
            screen.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(screen.mode, ProviderManagementMode::Busy { .. }));

        // Let the tokio::spawn task write the credential, then drain
        // the worker channel.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        screen.handle_tick();

        assert_eq!(
            credential_store.get("openai").as_deref(),
            Some("sk-test"),
            "credential store should have the new key",
        );
        assert!(matches!(
            screen.mode,
            ProviderManagementMode::Status {
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn view_models_opens_busy_then_picker_on_discovery() {
        let mut screen = ProviderManagementScreen::new_for_tests(vec![entry("openai", true)]);
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // Action menu second item is ViewModels.
        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(screen.mode, ProviderManagementMode::Busy { .. }));

        // Inject a successful discovery to open the picker.
        let selected = screen.selected_entry().cloned().expect("selected entry");
        screen.handle_worker_event(WorkerEvent::ModelsDiscovered {
            entry: selected,
            result: Ok(vec![ModelInfo {
                id: "gpt-4o".into(),
                name: "GPT-4o".into(),
                provider: "openai".into(),
                context_length: Some(128_000),
                supports_images: Some(true),
                supports_reasoning: Some(false),
            }]),
        });
        assert!(matches!(
            screen.mode,
            ProviderManagementMode::PickingModel { .. }
        ));
    }
}
