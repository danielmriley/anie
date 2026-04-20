use std::{path::Path, time::Duration};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Widget, Wrap},
};
use tokio::sync::mpsc;

use anie_auth::CredentialStore;
use anie_config::{ConfigMutator, global_config_path};
use anie_provider::{ApiKind, CostPerMillion, Model, ModelCompat, ModelInfo};
use anie_providers_builtin::{
    LocalServer, ModelDiscoveryRequest, builtin_models, detect_local_servers, discover_models,
};

use crate::{
    ModelPickerAction, ModelPickerPane, ProviderManagementAction, ProviderManagementScreen, Spinner,
};

/// A provider configured during onboarding.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfiguredProvider {
    /// The model selected as the default for this provider.
    pub model: Model,
    /// How this provider should be persisted into config.
    pub kind: ConfiguredProviderKind,
    /// Whether this provider should become the active default selection.
    pub is_default: bool,
}

/// How a configured provider should be written into config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfiguredProviderKind {
    /// Built-in hosted provider; do not write custom model metadata.
    BuiltinHosted,
    /// Config-backed provider; persist base URL, API kind, and model metadata.
    ConfigBacked,
}

/// Data returned when onboarding finishes.
#[derive(Debug, Clone, PartialEq)]
pub struct OnboardingCompletion {
    /// Providers that still need to be written into config by the caller.
    pub providers: Vec<ConfiguredProvider>,
    /// A config reload target for changes that were already persisted during onboarding.
    pub reload_target: Option<(Option<String>, Option<String>)>,
}

/// Actions emitted by the onboarding screen.
#[derive(Debug, Clone, PartialEq)]
pub enum OnboardingAction {
    /// Keep running the onboarding screen.
    Continue,
    /// Onboarding completed successfully.
    Complete(OnboardingCompletion),
    /// User cancelled onboarding.
    Cancelled,
}

#[derive(Debug, Clone)]
enum OnboardingState {
    MainMenu {
        selected: usize,
    },
    ManagingProviders,
    LocalServerWaiting,
    LocalServerSelect {
        selected: usize,
    },
    NoLocalServers,
    ProviderPresetList {
        selected: usize,
    },
    ApiKeyInput {
        preset_index: usize,
        input: TextField,
    },
    CustomEndpoint {
        form: CustomEndpointForm,
    },
    Busy {
        title: String,
        message: String,
        return_to: Box<OnboardingState>,
    },
    DiscoveringModels {
        context: ModelPickerContext,
        message: String,
    },
    PickingModel {
        context: ModelPickerContext,
        picker: ModelPickerPane,
    },
    Success {
        message: String,
    },
    Error {
        message: String,
        return_to: Box<OnboardingState>,
    },
    /// Sentinel state used during take-and-replace transitions.
    ///
    /// This should never survive past the current event-handler call.
    Transient,
}

#[derive(Debug, Clone)]
enum LocalDetectionState {
    Pending,
    Ready(Vec<LocalServer>),
}

#[derive(Debug, Clone, PartialEq)]
enum ModelPickerContext {
    LocalServer {
        selected: usize,
        server: LocalServer,
    },
    ApiPreset {
        preset_index: usize,
        preset: ProviderPreset,
        api_key: String,
    },
    CustomEndpoint {
        form_snapshot: CustomEndpointForm,
        api_key: String,
        base_url: String,
        provider_name: String,
    },
}

#[derive(Debug)]
enum WorkerEvent {
    LocalServersDetected(Vec<LocalServer>),
    Progress(String),
    PresetValidated {
        context: ModelPickerContext,
        result: Result<(), String>,
    },
    CustomEndpointValidated {
        context: ModelPickerContext,
        result: Result<(), String>,
    },
    ModelsDiscovered {
        context: ModelPickerContext,
        result: Result<Vec<ModelInfo>, String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
struct ProviderPreset {
    display_name: &'static str,
    provider_name: &'static str,
    kind: ConfiguredProviderKind,
    model: Model,
}

#[derive(Debug, Clone, PartialEq)]
struct CustomEndpointForm {
    base_url: TextField,
    provider_name: TextField,
    model_id: TextField,
    api_key: TextField,
    selected_field: usize,
}

use crate::widgets::{TextField, centered_rect, footer_line};

/// Full-screen onboarding widget.
pub struct OnboardingScreen {
    state: OnboardingState,
    credential_store: CredentialStore,
    configured_providers: Vec<ConfiguredProvider>,
    local_detection: LocalDetectionState,
    worker_tx: mpsc::UnboundedSender<WorkerEvent>,
    worker_rx: mpsc::UnboundedReceiver<WorkerEvent>,
    spinner: Spinner,
    provider_management: Option<ProviderManagementScreen>,
    persisted_reload_target: Option<(Option<String>, Option<String>)>,
}

impl crate::overlay::OverlayScreen for OnboardingScreen {
    fn dispatch_key(&mut self, key: KeyEvent) -> crate::overlay::OverlayOutcome {
        crate::overlay::OverlayOutcome::Onboarding(self.handle_key(key))
    }

    fn dispatch_tick(&mut self) -> crate::overlay::OverlayOutcome {
        crate::overlay::OverlayOutcome::Onboarding(self.handle_tick())
    }

    fn dispatch_render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.render(frame, area);
    }
}

impl OnboardingScreen {
    /// Create a new onboarding screen.
    #[must_use]
    pub fn new(credential_store: CredentialStore) -> Self {
        let (worker_tx, worker_rx) = mpsc::unbounded_channel();
        let mut screen = Self {
            state: OnboardingState::MainMenu { selected: 0 },
            credential_store,
            configured_providers: Vec::new(),
            local_detection: LocalDetectionState::Pending,
            worker_tx,
            worker_rx,
            spinner: Spinner::new(),
            provider_management: None,
            persisted_reload_target: None,
        };
        screen.start_local_detection();
        screen
    }

    /// Return providers configured so far.
    #[must_use]
    pub fn configured_providers(&self) -> &[ConfiguredProvider] {
        &self.configured_providers
    }

    /// Handle a key press.
    pub fn handle_key(&mut self, key: KeyEvent) -> OnboardingAction {
        if matches!(self.state, OnboardingState::Transient) {
            debug_assert!(false, "Transient state leaked into handle_key");
            return OnboardingAction::Continue;
        }
        if matches!(self.state, OnboardingState::Success { .. }) {
            self.state = OnboardingState::MainMenu { selected: 0 };
            return OnboardingAction::Continue;
        }
        if matches!(self.state, OnboardingState::Error { .. }) {
            self.restore_return_state();
            return OnboardingAction::Continue;
        }

        match &mut self.state {
            OnboardingState::MainMenu { .. } => self.handle_main_menu_key(key),
            OnboardingState::ManagingProviders => self.handle_provider_management_key(key),
            OnboardingState::LocalServerWaiting => self.handle_local_waiting_key(key),
            OnboardingState::LocalServerSelect { .. } => self.handle_local_select_key(key),
            OnboardingState::NoLocalServers => self.handle_no_local_servers_key(key),
            OnboardingState::ProviderPresetList { .. } => self.handle_provider_preset_key(key),
            OnboardingState::ApiKeyInput { .. } => self.handle_api_key_input_key(key),
            OnboardingState::CustomEndpoint { .. } => self.handle_custom_endpoint_key(key),
            OnboardingState::Busy { .. } => self.handle_busy_key(key),
            OnboardingState::DiscoveringModels { .. } => self.handle_busy_key(key),
            OnboardingState::PickingModel { .. } => self.handle_model_picker_key(key),
            OnboardingState::Success { .. }
            | OnboardingState::Error { .. }
            | OnboardingState::Transient => OnboardingAction::Continue,
        }
    }

    /// Poll background workers and update screen state.
    pub fn handle_tick(&mut self) -> OnboardingAction {
        if matches!(self.state, OnboardingState::Transient) {
            debug_assert!(false, "Transient state leaked into handle_tick");
            return OnboardingAction::Continue;
        }

        if matches!(self.state, OnboardingState::ManagingProviders)
            && let Some(screen) = &mut self.provider_management
        {
            match screen.handle_tick() {
                ProviderManagementAction::Continue => {}
                ProviderManagementAction::Close => {
                    self.provider_management = None;
                    self.state = OnboardingState::MainMenu { selected: 3 };
                }
                ProviderManagementAction::ConfigChanged {
                    provider, model, ..
                } => {
                    self.persisted_reload_target = Some((provider, model));
                }
            }
        }

        let mut action = OnboardingAction::Continue;
        while let Ok(event) = self.worker_rx.try_recv() {
            action = self.handle_worker_event(event);
            if !matches!(action, OnboardingAction::Continue) {
                return action;
            }
        }
        action
    }

