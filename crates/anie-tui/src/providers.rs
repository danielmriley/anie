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
    AnieConfig, CliOverrides, ConfigMutator, global_config_path, load_config_with_paths,
};
use anie_provider::{ApiKind, CostPerMillion, Model};
use anie_providers_builtin::builtin_models;

use crate::Spinner;

/// A configured provider shown in the management screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderEntry {
    pub name: String,
    pub provider_type: ProviderType,
    pub base_url: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderManagementAction {
    Continue,
    Close,
    ConfigChanged {
        provider: Option<String>,
        model: Option<String>,
        message: String,
    },
}

#[derive(Debug, Clone)]
enum ProviderManagementMode {
    Table,
    ActionMenu { selected: usize },
    ConfirmDelete,
    EditApiKey { input: TextField },
    Busy { message: String },
    Status { message: String, is_error: bool },
}

#[derive(Debug)]
enum WorkerEvent {
    TestCompleted {
        provider: String,
        result: TestResult,
    },
    ApiKeySaved {
        provider: String,
        result: Result<(), String>,
    },
}

#[derive(Debug, Clone, Copy)]
enum ActionItem {
    TestConnection,
    EditApiKey,
    SetAsDefault,
    DeleteProvider,
}

#[derive(Debug, Clone, Default)]
struct TextField {
    value: String,
    cursor: usize,
    masked: bool,
}

/// Provider-management overlay widget.
pub struct ProviderManagementScreen {
    providers: Vec<ProviderEntry>,
    selected: usize,
    mode: ProviderManagementMode,
    credential_store: CredentialStore,
    test_results: HashMap<String, TestResult>,
    worker_tx: mpsc::UnboundedSender<WorkerEvent>,
    worker_rx: mpsc::UnboundedReceiver<WorkerEvent>,
    spinner: Spinner,
    global_config_path: PathBuf,
}

impl ProviderManagementScreen {
    /// Create a management screen using the global config and default credential store.
    pub fn new() -> Result<Self> {
        let global_config_path = global_config_path().context("home directory is not available")?;
        let credential_store = CredentialStore::new();
        Self::with_config_path(global_config_path, credential_store)
    }

    fn with_config_path(
        global_config_path: PathBuf,
        credential_store: CredentialStore,
    ) -> Result<Self> {
        let providers = load_provider_entries(&global_config_path, &credential_store)?;
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
            global_config_path,
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

        let mode = self.mode.clone();
        let spinner_frame = self.spinner.tick().to_string();
        match mode {
            ProviderManagementMode::Table => self.render_table(frame, inner),
            ProviderManagementMode::ActionMenu { selected } => {
                self.render_action_menu(frame, inner, selected)
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
                self.render_edit_api_key(frame, inner, &input)
            }
            ProviderManagementMode::Busy { message } => self.render_status_panel(
                frame,
                inner,
                "Working",
                &format!("{} {message}", spinner_frame),
                Color::Cyan,
                footer_line("[Esc] Back"),
            ),
            ProviderManagementMode::Status { message, is_error } => self.render_status_panel(
                frame,
                inner,
                if is_error { "Error" } else { "Done" },
                &message,
                if is_error { Color::Red } else { Color::Green },
                footer_line("[Any key] Continue"),
            ),
        }
    }

    #[cfg(test)]
    fn new_for_tests(entries: Vec<ProviderEntry>) -> Self {
        let (worker_tx, worker_rx) = mpsc::unbounded_channel();
        Self {
            providers: entries,
            selected: 0,
            mode: ProviderManagementMode::Table,
            credential_store: CredentialStore::with_config("anie-test", None)
                .without_native_keyring(),
            test_results: HashMap::new(),
            worker_tx,
            worker_rx,
            spinner: Spinner::new(),
            global_config_path: PathBuf::from("/tmp/config.toml"),
        }
    }