    /// Render the onboarding screen into the provided area.
    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        Clear.render(area, frame.buffer_mut());
        let popup = centered_rect(area, 90, 80, 20, 18);
        let block = Block::default()
            .title(Line::from(vec![Span::styled(
                " Welcome to Anie — First Run ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        let inner = block.inner(popup);
        block.render(popup, frame.buffer_mut());

        let spinner_frame = self.spinner.tick().to_string();
        match &self.state {
            OnboardingState::MainMenu { selected } => {
                self.render_main_menu(frame, inner, *selected)
            }
            OnboardingState::ManagingProviders => {
                if let Some(screen) = &mut self.provider_management {
                    screen.render(frame, frame.area());
                }
            }
            OnboardingState::LocalServerWaiting => self.render_busy_panel(
                frame,
                inner,
                "Local Servers",
                &format!("{spinner_frame} Scanning for local servers…"),
                footer_line("[Esc] Back"),
            ),
            OnboardingState::LocalServerSelect { selected } => {
                self.render_local_server_select(frame, inner, *selected, &spinner_frame)
            }
            OnboardingState::NoLocalServers => self.render_busy_panel(
                frame,
                inner,
                "Local Servers",
                "No local model servers were detected. Use the custom endpoint flow to add one.",
                footer_line("[Esc] Back   [Enter] Custom Endpoint"),
            ),
            OnboardingState::ProviderPresetList { selected } => {
                self.render_provider_presets(frame, inner, *selected)
            }
            OnboardingState::ApiKeyInput {
                preset_index,
                input,
            } => self.render_api_key_input(frame, inner, *preset_index, input),
            OnboardingState::CustomEndpoint { form } => {
                self.render_custom_endpoint(frame, inner, form)
            }
            OnboardingState::Busy { title, message, .. } => self.render_busy_panel(
                frame,
                inner,
                title,
                &format!("{spinner_frame} {message}"),
                footer_line("[Esc] Back"),
            ),
            OnboardingState::DiscoveringModels { message, .. } => self.render_busy_panel(
                frame,
                inner,
                "Model Discovery",
                &format!("{spinner_frame} {message}"),
                footer_line("[Esc] Back"),
            ),
            OnboardingState::PickingModel { picker, .. } => {
                self.render_model_picker(frame, inner, picker, &spinner_frame)
            }
            OnboardingState::Success { message } => self.render_status_panel(
                frame,
                inner,
                "Success",
                message,
                Color::Green,
                footer_line("[Any key] Continue"),
            ),
            OnboardingState::Error { message, .. } => self.render_status_panel(
                frame,
                inner,
                "Error",
                message,
                Color::Red,
                footer_line("[Any key] Back"),
            ),
            OnboardingState::Transient => {
                debug_assert!(false, "Transient state leaked into render");
            }
        }
    }

    #[cfg(test)]
    fn new_for_tests() -> Self {
        let (worker_tx, worker_rx) = mpsc::unbounded_channel();
        Self {
            state: OnboardingState::MainMenu { selected: 0 },
            credential_store: CredentialStore::with_config("anie-test", None)
                .without_native_keyring(),
            configured_providers: Vec::new(),
            local_detection: LocalDetectionState::Ready(Vec::new()),
            worker_tx,
            worker_rx,
            spinner: Spinner::new(),
            provider_management: None,
            persisted_reload_target: None,
        }
    }

    #[cfg(test)]
    fn with_local_servers_for_tests(servers: Vec<LocalServer>) -> Self {
        let mut screen = Self::new_for_tests();
        screen.local_detection = LocalDetectionState::Ready(servers);
        screen
    }

    fn take_state_for_return_to(&mut self) -> Box<OnboardingState> {
        Box::new(std::mem::replace(
            &mut self.state,
            OnboardingState::Transient,
        ))
    }

    fn restore_return_state(&mut self) {
        let state = std::mem::replace(&mut self.state, OnboardingState::Transient);
        match state {
            OnboardingState::Busy { return_to, .. } | OnboardingState::Error { return_to, .. } => {
                self.state = *return_to;
            }
            other => {
                debug_assert!(
                    false,
                    "restore_return_state called outside Busy/Error: {other:?}"
                );
                self.state = other;
            }
        }
    }

    fn start_local_detection(&mut self) {
        if tokio::runtime::Handle::try_current().is_err() {
            self.local_detection = LocalDetectionState::Ready(Vec::new());
            return;
        }

        let tx = self.worker_tx.clone();
        tokio::spawn(async move {
            let servers = detect_local_servers().await;
            let _ = tx.send(WorkerEvent::LocalServersDetected(servers));
        });
    }

    fn handle_worker_event(&mut self, event: WorkerEvent) -> OnboardingAction {
        match event {
            WorkerEvent::LocalServersDetected(servers) => {
                self.local_detection = LocalDetectionState::Ready(servers.clone());
                if matches!(self.state, OnboardingState::LocalServerWaiting) {
                    self.state = if servers.is_empty() {
                        OnboardingState::NoLocalServers
                    } else {
                        OnboardingState::LocalServerSelect { selected: 0 }
                    };
                }
            }
            WorkerEvent::Progress(message) => {
                if let OnboardingState::Busy {
                    message: current, ..
                } = &mut self.state
                {
                    *current = message;
                }
            }
            WorkerEvent::PresetValidated { context, result } => match result {
                Ok(()) => self.start_model_discovery(context),
                Err(error) => {
                    self.state = OnboardingState::Error {
                        message: error,
                        return_to: Box::new(match context {
                            ModelPickerContext::ApiPreset {
                                preset_index,
                                api_key,
                                ..
                            } => OnboardingState::ApiKeyInput {
                                preset_index,
                                input: TextField::masked_with_value(&api_key),
                            },
                            _ => OnboardingState::MainMenu { selected: 1 },
                        }),
                    };
                }
            },
            WorkerEvent::CustomEndpointValidated { context, result } => match result {
                Ok(()) => self.start_model_discovery(context),
                Err(error) => {
                    self.state = OnboardingState::Error {
                        message: error,
                        return_to: Box::new(match context {
                            ModelPickerContext::CustomEndpoint { form_snapshot, .. } => {
                                OnboardingState::CustomEndpoint {
                                    form: form_snapshot,
                                }
                            }
                            _ => OnboardingState::MainMenu { selected: 2 },
                        }),
                    };
                }
            },
            WorkerEvent::ModelsDiscovered { context, result } => {
                self.apply_model_discovery_result(context, result);
            }
        }
        OnboardingAction::Continue
    }

    fn handle_main_menu_key(&mut self, key: KeyEvent) -> OnboardingAction {
        let current_selected = match &self.state {
            OnboardingState::MainMenu { selected } => *selected,
            _ => return OnboardingAction::Continue,
        };
        let item_count = self.main_menu_items().len();

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('q')) | (KeyModifiers::NONE, KeyCode::Esc) => {
                OnboardingAction::Cancelled
            }
            (KeyModifiers::NONE, KeyCode::Up) | (KeyModifiers::NONE, KeyCode::Char('k')) => {
                if let OnboardingState::MainMenu { selected } = &mut self.state {
                    *selected = current_selected.saturating_sub(1);
                }
                OnboardingAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Down) | (KeyModifiers::NONE, KeyCode::Char('j')) => {
                if let OnboardingState::MainMenu { selected } = &mut self.state {
                    *selected = (current_selected + 1).min(item_count.saturating_sub(1));
                }
                OnboardingAction::Continue
            }
            (KeyModifiers::NONE, KeyCode::Enter) => self.activate_main_menu(current_selected),
            _ => OnboardingAction::Continue,
        }
    }

    fn activate_main_menu(&mut self, selected: usize) -> OnboardingAction {
        match self
            .main_menu_items()
            .get(selected)
            .copied()
            .unwrap_or(MainMenuItem::AddApiKey)
        {
            MainMenuItem::LocalServer => {
                self.state = match &self.local_detection {
                    LocalDetectionState::Pending => OnboardingState::LocalServerWaiting,
                    LocalDetectionState::Ready(servers) if servers.is_empty() => {
                        OnboardingState::NoLocalServers
                    }
                    LocalDetectionState::Ready(_) => {
                        OnboardingState::LocalServerSelect { selected: 0 }
                    }
                };
                OnboardingAction::Continue
            }
            MainMenuItem::AddApiKey => {
                self.state = OnboardingState::ProviderPresetList { selected: 0 };
                OnboardingAction::Continue
            }
            MainMenuItem::ManageExisting => match ProviderManagementScreen::new() {
                Ok(screen) => {
                    self.provider_management = Some(screen);
                    self.state = OnboardingState::ManagingProviders;
                    OnboardingAction::Continue
                }
                Err(error) => {
                    self.state = OnboardingState::Error {
                        message: format!("Could not open provider management: {error}"),
                        return_to: Box::new(OnboardingState::MainMenu { selected: 2 }),
                    };
                    OnboardingAction::Continue
                }
            },
            MainMenuItem::CustomEndpoint => {
                self.state = OnboardingState::CustomEndpoint {
                    form: CustomEndpointForm::default(),
                };
                OnboardingAction::Continue
            }
            MainMenuItem::Done => OnboardingAction::Complete(OnboardingCompletion {
                providers: self.configured_providers.clone(),
                reload_target: self.persisted_reload_target.clone(),
            }),
        }
    }

    fn handle_provider_management_key(&mut self, key: KeyEvent) -> OnboardingAction {
        let Some(screen) = &mut self.provider_management else {
            self.state = OnboardingState::MainMenu { selected: 3 };
            return OnboardingAction::Continue;
        };

        match screen.handle_key(key) {
            ProviderManagementAction::Continue => {}
            ProviderManagementAction::Close => {
                self.provider_management = None;
                self.state = OnboardingState::MainMenu { selected: 3 };
            }
            ProviderManagementAction::ConfigChanged {
                provider, model, ..
            } => {
                self.persisted_reload_target = Some((provider, model));
            }
        }
        OnboardingAction::Continue
    }

    fn handle_local_waiting_key(&mut self, key: KeyEvent) -> OnboardingAction {
        if let (KeyModifiers::NONE, KeyCode::Esc) = (key.modifiers, key.code) {
            self.state = OnboardingState::MainMenu { selected: 0 };
        }
        OnboardingAction::Continue
    }

    fn handle_local_select_key(&mut self, key: KeyEvent) -> OnboardingAction {
        let selected_index = match &self.state {
            OnboardingState::LocalServerSelect { selected } => *selected,
            _ => return OnboardingAction::Continue,
        };
        let LocalDetectionState::Ready(servers) = &self.local_detection else {
            self.state = OnboardingState::LocalServerWaiting;
            return OnboardingAction::Continue;
        };

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.state = OnboardingState::MainMenu { selected: 0 };
            }
            (KeyModifiers::NONE, KeyCode::Up) | (KeyModifiers::NONE, KeyCode::Char('k')) => {
                if let OnboardingState::LocalServerSelect { selected } = &mut self.state {
                    *selected = selected.saturating_sub(1);
                }
            }
            (KeyModifiers::NONE, KeyCode::Down) | (KeyModifiers::NONE, KeyCode::Char('j')) => {
                if let OnboardingState::LocalServerSelect { selected } = &mut self.state {
                    *selected = (*selected + 1).min(servers.len().saturating_sub(1));
                }
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if let Some(server) = servers.get(selected_index).cloned() {
                    self.start_model_discovery(ModelPickerContext::LocalServer {
                        selected: selected_index,
                        server,
                    });
                }
            }
            _ => {}
        }
        OnboardingAction::Continue
    }