    fn handle_worker_event(&mut self, event: WorkerEvent) -> ProviderManagementAction {
        match event {
            WorkerEvent::TestCompleted { provider, result } => {
                self.test_results.insert(provider.clone(), result.clone());
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
                    self.test_results.remove(&provider);
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
                let status = match self.test_results.get(&entry.name) {
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

    fn spawn_test_for_selected(&mut self) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.test_results
            .insert(entry.name.clone(), TestResult::Pending);
        self.mode = ProviderManagementMode::Busy {
            message: format!("Testing '{}'…", entry.name),
        };

        let tx = self.worker_tx.clone();
        let credential_store = self.credential_store.clone();
        if tokio::runtime::Handle::try_current().is_err() {
            let _ = tx.send(WorkerEvent::TestCompleted {
                provider: entry.name.clone(),
                result: TestResult::Failed {
                    error: "provider testing requires an async runtime".to_string(),
                },
            });
            return;
        }

        tokio::spawn(async move {
            let result = test_provider(entry.clone(), credential_store).await;
            let _ = tx.send(WorkerEvent::TestCompleted {
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
            message: format!("Saving API key for '{}'…", provider_name),
        };
        let tx = self.worker_tx.clone();
        let credential_store = self.credential_store.clone();
        if tokio::runtime::Handle::try_current().is_err() {
            let _ = tx.send(WorkerEvent::ApiKeySaved {
                provider: provider_name.clone(),
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
                    message: format!("Set '{}' as the default provider.", provider),
                    is_error: false,
                };
                ProviderManagementAction::ConfigChanged {
                    provider: Some(provider),
                    model: Some(model),
                    message: "Default provider updated.".to_string(),
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
            let mut mutator = ConfigMutator::load_or_create(&self.global_config_path)?;
            mutator.remove_provider(&entry.name);
            mutator.save()?;

            self.providers
                .retain(|provider| provider.name != entry.name);
            self.selected = self.selected.min(self.providers.len().saturating_sub(1));
            self.test_results.remove(&entry.name);

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
                    message: format!("Deleted provider '{}'.", entry.name),
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
        let mut mutator = ConfigMutator::load_or_create(&self.global_config_path)?;
        mutator.set_default_model(provider, model);
        mutator.save()?;
        Ok((provider.to_string(), model.to_string()))
    }
}

fn action_items() -> &'static [ActionItem] {
    &[
        ActionItem::TestConnection,
        ActionItem::EditApiKey,
        ActionItem::SetAsDefault,
        ActionItem::DeleteProvider,
    ]
}

fn load_provider_entries(
    config_path: &PathBuf,
    credential_store: &CredentialStore,
) -> Result<Vec<ProviderEntry>> {
    let has_global_config = config_path.is_file();
    let config = if has_global_config {
        load_config_with_paths(Some(config_path.as_path()), None, CliOverrides::default())?
    } else {
        AnieConfig::default()
    };

    let stored_credentials = credential_store.list_providers();
    let mut provider_names = config.providers.keys().cloned().collect::<Vec<_>>();
    if has_global_config && !provider_names.contains(&config.model.provider) {
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
            let default_model = if has_global_config && config.model.provider == name {
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
                is_default: has_global_config && config.model.provider == name,
                name,
                provider_type,
                base_url,
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
    let api_key = credential_store.get(&entry.name);
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

impl TextField {
    fn masked() -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            masked: true,
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent) {
        if let KeyCode::Char(ch) = key.code
            && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
        {
            self.value.insert(self.cursor, ch);
            self.cursor += ch.len_utf8();
            return;
        }

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Backspace) => {
                if let Some(previous) = previous_boundary(&self.value, self.cursor) {
                    self.value.drain(previous..self.cursor);
                    self.cursor = previous;
                }
            }
            (KeyModifiers::NONE, KeyCode::Delete) => {
                if let Some(next) = next_boundary(&self.value, self.cursor) {
                    self.value.drain(self.cursor..next);
                }
            }
            (KeyModifiers::NONE, KeyCode::Left) => {
                if let Some(previous) = previous_boundary(&self.value, self.cursor) {
                    self.cursor = previous;
                }
            }
            (KeyModifiers::NONE, KeyCode::Right) => {
                if let Some(next) = next_boundary(&self.value, self.cursor) {
                    self.cursor = next;
                }
            }
            _ => {}
        }
    }

    fn render_value(&self) -> String {
        if self.masked {
            "•".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        }
    }

    fn cursor_x(&self) -> u16 {
        u16::try_from(self.value[..self.cursor].chars().count()).unwrap_or(u16::MAX)
    }

    fn trimmed(&self) -> String {
        self.value.trim().to_string()
    }
}

fn centered_rect(
    area: Rect,
    max_width_pct: u16,
    max_height_pct: u16,
    min_width: u16,
    min_height: u16,
) -> Rect {
    let width = ((area.width as u32 * max_width_pct as u32) / 100)
        .max(min_width as u32)
        .min(area.width as u32) as u16;
    let height = ((area.height as u32 * max_height_pct as u32) / 100)
        .max(min_height as u32)
        .min(area.height as u32) as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

fn footer_line(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(Color::DarkGray),
    ))
}

fn previous_boundary(text: &str, index: usize) -> Option<usize> {
    if index == 0 {
        return None;
    }
    text[..index]
        .char_indices()
        .last()
        .map(|(position, _)| position)
}

fn next_boundary(text: &str, index: usize) -> Option<usize> {
    if index >= text.len() {
        return None;
    }
    text[index..]
        .char_indices()
        .nth(1)
        .map(|(offset, _)| index + offset)
        .or(Some(text.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, is_default: bool) -> ProviderEntry {
        ProviderEntry {
            name: name.to_string(),
            provider_type: ProviderType::ApiKey,
            base_url: Some("https://api.example.com/v1".to_string()),
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
        assert_eq!(
            screen.test_results.get("openai"),
            Some(&TestResult::Pending)
        );
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
}