    fn handle_no_local_servers_key(&mut self, key: KeyEvent) -> OnboardingAction {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.state = OnboardingState::MainMenu { selected: 0 };
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                self.state = OnboardingState::CustomEndpoint {
                    form: CustomEndpointForm::default(),
                };
            }
            _ => {}
        }
        OnboardingAction::Continue
    }

    fn handle_provider_preset_key(&mut self, key: KeyEvent) -> OnboardingAction {
        let OnboardingState::ProviderPresetList { selected } = &mut self.state else {
            return OnboardingAction::Continue;
        };
        let presets = provider_presets();

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.state = OnboardingState::MainMenu { selected: 1 };
            }
            (KeyModifiers::NONE, KeyCode::Up) | (KeyModifiers::NONE, KeyCode::Char('k')) => {
                *selected = selected.saturating_sub(1);
            }
            (KeyModifiers::NONE, KeyCode::Down) | (KeyModifiers::NONE, KeyCode::Char('j')) => {
                *selected = (*selected + 1).min(presets.len().saturating_sub(1));
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                self.state = OnboardingState::ApiKeyInput {
                    preset_index: *selected,
                    input: TextField::masked(),
                };
            }
            _ => {}
        }
        OnboardingAction::Continue
    }

    fn handle_api_key_input_key(&mut self, key: KeyEvent) -> OnboardingAction {
        let (preset_index, api_key_snapshot) = match &self.state {
            OnboardingState::ApiKeyInput {
                preset_index,
                input,
            } => (*preset_index, input.trimmed()),
            _ => return OnboardingAction::Continue,
        };

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.state = OnboardingState::ProviderPresetList {
                    selected: preset_index,
                };
                return OnboardingAction::Continue;
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if api_key_snapshot.is_empty() {
                    let return_to = self.take_state_for_return_to();
                    self.state = OnboardingState::Error {
                        message: "API key cannot be empty.".to_string(),
                        return_to,
                    };
                    return OnboardingAction::Continue;
                }
                let preset = provider_presets()
                    .get(preset_index)
                    .cloned()
                    .unwrap_or_else(default_openai_preset);
                let return_to = self.take_state_for_return_to();
                self.state = OnboardingState::Busy {
                    title: preset.display_name.to_string(),
                    message: "Verifying API key…".to_string(),
                    return_to,
                };
                self.spawn_preset_validation_worker(ModelPickerContext::ApiPreset {
                    preset_index,
                    preset,
                    api_key: api_key_snapshot,
                });
                return OnboardingAction::Continue;
            }
            _ => {
                if let OnboardingState::ApiKeyInput { input, .. } = &mut self.state {
                    input.handle_edit_key(key);
                }
            }
        }

        OnboardingAction::Continue
    }

    fn handle_custom_endpoint_key(&mut self, key: KeyEvent) -> OnboardingAction {
        let form_snapshot = match &self.state {
            OnboardingState::CustomEndpoint { form } => form.clone(),
            _ => return OnboardingAction::Continue,
        };

        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Esc) => {
                self.state = OnboardingState::MainMenu { selected: 2 };
                return OnboardingAction::Continue;
            }
            (KeyModifiers::SHIFT, KeyCode::BackTab)
            | (KeyModifiers::NONE, KeyCode::Up)
            | (KeyModifiers::NONE, KeyCode::BackTab) => {
                if let OnboardingState::CustomEndpoint { form } = &mut self.state {
                    form.selected_field = form.selected_field.saturating_sub(1);
                }
                return OnboardingAction::Continue;
            }
            (KeyModifiers::NONE, KeyCode::Tab) | (KeyModifiers::NONE, KeyCode::Down) => {
                if let OnboardingState::CustomEndpoint { form } = &mut self.state {
                    form.selected_field = (form.selected_field + 1).min(3);
                }
                return OnboardingAction::Continue;
            }
            (KeyModifiers::NONE, KeyCode::Enter) => {
                if form_snapshot.selected_field < 3 {
                    if let OnboardingState::CustomEndpoint { form } = &mut self.state {
                        form.selected_field += 1;
                    }
                    return OnboardingAction::Continue;
                }

                let base_url = form_snapshot.base_url.trimmed();
                let provider_name = if form_snapshot.provider_name.trimmed().is_empty() {
                    "custom".to_string()
                } else {
                    form_snapshot.provider_name.trimmed()
                };
                let api_key = form_snapshot.api_key.trimmed();

                if base_url.is_empty() {
                    let return_to = self.take_state_for_return_to();
                    self.state = OnboardingState::Error {
                        message: "Base URL is required.".to_string(),
                        return_to,
                    };
                    return OnboardingAction::Continue;
                }

                let return_to = self.take_state_for_return_to();
                self.state = OnboardingState::Busy {
                    title: "Custom Endpoint".to_string(),
                    message: "Testing endpoint…".to_string(),
                    return_to,
                };
                self.spawn_custom_validation_worker(ModelPickerContext::CustomEndpoint {
                    form_snapshot,
                    api_key,
                    base_url,
                    provider_name,
                });
                return OnboardingAction::Continue;
            }
            _ => {
                if let OnboardingState::CustomEndpoint { form } = &mut self.state {
                    form.selected_field_mut().handle_edit_key(key);
                }
            }
        }

        OnboardingAction::Continue
    }

    fn handle_busy_key(&mut self, key: KeyEvent) -> OnboardingAction {
        if let (KeyModifiers::NONE, KeyCode::Esc) = (key.modifiers, key.code) {
            match &self.state {
                OnboardingState::Busy { .. } | OnboardingState::Error { .. } => {
                    self.restore_return_state();
                }
                OnboardingState::DiscoveringModels { context, .. } => {
                    self.state = self.return_state_for_context(context);
                }
                _ => {}
            }
        }
        OnboardingAction::Continue
    }

    fn handle_model_picker_key(&mut self, key: KeyEvent) -> OnboardingAction {
        let action = match &mut self.state {
            OnboardingState::PickingModel { picker, .. } => picker.handle_key(key),
            _ => return OnboardingAction::Continue,
        };

        match action {
            ModelPickerAction::Continue => {}
            ModelPickerAction::Cancelled => {
                if let OnboardingState::PickingModel { context, .. } = &self.state {
                    self.state = self.return_state_for_context(context);
                }
            }
            ModelPickerAction::Refresh => {
                let context = match &mut self.state {
                    OnboardingState::PickingModel { context, picker } => {
                        picker.set_loading(true);
                        picker.set_error(None);
                        context.clone()
                    }
                    _ => return OnboardingAction::Continue,
                };
                self.spawn_model_discovery_worker(context);
            }
            ModelPickerAction::Selected(model_info) => {
                let context = match &self.state {
                    OnboardingState::PickingModel { context, .. } => context.clone(),
                    _ => return OnboardingAction::Continue,
                };
                self.finalize_configured_provider(
                    self.configured_provider_from_context(&context, &model_info),
                );
            }
        }

        OnboardingAction::Continue
    }

    fn spawn_preset_validation_worker(&self, context: ModelPickerContext) {
        let tx = self.worker_tx.clone();
        let credential_store = self.credential_store.clone();
        if tokio::runtime::Handle::try_current().is_err() {
            let _ = tx.send(WorkerEvent::PresetValidated {
                context,
                result: Err(
                    "Onboarding requires an async runtime to validate providers.".to_string(),
                ),
            });
            return;
        }

        tokio::spawn(async move {
            let result = validate_preset_context(&context, credential_store, tx.clone()).await;
            let _ = tx.send(WorkerEvent::PresetValidated { context, result });
        });
    }

    fn spawn_custom_validation_worker(&self, context: ModelPickerContext) {
        let tx = self.worker_tx.clone();
        let credential_store = self.credential_store.clone();
        if tokio::runtime::Handle::try_current().is_err() {
            let _ = tx.send(WorkerEvent::CustomEndpointValidated {
                context,
                result: Err(
                    "Onboarding requires an async runtime to validate providers.".to_string(),
                ),
            });
            return;
        }

        tokio::spawn(async move {
            let result = validate_custom_context(&context, credential_store, tx.clone()).await;
            let _ = tx.send(WorkerEvent::CustomEndpointValidated { context, result });
        });
    }

    fn start_model_discovery(&mut self, context: ModelPickerContext) {
        self.state = OnboardingState::DiscoveringModels {
            message: context.discovery_message(),
            context: context.clone(),
        };
        self.spawn_model_discovery_worker(context);
    }

    fn spawn_model_discovery_worker(&self, context: ModelPickerContext) {
        let tx = self.worker_tx.clone();
        if tokio::runtime::Handle::try_current().is_err() {
            let _ = tx.send(WorkerEvent::ModelsDiscovered {
                context,
                result: Err("Onboarding requires an async runtime to discover models.".to_string()),
            });
            return;
        }

        tokio::spawn(async move {
            let result = discover_models_for_context(&context).await;
            let _ = tx.send(WorkerEvent::ModelsDiscovered { context, result });
        });
    }

    fn apply_model_discovery_result(
        &mut self,
        context: ModelPickerContext,
        result: Result<Vec<ModelInfo>, String>,
    ) {
        let is_refresh = matches!(
            &self.state,
            OnboardingState::PickingModel {
                context: active_context,
                ..
            } if *active_context == context
        );

        match result {
            Ok(models) if !models.is_empty() => {
                if is_refresh {
                    if let OnboardingState::PickingModel { picker, .. } = &mut self.state {
                        picker.set_models(models);
                    }
                } else {
                    self.state = OnboardingState::PickingModel {
                        picker: ModelPickerPane::new(
                            models,
                            context.provider_name().to_string(),
                            context.current_model_id().unwrap_or_default(),
                            None,
                        ),
                        context,
                    };
                }
            }
            Ok(_) => {
                let message = "No models were advertised by this endpoint.".to_string();
                if is_refresh {
                    if let OnboardingState::PickingModel { picker, .. } = &mut self.state {
                        picker.set_loading(false);
                        picker.set_error(Some(message));
                    }
                } else {
                    match self.fallback_provider_for_context(&context, &message) {
                        Some((configured, fallback_message)) => self
                            .finalize_configured_provider_with_message(
                                configured,
                                fallback_message,
                            ),
                        None => {
                            self.state = OnboardingState::Error {
                                message,
                                return_to: Box::new(self.return_state_for_context(&context)),
                            };
                        }
                    }
                }
            }
            Err(message) => {
                if is_refresh {
                    if let OnboardingState::PickingModel { picker, .. } = &mut self.state {
                        picker.set_loading(false);
                        picker.set_error(Some(message));
                    }
                } else {
                    match self.fallback_provider_for_context(&context, &message) {
                        Some((configured, fallback_message)) => self
                            .finalize_configured_provider_with_message(
                                configured,
                                fallback_message,
                            ),
                        None => {
                            self.state = OnboardingState::Error {
                                message,
                                return_to: Box::new(self.return_state_for_context(&context)),
                            };
                        }
                    }
                }
            }
        }
    }

    fn return_state_for_context(&self, context: &ModelPickerContext) -> OnboardingState {
        match context {
            ModelPickerContext::LocalServer { selected, .. } => {
                OnboardingState::LocalServerSelect {
                    selected: *selected,
                }
            }
            ModelPickerContext::ApiPreset {
                preset_index,
                api_key,
                ..
            } => OnboardingState::ApiKeyInput {
                preset_index: *preset_index,
                input: TextField::masked_with_value(api_key),
            },
            ModelPickerContext::CustomEndpoint { form_snapshot, .. } => {
                OnboardingState::CustomEndpoint {
                    form: form_snapshot.clone(),
                }
            }
        }
    }

    fn configured_provider_from_context(
        &self,
        context: &ModelPickerContext,
        model_info: &ModelInfo,
    ) -> ConfiguredProvider {
        match context {
            ModelPickerContext::LocalServer { server, .. } => {
                let api = server
                    .models
                    .first()
                    .map(|model| model.api)
                    .unwrap_or(ApiKind::OpenAICompletions);
                let base_url = server
                    .models
                    .first()
                    .map(|model| model.base_url.clone())
                    .unwrap_or_else(|| normalize_openai_base_url(&server.base_url));
                ConfiguredProvider {
                    model: model_info.to_model(api, &base_url),
                    kind: ConfiguredProviderKind::ConfigBacked,
                    is_default: true,
                }
            }
            ModelPickerContext::ApiPreset { preset, .. } => ConfiguredProvider {
                model: model_info.to_model(preset.model.api, &preset.model.base_url),
                kind: preset.kind,
                is_default: true,
            },
            ModelPickerContext::CustomEndpoint {
                base_url,
                provider_name,
                ..
            } => {
                let mut model = model_info.to_model(ApiKind::OpenAICompletions, base_url);
                model.provider = provider_name.clone();
                ConfiguredProvider {
                    model,
                    kind: ConfiguredProviderKind::ConfigBacked,
                    is_default: true,
                }
            }
        }
    }

    fn fallback_provider_for_context(
        &self,
        context: &ModelPickerContext,
        message: &str,
    ) -> Option<(ConfiguredProvider, String)> {
        match context {
            ModelPickerContext::ApiPreset { preset, .. } => {
                let configured = ConfiguredProvider {
                    model: preset.model.clone(),
                    kind: preset.kind,
                    is_default: true,
                };
                Some((
                    configured,
                    format!(
                        "Model discovery failed ({message}). Falling back to {}.",
                        preset.model.id
                    ),
                ))
            }
            ModelPickerContext::CustomEndpoint {
                form_snapshot,
                base_url,
                provider_name,
                ..
            } if !form_snapshot.model_id.trimmed().is_empty() => {
                let manual_model_id = form_snapshot.model_id.trimmed();
                Some((
                    ConfiguredProvider {
                        model: Model {
                            id: manual_model_id.clone(),
                            name: manual_model_id.clone(),
                            provider: provider_name.clone(),
                            api: ApiKind::OpenAICompletions,
                            base_url: normalize_openai_base_url(base_url),
                            context_window: 32_768,
                            max_tokens: 8_192,
                            supports_reasoning: false,
                            reasoning_capabilities: None,
                            supports_images: false,
                            cost_per_million: CostPerMillion::zero(),
                            replay_capabilities: None,
                            compat: ModelCompat::None,
                        },
                        kind: ConfiguredProviderKind::ConfigBacked,
                        is_default: true,
                    },
                    format!(
                        "Model discovery failed ({message}). Using manual model ID {manual_model_id}.",
                    ),
                ))
            }
            _ => None,
        }
    }

    fn finalize_configured_provider(&mut self, configured: ConfiguredProvider) {
        let message = format!(
            "Configured {} with {} as the default model.",
            configured.model.provider, configured.model.id
        );
        self.finalize_configured_provider_with_message(configured, message);
    }

    fn finalize_configured_provider_with_message(
        &mut self,
        configured: ConfiguredProvider,
        message: String,
    ) {
        for provider in &mut self.configured_providers {
            provider.is_default = false;
        }
        if let Some(existing) = self
            .configured_providers
            .iter_mut()
            .find(|provider| provider.model.provider == configured.model.provider)
        {
            *existing = configured;
        } else {
            self.configured_providers.push(configured);
        }
        self.state = OnboardingState::Success { message };
    }

    fn main_menu_items(&self) -> Vec<MainMenuItem> {
        let mut items = vec![MainMenuItem::LocalServer, MainMenuItem::AddApiKey];
        if self.has_existing_providers() {
            items.push(MainMenuItem::ManageExisting);
        }
        items.push(MainMenuItem::CustomEndpoint);
        items.push(MainMenuItem::Done);
        items
    }

    fn has_existing_providers(&self) -> bool {
        self.provider_management.is_some()
            || global_config_path().as_deref().is_some_and(Path::exists)
            || !self.credential_store.list_providers().is_empty()
    }

    fn render_main_menu(&self, frame: &mut Frame<'_>, area: Rect, selected: usize) {
        let items = self
            .main_menu_items()
            .into_iter()
            .map(|item| ListItem::new(item.label(&self.local_detection)))
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        state.select(Some(selected));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(2)])
            .split(area);

        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(Color::White)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        frame.render_stateful_widget(list, chunks[0], &mut state);

        let summary = if self.configured_providers.is_empty() {
            "No providers configured yet."
        } else {
            "Providers configured in this run will be written when you choose Done."
        };
        Paragraph::new(vec![
            Line::from(summary),
            footer_line("[↑↓] Navigate   [Enter] Select   [q] Quit"),
        ])
        .wrap(Wrap { trim: false })
        .render(chunks[1], frame.buffer_mut());
    }

    fn render_local_server_select(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        selected: usize,
        spinner_frame: &str,
    ) {
        let LocalDetectionState::Ready(servers) = &self.local_detection else {
            self.render_busy_panel(
                frame,
                area,
                "Local Servers",
                &format!("{spinner_frame} Scanning for local servers…"),
                footer_line("[Esc] Back"),
            );
            return;
        };

        let items = servers
            .iter()
            .map(|server| {
                let model_summary = server
                    .models
                    .first()
                    .map(|model| model.id.as_str())
                    .unwrap_or("no models");
                ListItem::new(format!(
                    "{} — {} (default: {})",
                    server.name, server.base_url, model_summary
                ))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        state.select(Some(selected));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(6), Constraint::Length(2)])
            .split(area);

        frame.render_stateful_widget(
            List::new(items).highlight_symbol("› ").highlight_style(
                Style::default()
                    .fg(Color::White)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            chunks[0],
            &mut state,
        );
        Paragraph::new(vec![
            Line::from(
                "Choose a detected local server. The first reported model becomes the default.",
            ),
            footer_line("[↑↓] Navigate   [Enter] Configure   [Esc] Back"),
        ])
        .wrap(Wrap { trim: false })
        .render(chunks[1], frame.buffer_mut());
    }

    fn render_provider_presets(&self, frame: &mut Frame<'_>, area: Rect, selected: usize) {
        let presets = provider_presets();
        let items = presets
            .iter()
            .map(|preset| ListItem::new(format!("{} ({})", preset.display_name, preset.model.id)))
            .collect::<Vec<_>>();
        let mut state = ListState::default();
        state.select(Some(selected));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(2)])
            .split(area);

        frame.render_stateful_widget(
            List::new(items).highlight_symbol("› ").highlight_style(
                Style::default()
                    .fg(Color::White)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            chunks[0],
            &mut state,
        );
        Paragraph::new(vec![
            Line::from("Select a provider preset, then enter its API key."),
            footer_line("[↑↓] Navigate   [Enter] Select   [Esc] Back"),
        ])
        .wrap(Wrap { trim: false })
        .render(chunks[1], frame.buffer_mut());
    }

    fn render_api_key_input(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        preset_index: usize,
        input: &TextField,
    ) {
        let preset = provider_presets()
            .get(preset_index)
            .cloned()
            .unwrap_or_else(default_openai_preset);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);

        Paragraph::new(vec![
            Line::from(vec![Span::styled(
                preset.display_name,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(format!("Default model: {}", preset.model.id)),
        ])
        .render(chunks[0], frame.buffer_mut());

        let input_block = Block::default().borders(Borders::ALL).title("API Key");
        let input_area = input_block.inner(chunks[1]);
        input_block.render(chunks[1], frame.buffer_mut());
        Paragraph::new(input.render_value())
            .style(Style::default().fg(Color::White))
            .render(input_area, frame.buffer_mut());
        frame.set_cursor_position((
            input_area.x + input.cursor_x().min(input_area.width.saturating_sub(1)),
            input_area.y,
        ));

        Paragraph::new(vec![
            Line::from("The key is masked while you type."),
            footer_line("[Enter] Verify & Save   [Esc] Back"),
        ])
        .wrap(Wrap { trim: false })
        .render(chunks[3], frame.buffer_mut());
    }

    fn render_custom_endpoint(&self, frame: &mut Frame<'_>, area: Rect, form: &CustomEndpointForm) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);

        Paragraph::new(vec![
            Line::from("Add a custom OpenAI-compatible endpoint."),
            Line::from("Leave API key empty for local providers. Manual Model ID is only used if discovery fails."),
        ])
        .render(chunks[0], frame.buffer_mut());

        self.render_labeled_field(
            frame,
            chunks[1],
            "Base URL",
            &form.base_url,
            form.selected_field == 0,
        );
        self.render_labeled_field(
            frame,
            chunks[2],
            "Provider Name",
            &form.provider_name,
            form.selected_field == 1,
        );
        self.render_labeled_field(
            frame,
            chunks[3],
            "Manual Model ID (fallback)",
            &form.model_id,
            form.selected_field == 2,
        );
        self.render_labeled_field(
            frame,
            chunks[4],
            "API Key",
            &form.api_key,
            form.selected_field == 3,
        );

        let selected_field = form.selected_field_ref();
        let field_area = [chunks[1], chunks[2], chunks[3], chunks[4]][form.selected_field];
        let inner = Block::default().borders(Borders::ALL).inner(field_area);
        frame.set_cursor_position((
            inner.x + selected_field.cursor_x().min(inner.width.saturating_sub(1)),
            inner.y,
        ));

        Paragraph::new(footer_line(
            "[Tab] Next   [Shift+Tab] Back   [Enter] Continue/Submit   [Esc] Back",
        ))
        .render(chunks[6], frame.buffer_mut());
    }

    fn render_labeled_field(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        label: &str,
        field: &TextField,
        focused: bool,
    ) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(label)
            .border_style(if focused {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            });
        let inner = block.inner(area);
        block.render(area, frame.buffer_mut());
        Paragraph::new(field.render_value())
            .style(Style::default().fg(Color::White))
            .render(inner, frame.buffer_mut());
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

    fn render_busy_panel(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        title: &str,
        body: &str,
        footer: Line<'static>,
    ) {
        self.render_status_panel(frame, area, title, body, Color::Cyan, footer);
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
}

#[derive(Debug, Clone, Copy)]
enum MainMenuItem {
    LocalServer,
    AddApiKey,
    ManageExisting,
    CustomEndpoint,
    Done,
}

impl MainMenuItem {
    fn label(self, local_detection: &LocalDetectionState) -> String {
        match self {
            Self::LocalServer => match local_detection {
                LocalDetectionState::Pending => "Configure Local Server (scanning…)".to_string(),
                LocalDetectionState::Ready(servers) if servers.is_empty() => {
                    "Configure Local Server".to_string()
                }
                LocalDetectionState::Ready(servers) => format!(
                    "Configure Local Server ({} detected)",
                    servers
                        .iter()
                        .map(|server| server.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
            Self::AddApiKey => "Add API Key Provider".to_string(),
            Self::ManageExisting => "Manage Existing Providers".to_string(),
            Self::CustomEndpoint => "Advanced / Custom OpenAI-compatible endpoint".to_string(),
            Self::Done => "Done".to_string(),
        }
    }
}

impl Default for CustomEndpointForm {
    fn default() -> Self {
        Self {
            base_url: TextField::default(),
            provider_name: TextField::from("custom"),
            model_id: TextField::default(),
            api_key: TextField::masked(),
            selected_field: 0,
        }
    }
}

impl CustomEndpointForm {
    fn selected_field_mut(&mut self) -> &mut TextField {
        match self.selected_field {
            0 => &mut self.base_url,
            1 => &mut self.provider_name,
            2 => &mut self.model_id,
            _ => &mut self.api_key,
        }
    }

    fn selected_field_ref(&self) -> &TextField {
        match self.selected_field {
            0 => &self.base_url,
            1 => &self.provider_name,
            2 => &self.model_id,
            _ => &self.api_key,
        }
    }
}

impl ModelPickerContext {
    fn provider_name(&self) -> &str {
        match self {
            Self::LocalServer { server, .. } => server.name.as_str(),
            Self::ApiPreset { preset, .. } => preset.provider_name,
            Self::CustomEndpoint { provider_name, .. } => provider_name.as_str(),
        }
    }

    fn current_model_id(&self) -> Option<String> {
        match self {
            Self::LocalServer { server, .. } => server.models.first().map(|model| model.id.clone()),
            Self::ApiPreset { preset, .. } => Some(preset.model.id.clone()),
            Self::CustomEndpoint { form_snapshot, .. } => {
                let model_id = form_snapshot.model_id.trimmed();
                if model_id.is_empty() {
                    None
                } else {
                    Some(model_id)
                }
            }
        }
    }

    fn discovery_message(&self) -> String {
        format!("Discovering models for {}…", self.provider_name())
    }
}

/// Write configured providers into a config file and return the selected default.
pub fn write_configured_providers(
    path: &Path,
    providers: &[ConfiguredProvider],
) -> Result<Option<(String, String)>> {
    if providers.is_empty() {
        return Ok(None);
    }

    let mut mutator = ConfigMutator::load_or_create(path)?;
    for configured in providers {
        match configured.kind {
            ConfiguredProviderKind::BuiltinHosted => {
                mutator.ensure_provider(&configured.model.provider);
            }
            ConfiguredProviderKind::ConfigBacked => {
                mutator.upsert_provider(
                    &configured.model.provider,
                    Some(configured.model.base_url.as_str()),
                    Some(configured.model.api),
                );
                mutator.upsert_provider_model(&configured.model.provider, &configured.model);
            }
        }
    }

    // Safe because `providers` is verified non-empty above (caller asserts;
    // the file rewrites are called after the list is populated).
    #[allow(clippy::expect_used)]
    let default = providers
        .iter()
        .rev()
        .find(|provider| provider.is_default)
        .or_else(|| providers.last())
        .expect("providers should not be empty");
    mutator.set_default_model(&default.model.provider, &default.model.id);
    mutator.save()?;
    Ok(Some((
        default.model.provider.clone(),
        default.model.id.clone(),
    )))
}

async fn validate_preset_context(
    context: &ModelPickerContext,
    credential_store: CredentialStore,
    tx: mpsc::UnboundedSender<WorkerEvent>,
) -> Result<(), String> {
    let ModelPickerContext::ApiPreset {
        preset, api_key, ..
    } = context
    else {
        return Err("invalid preset validation context".to_string());
    };

    validate_provider_connection(&preset.model, Some(api_key.as_str())).await?;
    let _ = tx.send(WorkerEvent::Progress(
        "Waiting for OS keyring authorization…".to_string(),
    ));
    credential_store
        .set(preset.provider_name, api_key)
        .map_err(|error| format!("failed to save credential: {error}"))?;
    Ok(())
}

async fn validate_custom_context(
    context: &ModelPickerContext,
    credential_store: CredentialStore,
    tx: mpsc::UnboundedSender<WorkerEvent>,
) -> Result<(), String> {
    let ModelPickerContext::CustomEndpoint {
        api_key,
        base_url,
        provider_name,
        ..
    } = context
    else {
        return Err("invalid custom endpoint validation context".to_string());
    };

    let normalized_base_url = normalize_openai_base_url(base_url);
    let trimmed_api_key = api_key.trim().to_string();
    let api_key_option = if trimmed_api_key.is_empty() {
        None
    } else {
        Some(trimmed_api_key.as_str())
    };

    validate_openai_compatible_endpoint(&normalized_base_url, api_key_option).await?;
    if !trimmed_api_key.is_empty() {
        let _ = tx.send(WorkerEvent::Progress(
            "Waiting for OS keyring authorization…".to_string(),
        ));
        credential_store
            .set(provider_name, &trimmed_api_key)
            .map_err(|error| format!("failed to save credential: {error}"))?;
    }
    Ok(())
}

async fn discover_models_for_context(
    context: &ModelPickerContext,
) -> Result<Vec<ModelInfo>, String> {
    let request = discovery_request_for_context(context)?;
    discover_models(&request)
        .await
        .map_err(|error| error.to_string())
}

fn discovery_request_for_context(
    context: &ModelPickerContext,
) -> Result<ModelDiscoveryRequest, String> {
    match context {
        ModelPickerContext::LocalServer { server, .. } => {
            let sample = server.models.first();
            Ok(ModelDiscoveryRequest {
                provider_name: server.name.clone(),
                api: sample
                    .map(|model| model.api)
                    .unwrap_or(ApiKind::OpenAICompletions),
                base_url: sample
                    .map(|model| model.base_url.clone())
                    .unwrap_or_else(|| normalize_openai_base_url(&server.base_url)),
                api_key: None,
                headers: std::collections::HashMap::new(),
            })
        }
        ModelPickerContext::ApiPreset {
            preset, api_key, ..
        } => Ok(ModelDiscoveryRequest {
            provider_name: preset.provider_name.to_string(),
            api: preset.model.api,
            base_url: preset.model.base_url.clone(),
            api_key: Some(api_key.clone()),
            headers: std::collections::HashMap::new(),
        }),
        ModelPickerContext::CustomEndpoint {
            api_key,
            base_url,
            provider_name,
            ..
        } => Ok(ModelDiscoveryRequest {
            provider_name: provider_name.clone(),
            api: ApiKind::OpenAICompletions,
            base_url: normalize_openai_base_url(base_url),
            api_key: if api_key.trim().is_empty() {
                None
            } else {
                Some(api_key.clone())
            },
            headers: std::collections::HashMap::new(),
        }),
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
    let url = format!("{}/models", base_url.trim_end_matches('/'));
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

fn provider_presets() -> Vec<ProviderPreset> {
    let openai = builtin_models()
        .into_iter()
        .find(|model| model.provider == "openai")
        .unwrap_or_else(|| Model {
            id: "gpt-4o".to_string(),
            name: "GPT-4o".to_string(),
            provider: "openai".to_string(),
            api: ApiKind::OpenAICompletions,
            base_url: "https://api.openai.com/v1".to_string(),
            context_window: 128_000,
            max_tokens: 16_384,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: true,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        });
    let anthropic = builtin_models()
        .into_iter()
        .find(|model| model.provider == "anthropic")
        .unwrap_or_else(|| Model {
            id: "claude-sonnet-4-6".to_string(),
            name: "Claude Sonnet 4.6".to_string(),
            provider: "anthropic".to_string(),
            api: ApiKind::AnthropicMessages,
            base_url: "https://api.anthropic.com".to_string(),
            context_window: 1_000_000,
            max_tokens: 128_000,
            supports_reasoning: true,
            reasoning_capabilities: None,
            supports_images: true,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        });

    vec![
        ProviderPreset {
            display_name: "Anthropic (Claude)",
            provider_name: "anthropic",
            kind: ConfiguredProviderKind::BuiltinHosted,
            model: anthropic,
        },
        ProviderPreset {
            display_name: "OpenAI (GPT-4o, o-series)",
            provider_name: "openai",
            kind: ConfiguredProviderKind::BuiltinHosted,
            model: openai,
        },
        custom_openai_preset(
            "OpenRouter (discovery, 500+ models)",
            "openrouter",
            "https://openrouter.ai/api/v1",
            "openai/gpt-4o",
        ),
        custom_openai_preset("xAI / Grok", "xai", "https://api.x.ai/v1", "grok-2-1212"),
        custom_openai_preset(
            "Groq",
            "groq",
            "https://api.groq.com/openai/v1",
            "llama-3.3-70b-versatile",
        ),
        custom_openai_preset(
            "Together.ai",
            "together",
            "https://api.together.xyz/v1",
            "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        ),
        custom_openai_preset(
            "Fireworks",
            "fireworks",
            "https://api.fireworks.ai/inference/v1",
            "accounts/fireworks/models/llama-v3p1-70b-instruct",
        ),
        custom_openai_preset(
            "Mistral",
            "mistral",
            "https://api.mistral.ai/v1",
            "mistral-large-latest",
        ),
    ]
}

fn default_openai_preset() -> ProviderPreset {
    provider_presets()
        .into_iter()
        .find(|preset| preset.provider_name == "openai")
        .unwrap_or_else(|| {
            custom_openai_preset("OpenAI", "openai", "https://api.openai.com/v1", "gpt-4o")
        })
}

fn custom_openai_preset(
    display_name: &'static str,
    provider_name: &'static str,
    base_url: &'static str,
    model_id: &'static str,
) -> ProviderPreset {
    ProviderPreset {
        display_name,
        provider_name,
        kind: ConfiguredProviderKind::ConfigBacked,
        model: Model {
            id: model_id.to_string(),
            name: model_id.to_string(),
            provider: provider_name.to_string(),
            api: ApiKind::OpenAICompletions,
            base_url: normalize_openai_base_url(base_url),
            context_window: 128_000,
            max_tokens: 8_192,
            supports_reasoning: false,
            reasoning_capabilities: None,
            supports_images: false,
            cost_per_million: CostPerMillion::zero(),
            replay_capabilities: None,
            compat: ModelCompat::None,
        },
    }
}

fn normalize_openai_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anie_provider::{
        ReasoningCapabilities, ReasoningControlMode, ReasoningOutputMode, ThinkingRequestMode,
    };

    fn sample_local_server() -> LocalServer {
        LocalServer {
            name: "ollama".to_string(),
            base_url: "http://localhost:11434".to_string(),
            models: vec![Model {
                id: "qwen3:32b".to_string(),
                name: "Qwen 3 32B".to_string(),
                provider: "ollama".to_string(),
                api: ApiKind::OpenAICompletions,
                base_url: "http://localhost:11434/v1".to_string(),
                context_window: 32_768,
                max_tokens: 8_192,
                supports_reasoning: true,
                reasoning_capabilities: Some(ReasoningCapabilities {
                    control: Some(ReasoningControlMode::Native),
                    output: Some(ReasoningOutputMode::Separated),
                    tags: None,
                    request_mode: Some(ThinkingRequestMode::ReasoningEffort),
                }),
                supports_images: false,
                cost_per_million: CostPerMillion::zero(),
                replay_capabilities: None,
                compat: ModelCompat::None,
            }],
        }
    }

    #[test]
    fn openrouter_preset_registered_and_in_onboarding_shortlist() {
        let presets = provider_presets();
        let openrouter = presets
            .iter()
            .find(|preset| preset.provider_name == "openrouter")
            .expect("openrouter preset must appear in the onboarding shortlist");

        assert_eq!(openrouter.model.provider, "openrouter");
        assert_eq!(openrouter.model.api, ApiKind::OpenAICompletions);
        assert_eq!(openrouter.model.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(openrouter.kind, ConfiguredProviderKind::ConfigBacked);
        assert!(
            openrouter.display_name.contains("OpenRouter"),
            "display name should call out OpenRouter, got {:?}",
            openrouter.display_name
        );

        // Ordering contract: OpenRouter appears within the first
        // three third-party-API presets the user sees, so the
        // shortlist keeps it prominent alongside Anthropic/OpenAI.
        let openrouter_index = presets
            .iter()
            .position(|preset| preset.provider_name == "openrouter")
            .expect("position");
        assert!(
            openrouter_index < 4,
            "openrouter should be near the top of the shortlist, found at index {openrouter_index}"
        );
    }

    #[test]
    fn openrouter_preset_builds_discovery_request_with_expected_fields() {
        // Mirrors the path taken after the user enters their API
        // key on the OpenRouter preset: the worker builds a
        // `ModelDiscoveryRequest` from the `ApiPreset` context and
        // hands it to `discover_models`. We verify the request is
        // addressed at OpenRouter's `/v1/models` with the user's
        // key.
        let preset = provider_presets()
            .into_iter()
            .find(|preset| preset.provider_name == "openrouter")
            .expect("openrouter preset");

        let context = ModelPickerContext::ApiPreset {
            preset_index: 0,
            preset,
            api_key: "sk-or-example".into(),
        };
        let request =
            discovery_request_for_context(&context).expect("discovery request");
        assert_eq!(request.provider_name, "openrouter");
        assert_eq!(request.api, ApiKind::OpenAICompletions);
        assert_eq!(request.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(request.api_key.as_deref(), Some("sk-or-example"));
    }

    #[test]
    fn main_menu_navigation_moves_selection() {
        let mut screen = OnboardingScreen::new_for_tests();
        assert!(matches!(
            screen.state,
            OnboardingState::MainMenu { selected: 0 }
        ));

        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(
            screen.state,
            OnboardingState::MainMenu { selected: 1 }
        ));
        screen.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(matches!(
            screen.state,
            OnboardingState::MainMenu { selected: 0 }
        ));
    }

    #[test]
    fn selecting_add_api_key_opens_preset_list() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            screen.state,
            OnboardingState::ProviderPresetList { .. }
        ));
    }

    #[test]
    fn selecting_local_server_opens_detected_servers() {
        let mut screen =
            OnboardingScreen::with_local_servers_for_tests(vec![sample_local_server()]);
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            screen.state,
            OnboardingState::LocalServerSelect { selected: 0 }
        ));
    }

    #[test]
    fn local_server_selection_starts_model_discovery() {
        let mut screen =
            OnboardingScreen::with_local_servers_for_tests(vec![sample_local_server()]);
        screen.state = OnboardingState::LocalServerSelect { selected: 0 };
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(
            screen.state,
            OnboardingState::DiscoveringModels { .. }
        ));
    }

    #[test]
    fn models_discovered_success_opens_picker() {
        let mut screen = OnboardingScreen::new_for_tests();
        let context = ModelPickerContext::LocalServer {
            selected: 0,
            server: sample_local_server(),
        };
        screen.handle_worker_event(WorkerEvent::ModelsDiscovered {
            context,
            result: Ok(vec![ModelInfo {
                id: "qwen3:32b".into(),
                name: "Qwen 3 32B".into(),
                provider: "ollama".into(),
                context_length: Some(32_768),
                supports_images: Some(false),
                supports_reasoning: Some(true),
                pricing: None,
                supported_parameters: None,
            }]),
        });

        assert!(matches!(screen.state, OnboardingState::PickingModel { .. }));
    }

    #[test]
    fn preset_selection_opens_masked_key_input() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.state = OnboardingState::ProviderPresetList { selected: 0 };
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let OnboardingState::ApiKeyInput { input, .. } = &screen.state else {
            panic!("expected api key input state");
        };
        assert!(input.masked);
    }

    #[test]
    fn api_key_input_masks_rendered_value() {
        let mut input = TextField::masked();
        input.handle_edit_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        input.handle_edit_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        input.handle_edit_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE));

        assert_eq!(input.value, "sk-");
        assert_eq!(input.render_value(), "•••");
    }

    #[test]
    fn escape_returns_to_previous_state() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.state = OnboardingState::ProviderPresetList { selected: 2 };
        screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            screen.state,
            OnboardingState::MainMenu { selected: 1 }
        ));
    }

    #[test]
    fn main_menu_quit_returns_cancelled() {
        let mut screen = OnboardingScreen::new_for_tests();
        let action = screen.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, OnboardingAction::Cancelled);
    }

    #[test]
    fn picker_cancel_returns_to_previous_state() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.state = OnboardingState::PickingModel {
            context: ModelPickerContext::LocalServer {
                selected: 0,
                server: sample_local_server(),
            },
            picker: ModelPickerPane::new(
                vec![ModelInfo {
                    id: "qwen3:32b".into(),
                    name: "Qwen 3 32B".into(),
                    provider: "ollama".into(),
                    context_length: Some(32_768),
                    supports_images: Some(false),
                    supports_reasoning: Some(true),
                    pricing: None,
                    supported_parameters: None,
                }],
                "ollama".into(),
                "qwen3:32b".into(),
                None,
            ),
        };

        screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            screen.state,
            OnboardingState::LocalServerSelect { selected: 0 }
        ));
    }

    #[test]
    fn provider_success_returns_to_main_menu_after_ack() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.finalize_configured_provider(ConfiguredProvider {
            model: default_openai_preset().model,
            kind: ConfiguredProviderKind::BuiltinHosted,
            is_default: true,
        });

        assert!(matches!(screen.state, OnboardingState::Success { .. }));
        screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            screen.state,
            OnboardingState::MainMenu { selected: 0 }
        ));
    }

    #[test]
    fn done_returns_complete_with_configured_providers() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.configured_providers.push(ConfiguredProvider {
            model: default_openai_preset().model,
            kind: ConfiguredProviderKind::BuiltinHosted,
            is_default: true,
        });
        // "Done" is always the last menu item; its index shifts when
        // "Manage Existing" is conditionally inserted.
        let done_index = screen.main_menu_items().len() - 1;
        screen.state = OnboardingState::MainMenu {
            selected: done_index,
        };

        let action = screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let OnboardingAction::Complete(completion) = action else {
            panic!("expected complete action");
        };
        assert_eq!(completion.providers.len(), 1);
        assert_eq!(completion.providers[0].model.provider, "openai");
        assert_eq!(completion.reload_target, None);
    }

    #[test]
    fn custom_endpoint_discovery_failure_uses_manual_model_id_fallback() {
        let mut screen = OnboardingScreen::new_for_tests();
        let form = CustomEndpointForm {
            base_url: TextField::from("http://localhost:8080"),
            provider_name: TextField::from("custom"),
            model_id: TextField::from("manual-model"),
            ..CustomEndpointForm::default()
        };
        let context = ModelPickerContext::CustomEndpoint {
            form_snapshot: form,
            api_key: String::new(),
            base_url: "http://localhost:8080".into(),
            provider_name: "custom".into(),
        };

        screen.handle_worker_event(WorkerEvent::ModelsDiscovered {
            context,
            result: Err("boom".into()),
        });

        assert!(matches!(screen.state, OnboardingState::Success { .. }));
        assert_eq!(screen.configured_providers.len(), 1);
        assert_eq!(screen.configured_providers[0].model.id, "manual-model");
    }

    #[test]
    fn custom_form_tab_navigation_advances_fields() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.state = OnboardingState::CustomEndpoint {
            form: CustomEndpointForm::default(),
        };

        screen.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let OnboardingState::CustomEndpoint { form } = &screen.state else {
            panic!("expected custom endpoint state");
        };
        assert_eq!(form.selected_field, 1);
    }

    #[test]
    fn local_server_waiting_transitions_when_detection_completes() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.local_detection = LocalDetectionState::Pending;
        screen.state = OnboardingState::LocalServerWaiting;

        screen.handle_worker_event(WorkerEvent::LocalServersDetected(vec![
            sample_local_server(),
        ]));
        assert!(matches!(
            screen.state,
            OnboardingState::LocalServerSelect { selected: 0 }
        ));
    }

    #[test]
    fn local_server_waiting_falls_through_to_no_servers_when_detection_empty() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.local_detection = LocalDetectionState::Pending;
        screen.state = OnboardingState::LocalServerWaiting;

        screen.handle_worker_event(WorkerEvent::LocalServersDetected(Vec::new()));
        assert!(matches!(screen.state, OnboardingState::NoLocalServers));
    }

    #[test]
    fn custom_endpoint_form_fields_accept_input_in_order() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.state = OnboardingState::CustomEndpoint {
            form: CustomEndpointForm::default(),
        };

        for c in "http://localhost:9000".chars() {
            screen.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        screen.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        // provider_name defaults to "custom"; replace by backspacing first.
        for _ in 0.."custom".len() {
            screen.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        for c in "my-provider".chars() {
            screen.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        screen.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        for c in "my-model".chars() {
            screen.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        screen.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        for c in "sk-local".chars() {
            screen.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }

        let OnboardingState::CustomEndpoint { form } = &screen.state else {
            panic!("expected custom endpoint state");
        };
        assert_eq!(form.base_url.value, "http://localhost:9000");
        assert_eq!(form.provider_name.value, "my-provider");
        assert_eq!(form.model_id.value, "my-model");
        assert_eq!(form.api_key.value, "sk-local");
        assert_eq!(form.selected_field, 3);
    }

    #[test]
    fn preset_validation_failure_transitions_to_error_state() {
        let mut screen = OnboardingScreen::new_for_tests();
        let preset = default_openai_preset();
        let context = ModelPickerContext::ApiPreset {
            preset_index: 0,
            preset: preset.clone(),
            api_key: "bad-key".into(),
        };
        screen.state = OnboardingState::Busy {
            title: preset.display_name.to_string(),
            message: "Verifying API key…".into(),
            return_to: Box::new(OnboardingState::ApiKeyInput {
                preset_index: 0,
                input: TextField::masked_with_value("bad-key"),
            }),
        };

        screen.handle_worker_event(WorkerEvent::PresetValidated {
            context,
            result: Err("401 unauthorized".into()),
        });
        assert!(matches!(screen.state, OnboardingState::Error { .. }));
    }

    #[test]
    fn escape_returns_to_main_menu_from_custom_endpoint() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.state = OnboardingState::CustomEndpoint {
            form: CustomEndpointForm::default(),
        };
        screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            screen.state,
            OnboardingState::MainMenu { selected: 2 }
        ));
    }

    #[test]
    fn escape_returns_to_main_menu_from_api_key_input() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.state = OnboardingState::ApiKeyInput {
            preset_index: 1,
            input: TextField::masked(),
        };
        screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        // Esc from ApiKeyInput returns to the preset list, not main menu.
        assert!(matches!(
            screen.state,
            OnboardingState::ProviderPresetList { selected: 1 }
        ));
        screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            screen.state,
            OnboardingState::MainMenu { selected: 1 }
        ));
    }

    #[test]
    fn render_does_not_mutate_state() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.state = OnboardingState::ApiKeyInput {
            preset_index: 0,
            input: TextField::masked_with_value("sk-test"),
        };

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 20)).expect("terminal");
        terminal
            .draw(|frame| screen.render(frame, frame.area()))
            .expect("draw onboarding");

        assert!(matches!(
            &screen.state,
            OnboardingState::ApiKeyInput { preset_index, input }
                if *preset_index == 0 && input.trimmed() == "sk-test"
        ));
    }

    #[test]
    fn busy_transition_restores_previous_state_on_escape() {
        let mut screen = OnboardingScreen::new_for_tests();
        screen.state = OnboardingState::ApiKeyInput {
            preset_index: 0,
            input: TextField::masked_with_value("sk-test"),
        };

        let action = screen.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, OnboardingAction::Continue);
        assert!(matches!(screen.state, OnboardingState::Busy { .. }));

        screen.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            &screen.state,
            OnboardingState::ApiKeyInput { preset_index, input }
                if *preset_index == 0 && input.trimmed() == "sk-test"
        ));
    }

    #[test]
    fn normalize_openai_base_url_adds_v1_when_needed() {
        assert_eq!(
            normalize_openai_base_url("http://localhost:11434"),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            normalize_openai_base_url("http://localhost:11434/v1"),
            "http://localhost:11434/v1"
        );
    }
}
