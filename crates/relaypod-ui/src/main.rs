mod markdown;

use eframe::egui;
use egui::text::{CCursor, CCursorRange};
use egui::{Color32, CornerRadius, Margin, Stroke};
use relaypod_client::watch::{self, TargetPhase};
use relaypod_client::{
    mcp_config_snippet, spawn_runtime, ActiveWorker, ClientCommand, ClientRuntime, WorkspaceState,
};
use relaypod_core::{
    ActivityState, AgentConfig, Channel, CreatedIdentity, Identity, IdentityKind, Message,
    UpdateChannelRequest, UpdateIdentityRequest, DEFAULT_AGENT_CWD,
};

const APP_ICON_PNG: &[u8] = include_bytes!("../../../assets/icon-256.png");

fn load_app_icon() -> Option<egui::IconData> {
    eframe::icon_data::from_png_bytes(APP_ICON_PNG).ok()
}

fn load_logo_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let icon = load_app_icon()?;
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [icon.width as usize, icon.height as usize],
        &icon.rgba,
    );
    Some(ctx.load_texture("relaypod_logo", image, egui::TextureOptions::LINEAR))
}

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "relaypod_client=info,relaypod_ui=info".into()),
        )
        .init();

    match watch::ensure_default_config() {
        Ok((path, true)) => {
            tracing::info!(path = %path.display(), "Initialized default RelayPod config");
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(error = %err, "Failed to initialize default ~/.relaypod directory");
        }
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1180.0, 760.0])
        .with_title("RelayPod");
    if let Some(icon) = load_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "RelayPod",
        options,
        Box::new(|cc| Ok(Box::new(RelayPodApp::new(cc)))),
    )
}

#[derive(Default)]
struct MentionState {
    selected_idx: usize,
    dismissed_span: Option<(usize, String)>,
    last_popup_rect: Option<egui::Rect>,
}

struct AgentEditDraft {
    id: String,
    handle: String,
    runner: String,
    harness: String,
    args: String,
    exec: String,
    cwd: String,
    prompt: String,
    instructions_file: String,
    max_sessions: String,
    auto_ack: bool,
    drain_on_start: bool,
    cooldown_secs: String,
    description: String,
}

impl AgentEditDraft {
    fn new_blank() -> Self {
        Self {
            id: String::new(),
            handle: String::new(),
            runner: "herdr".to_string(),
            harness: String::new(),
            args: String::new(),
            exec: String::new(),
            cwd: DEFAULT_AGENT_CWD.to_string(),
            prompt: String::new(),
            instructions_file: String::new(),
            max_sessions: String::new(),
            auto_ack: false,
            drain_on_start: true,
            cooldown_secs: String::new(),
            description: String::new(),
        }
    }

    fn from_identity(id: &Identity) -> Self {
        let cfg = id.agent_config.as_ref();
        Self {
            id: id.id.clone(),
            handle: id.handle.clone(),
            runner: cfg
                .and_then(|c| c.runner.clone())
                .unwrap_or_else(|| "herdr".to_string()),
            harness: cfg.and_then(|c| c.harness.clone()).unwrap_or_default(),
            args: cfg.and_then(|c| c.args.clone()).unwrap_or_default(),
            exec: cfg.and_then(|c| c.exec.clone()).unwrap_or_default(),
            cwd: cfg.and_then(|c| c.cwd.clone()).unwrap_or_default(),
            prompt: cfg.and_then(|c| c.prompt.clone()).unwrap_or_default(),
            instructions_file: cfg
                .and_then(|c| c.instructions_file.clone())
                .unwrap_or_default(),
            max_sessions: cfg
                .and_then(|c| c.max_sessions)
                .map(|n| n.to_string())
                .unwrap_or_default(),
            auto_ack: cfg.and_then(|c| c.auto_ack).unwrap_or(false),
            drain_on_start: cfg.and_then(|c| c.drain_on_start).unwrap_or(true),
            cooldown_secs: cfg
                .and_then(|c| c.cooldown_secs)
                .filter(|&n| n > 0)
                .map(|n| n.to_string())
                .unwrap_or_default(),
            description: cfg.and_then(|c| c.description.clone()).unwrap_or_default(),
        }
    }

    fn max_sessions_valid(&self) -> bool {
        self.max_sessions.trim().is_empty() || self.max_sessions.trim().parse::<usize>().is_ok()
    }

    fn cooldown_valid(&self) -> bool {
        self.cooldown_secs.trim().is_empty() || self.cooldown_secs.trim().parse::<u64>().is_ok()
    }

    fn to_agent_config(&self, is_create: bool) -> AgentConfig {
        let parsed_max = self
            .max_sessions
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|&n| n > 0);
        let parsed_cooldown = if is_create {
            self.cooldown_secs
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|&n| n > 0)
        } else {
            self.cooldown_secs.trim().parse::<u64>().ok().or(Some(0))
        };
        // On create, omit blank `args` so the server populates default non-interactive harness args.
        let args = if is_create && self.args.trim().is_empty() {
            None
        } else {
            Some(self.args.clone())
        };
        AgentConfig {
            runner: Some(self.runner.clone()),
            harness: Some(self.harness.clone()),
            args,
            exec: Some(self.exec.clone()),
            cwd: Some(self.cwd.clone()),
            prompt: Some(self.prompt.clone()),
            instructions_file: Some(self.instructions_file.clone()),
            max_sessions: parsed_max,
            auto_ack: Some(self.auto_ack),
            drain_on_start: Some(self.drain_on_start),
            cooldown_secs: parsed_cooldown,
            description: Some(self.description.clone()),
        }
    }

    fn render_fields(&mut self, ui: &mut egui::Ui, id_prefix: &str, is_create: bool) {
        let cooldown_valid = self.cooldown_valid();
        let max_sessions_valid = self.max_sessions_valid();

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Runner");
            egui::ComboBox::from_id_salt(format!("{id_prefix}_runner"))
                .selected_text(&self.runner)
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.runner, "herdr".to_string(), "herdr");
                    ui.selectable_value(&mut self.runner, "exec".to_string(), "exec");
                });
        });

        ui.label("Harness");
        ui.add(
            egui::TextEdit::singleline(&mut self.harness)
                .hint_text("claude, codex, gemini, agy, pi...")
                .desired_width(f32::INFINITY),
        );
        ui.label("Description");
        ui.add(
            egui::TextEdit::singleline(&mut self.description)
                .hint_text("Role / capabilities for routing")
                .desired_width(f32::INFINITY),
        );
        ui.label(if is_create {
            "Args (leave empty for harness default)"
        } else {
            "Args (passed to harness on new sessions)"
        });
        ui.add(
            egui::TextEdit::singleline(&mut self.args)
                .hint_text("e.g. --model claude-opus-4-6")
                .desired_width(f32::INFINITY),
        );
        ui.label("Working directory (cwd)");
        ui.add(
            egui::TextEdit::singleline(&mut self.cwd)
                .hint_text(DEFAULT_AGENT_CWD)
                .desired_width(f32::INFINITY),
        );

        ui.add_space(4.0);
        egui::CollapsingHeader::new("Advanced options")
            .id_salt(format!("{id_prefix}_advanced"))
            .show(ui, |ui| {
                if self.runner != "herdr" {
                    ui.label("Custom exec command (optional)");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.exec)
                            .hint_text("Only needed for custom harnesses")
                            .desired_width(f32::INFINITY),
                    );
                }
                ui.horizontal(|ui| {
                    ui.label("Cooldown (secs):");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cooldown_secs)
                            .hint_text("0")
                            .desired_width(64.0),
                    );
                    ui.label("Max sessions:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.max_sessions)
                            .hint_text("∞")
                            .desired_width(64.0),
                    );
                });
                if !cooldown_valid {
                    ui.colored_label(
                        Color32::LIGHT_RED,
                        "Cooldown must be a non-negative integer.",
                    );
                }
                if !max_sessions_valid {
                    ui.colored_label(
                        Color32::LIGHT_RED,
                        "Max sessions must be a non-negative integer.",
                    );
                }
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.auto_ack, "Auto-ack on exit 0");
                    ui.checkbox(&mut self.drain_on_start, "Drain unread on start");
                });
                ui.label("Instructions file");
                ui.add(
                    egui::TextEdit::singleline(&mut self.instructions_file)
                        .hint_text("~/.relaypod/instructions.md")
                        .desired_width(f32::INFINITY),
                );
                ui.label("Custom prompt template");
                ui.add(
                    egui::TextEdit::multiline(&mut self.prompt)
                        .desired_rows(2)
                        .hint_text("Leave empty to use default RelayPod prompt")
                        .desired_width(f32::INFINITY),
                );
            });
    }
}

/// Pure `egui` presentation layer over `relaypod_client::{WorkspaceState, ClientRuntime}`.
struct RelayPodApp {
    runtime: ClientRuntime,
    state: WorkspaceState,
    logo_texture: Option<egui::TextureHandle>,

    // View-only form inputs & modal flags
    server_url: String,
    token: String,
    show_unread_drawer: bool,

    channel_composer: String,
    channel_mention: MentionState,
    thread_composer: String,
    thread_mention: MentionState,
    prefilled_thread_id: Option<String>,
    thread_panel_open_seq: u64,
    last_min_window_width: f32,

    show_new_channel_modal: bool,
    new_channel_name: String,
    new_channel_desc: String,

    show_new_identity_modal: bool,
    new_identity_kind: IdentityKind,
    new_identity_draft: AgentEditDraft,
    new_identity_watch_locally: bool,
    last_created_identity: Option<CreatedIdentity>,
    last_created_watcher_note: Option<Result<String, String>>,

    pending_delete_message: Option<Message>,
    pending_delete_channel: Option<Channel>,
    pending_delete_identity: Option<Identity>,
    pending_regenerate_token: Option<Identity>,
    regenerated_token: Option<CreatedIdentity>,
    regenerated_watcher_note: Option<Result<String, String>>,
    channel_edit: Option<Channel>,
    identity_edit: Option<AgentEditDraft>,

    show_watcher_modal: bool,
    watcher_auto_start: bool,
    remember_connection: bool,
    save_connection_on_connect: bool,

    channel_search: String,
    channels_section: CollapsibleSection,
    archived_channels_section: CollapsibleSection,
}

impl RelayPodApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_style(&cc.egui_ctx);
        let logo_texture = load_logo_texture(&cc.egui_ctx);

        let egui_ctx = cc.egui_ctx.clone();
        let runtime = spawn_runtime(move || egui_ctx.request_repaint());

        let file_cfg = match watch::load_optional_config(None) {
            Ok(Some((_, cfg))) => Some(cfg),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(error = %err, "Failed to read ~/.relaypod/config.yaml on startup");
                None
            }
        };

        let server_url = std::env::var("RELAYPOD_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| file_cfg.as_ref().and_then(|c| c.url.clone()))
            .unwrap_or_else(|| watch::DEFAULT_SERVER_URL.to_string());
        let token = std::env::var("RELAYPOD_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| file_cfg.as_ref().and_then(|c| c.token.clone()))
            .unwrap_or_default();

        if !token.is_empty() {
            runtime.send(ClientCommand::Connect {
                base_url: server_url.clone(),
                token: token.clone(),
            });
        }

        Self {
            runtime,
            state: WorkspaceState::default(),
            logo_texture,
            server_url,
            token,
            show_unread_drawer: false,
            channel_composer: String::new(),
            channel_mention: MentionState::default(),
            thread_composer: String::new(),
            thread_mention: MentionState::default(),
            prefilled_thread_id: None,
            thread_panel_open_seq: 0,
            last_min_window_width: 0.0,
            show_new_channel_modal: false,
            new_channel_name: String::new(),
            new_channel_desc: String::new(),
            show_new_identity_modal: false,
            new_identity_kind: IdentityKind::Agent,
            new_identity_draft: AgentEditDraft::new_blank(),
            new_identity_watch_locally: true,
            last_created_identity: None,
            last_created_watcher_note: None,
            pending_delete_message: None,
            pending_delete_channel: None,
            pending_delete_identity: None,
            pending_regenerate_token: None,
            regenerated_token: None,
            regenerated_watcher_note: None,
            channel_edit: None,
            identity_edit: None,
            show_watcher_modal: false,
            watcher_auto_start: true,
            remember_connection: true,
            save_connection_on_connect: false,
            channel_search: String::new(),
            channels_section: CollapsibleSection::new(true),
            archived_channels_section: CollapsibleSection::new(false),
        }
    }

    fn drain_events(&mut self) {
        while let Some(ev) = self.runtime.try_recv() {
            let effects = self.state.apply(ev);
            for cmd in effects.commands {
                self.runtime.send(cmd);
            }
            if effects.just_connected {
                if self.save_connection_on_connect {
                    self.save_connection_on_connect = false;
                    if let Err(err) = watch::default_config_path().and_then(|path| {
                        watch::upsert_client_connection_in_config(
                            &path,
                            &self.server_url,
                            Some(&self.token),
                        )
                    }) {
                        tracing::warn!(error = %err, "Failed to save connection to ~/.relaypod/config.yaml");
                    }
                }
                if self.watcher_auto_start {
                    self.runtime.send(ClientCommand::StartWatcher {
                        default_url: self.server_url.clone(),
                        config_path: None,
                    });
                }
            }
            if effects.channel_just_created {
                self.show_new_channel_modal = false;
                self.new_channel_name.clear();
                self.new_channel_desc.clear();
            }
            if let Some(created) = effects.identity_just_created {
                if created.identity.kind == IdentityKind::Agent && self.new_identity_watch_locally {
                    match watch::default_config_path().and_then(|path| {
                        watch::upsert_watcher_token_in_config(
                            &path,
                            &self.server_url,
                            &created.identity.handle,
                            &created.token,
                        )
                        .map(|()| path)
                    }) {
                        Ok(path) => {
                            self.watcher_auto_start = true;
                            self.runtime.send(ClientCommand::ReloadWatcher {
                                default_url: self.server_url.clone(),
                                config_path: None,
                            });
                            self.last_created_watcher_note = Some(Ok(format!(
                                "Saved '{}' to {} and started local watcher.",
                                created.identity.handle,
                                path.display()
                            )));
                        }
                        Err(err) => {
                            self.last_created_watcher_note =
                                Some(Err(format!("Failed to update config.yaml: {err}")));
                        }
                    }
                } else {
                    self.last_created_watcher_note = None;
                }
                self.last_created_identity = Some(created);
                self.new_identity_draft = AgentEditDraft::new_blank();
            }
            if let Some(regenerated) = effects.token_just_regenerated {
                let rotated_self = self
                    .state
                    .me
                    .as_ref()
                    .is_some_and(|me| me.id == regenerated.identity.id);
                if rotated_self {
                    self.token = regenerated.token.clone();
                }
                if regenerated.identity.kind == IdentityKind::Agent {
                    let key = self
                        .state
                        .watcher
                        .token_name_for_identity(&regenerated.identity.id)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| regenerated.identity.handle.clone());
                    self.regenerated_watcher_note =
                        match watch::default_config_path().and_then(|path| {
                            watch::update_watcher_token_in_config(&path, &key, &regenerated.token)
                                .map(|updated| (path, updated))
                        }) {
                            Ok((path, true)) => {
                                if self.state.watcher.running || self.watcher_auto_start {
                                    self.runtime.send(ClientCommand::ReloadWatcher {
                                        default_url: self.server_url.clone(),
                                        config_path: None,
                                    });
                                }
                                Some(Ok(format!(
                                    "Updated '{key}' in {} and reloaded local watcher.",
                                    path.display()
                                )))
                            }
                            Ok((_, false)) => None,
                            Err(err) => Some(Err(format!("Failed to update config.yaml: {err}"))),
                        };
                } else if rotated_self {
                    self.regenerated_watcher_note =
                        match watch::default_config_path().and_then(|path| {
                            watch::update_client_token_in_config(&path, &regenerated.token)
                                .map(|updated| (path, updated))
                        }) {
                            Ok((path, true)) => {
                                Some(Ok(format!("Updated saved token in {}.", path.display())))
                            }
                            Ok((_, false)) => None,
                            Err(err) => Some(Err(format!("Failed to update config.yaml: {err}"))),
                        };
                } else {
                    self.regenerated_watcher_note = None;
                }
                self.regenerated_token = Some(regenerated);
            }
        }
    }

    fn select_channel(&mut self, channel_id: String) {
        if self.state.active_thread.is_some() {
            self.thread_panel_open_seq = self.thread_panel_open_seq.wrapping_add(1);
        }
        let cmd = self.state.select_channel(channel_id);
        self.runtime.send(cmd);
    }

    fn is_admin(&self) -> bool {
        self.state
            .me
            .as_ref()
            .is_some_and(|me| me.kind == IdentityKind::Human && me.is_admin)
    }

    fn can_manage_channels(&self) -> bool {
        self.state
            .me
            .as_ref()
            .is_some_and(|me| me.kind == IdentityKind::Human)
    }

    fn can_delete_channel(&self, channel: &Channel) -> bool {
        self.state.me.as_ref().is_some_and(|me| {
            me.kind == IdentityKind::Human
                && (me.is_admin || channel.created_by.as_deref() == Some(me.id.as_str()))
        })
    }

    fn can_rename_identity(&self, identity: &Identity) -> bool {
        identity.kind == IdentityKind::Agent
            && self.state.me.as_ref().is_some_and(|me| {
                me.kind == IdentityKind::Human
                    && identity.owner_id.as_deref() == Some(me.id.as_str())
            })
    }

    fn can_delete_identity(&self, identity: &Identity) -> bool {
        self.state.me.as_ref().is_some_and(|me| {
            if me.kind != IdentityKind::Human {
                return false;
            }
            match identity.kind {
                IdentityKind::Agent => identity.owner_id.as_deref() == Some(me.id.as_str()),
                IdentityKind::Human => me.is_admin && identity.id != me.id,
            }
        })
    }

    fn can_regenerate_token(&self, identity: &Identity) -> bool {
        self.state.me.as_ref().is_some_and(|me| {
            if me.kind != IdentityKind::Human {
                return false;
            }
            if me.is_admin {
                return true;
            }
            identity.kind == IdentityKind::Agent
                && identity.owner_id.as_deref() == Some(me.id.as_str())
        })
    }

    fn can_delete_message(&self, msg: &Message) -> bool {
        if msg.parent_id.is_some() {
            return false;
        }
        self.state.me.as_ref().is_some_and(|me| {
            if me.kind != IdentityKind::Human {
                return false;
            }
            if me.is_admin || msg.author_id == me.id {
                return true;
            }
            msg.author_kind == IdentityKind::Agent
                && self
                    .state
                    .identities
                    .iter()
                    .find(|i| i.id == msg.author_id)
                    .and_then(|i| i.owner_id.as_deref())
                    == Some(me.id.as_str())
        })
    }

    fn render_channel_row(&mut self, ui: &mut egui::Ui, channel: &Channel) {
        let selected = self.state.selected_channel_id.as_deref() == Some(channel.id.as_str());
        let mut row = ui.selectable_label(selected, format!("# {}", channel.name));
        if !channel.description.trim().is_empty() {
            row = row.on_hover_text(&channel.description);
        }
        if row.clicked() {
            self.select_channel(channel.id.clone());
        }

        if self.can_manage_channels() {
            row.context_menu(|ui| {
                if ui.button("Edit channel").clicked() {
                    self.channel_edit = Some(channel.clone());
                    ui.close_menu();
                }
                let archive = !channel.archived;
                if ui
                    .button(if archive { "Archive" } else { "Unarchive" })
                    .clicked()
                {
                    self.runtime.send(ClientCommand::UpdateChannel {
                        channel_id: channel.id.clone(),
                        request: UpdateChannelRequest {
                            archived: Some(archive),
                            ..Default::default()
                        },
                    });
                    ui.close_menu();
                }
                if self.can_delete_channel(channel) && ui.button("Delete").clicked() {
                    self.pending_delete_channel = Some(channel.clone());
                    ui.close_menu();
                }
            });
        }
    }

    fn render_connect_screen(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(96.0);
                if let Some(ref tex) = self.logo_texture {
                    ui.add(
                        egui::Image::from_texture(tex)
                            .fit_to_exact_size(egui::vec2(64.0, 64.0))
                            .corner_radius(CornerRadius::same(14)),
                    );
                    ui.add_space(10.0);
                }
                ui.heading("RelayPod");
                ui.label("Shared message board for humans & AI agents");
                ui.add_space(20.0);

                egui::Frame::group(ui.style())
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(16.0)
                    .show(ui, |ui| {
                        ui.set_width(420.0);
                        ui.label("Server URL");
                        let url_resp = ui.text_edit_singleline(&mut self.server_url);
                        if is_non_loopback_http(&self.server_url) {
                            ui.colored_label(
                                Color32::YELLOW,
                                "Warning: connecting over plaintext http:// to a non-local host",
                            );
                        }
                        ui.add_space(8.0);
                        ui.label("Bearer Token");
                        let token_resp = ui.add(
                            egui::TextEdit::singleline(&mut self.token)
                                .password(true)
                                .hint_text("rp_..."),
                        );
                        ui.add_space(6.0);
                        ui.checkbox(
                            &mut self.remember_connection,
                            "Save URL & token to ~/.relaypod/config.yaml",
                        );
                        let enter_pressed = (url_resp.lost_focus() || token_resp.lost_focus())
                            && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.add_space(10.0);
                        if (ui.button("Connect").clicked() || enter_pressed)
                            && !self.token.trim().is_empty()
                        {
                            self.state.status_error = None;
                            self.save_connection_on_connect = self.remember_connection;
                            self.runtime.send(ClientCommand::Connect {
                                base_url: self.server_url.clone(),
                                token: self.token.clone(),
                            });
                        }
                        if let Some(err) = &self.state.status_error {
                            ui.add_space(8.0);
                            ui.colored_label(Color32::LIGHT_RED, err);
                        }
                    });
            });
        });
    }

    fn render_modals(&mut self, ctx: &egui::Context) {
        if self.show_new_channel_modal {
            let mut close = false;
            let modal = egui::Modal::new(egui::Id::new("new_channel")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.heading("New Channel");
                ui.label("Channel name (without #)");
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_channel_name)
                        .desired_width(f32::INFINITY),
                );
                let desc_valid =
                    render_channel_description_input(ui, "new_channel", &mut self.new_channel_desc);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.new_channel_name.trim().is_empty() && desc_valid,
                            egui::Button::new("Create"),
                        )
                        .clicked()
                    {
                        self.runtime.send(ClientCommand::CreateChannel {
                            name: self.new_channel_name.clone(),
                            description: self.new_channel_desc.clone(),
                        });
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
            if close || modal.should_close() {
                self.show_new_channel_modal = false;
            }
        }

        if self.show_new_identity_modal {
            let mut close = false;
            let is_admin = self.is_admin();
            if !is_admin {
                self.new_identity_kind = IdentityKind::Agent;
            }
            let max_sessions_valid = self.new_identity_draft.max_sessions_valid();
            let cooldown_valid = self.new_identity_draft.cooldown_valid();

            let modal = egui::Modal::new(egui::Id::new("new_identity")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.heading(if is_admin {
                    "Register Identity"
                } else {
                    "Register Agent"
                });

                if is_admin {
                    ui.horizontal(|ui| {
                        ui.label("Kind:");
                        ui.selectable_value(
                            &mut self.new_identity_kind,
                            IdentityKind::Agent,
                            "Agent",
                        );
                        ui.selectable_value(
                            &mut self.new_identity_kind,
                            IdentityKind::Human,
                            "Human",
                        );
                    });
                }

                let is_agent = self.new_identity_kind == IdentityKind::Agent;

                egui::ScrollArea::vertical()
                    .max_height(460.0)
                    .show(ui, |ui| {
                        ui.label(if is_agent {
                            "Handle (e.g. claude-dev)"
                        } else {
                            "Handle (e.g. alice)"
                        });
                        ui.add(
                            egui::TextEdit::singleline(&mut self.new_identity_draft.handle)
                                .desired_width(f32::INFINITY),
                        );

                        if is_agent {
                            self.new_identity_draft.render_fields(ui, "new_agent", true);
                            ui.add_space(6.0);
                            ui.checkbox(
                                &mut self.new_identity_watch_locally,
                                "Watch on this machine (save to ~/.relaypod/config.yaml)",
                            );
                        }
                    });

                ui.add_space(6.0);
                let can_create = !self.new_identity_draft.handle.trim().is_empty()
                    && (!is_agent || (max_sessions_valid && cooldown_valid));
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(can_create, egui::Button::new("Create"))
                        .clicked()
                    {
                        let agent_config = if is_agent {
                            Some(Box::new(self.new_identity_draft.to_agent_config(true)))
                        } else {
                            None
                        };
                        self.runtime.send(ClientCommand::CreateIdentity {
                            handle: self.new_identity_draft.handle.clone(),
                            kind: self.new_identity_kind,
                            agent_config,
                        });
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });

                if let Some(created) = &self.last_created_identity {
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            Color32::LIGHT_GREEN,
                            format!(
                                "Created @{} — save this token now:",
                                created.identity.handle
                            ),
                        );
                        if ui.small_button("Copy token").clicked() {
                            ui.ctx().copy_text(created.token.clone());
                        }
                    });
                    let mut tok = created.token.clone();
                    ui.add(egui::TextEdit::singleline(&mut tok).desired_width(f32::INFINITY));

                    if let Some(ref note) = self.last_created_watcher_note {
                        ui.add_space(4.0);
                        match note {
                            Ok(msg) => {
                                ui.colored_label(Color32::LIGHT_GREEN, msg);
                            }
                            Err(err) => {
                                ui.colored_label(Color32::LIGHT_RED, err);
                            }
                        }
                    }

                    if created.identity.kind == IdentityKind::Agent {
                        ui.add_space(4.0);
                        let cfg_line =
                            format!("    {}: {}", created.identity.handle, created.token);
                        if self.last_created_watcher_note.is_none() {
                            ui.label(
                                egui::RichText::new(
                                    "Add under watcher.agent_tokens in ~/.relaypod/config.yaml:",
                                )
                                .size(11.5)
                                .color(ui.visuals().weak_text_color()),
                            );
                        }
                        ui.horizontal(|ui| {
                            if ui.small_button("Copy config line").clicked() {
                                ui.ctx().copy_text(cfg_line.clone());
                            }
                            if ui.small_button("Copy MCP JSON").clicked() {
                                ui.ctx().copy_text(mcp_config_snippet(
                                    &self.server_url,
                                    &created.token,
                                ));
                            }
                        });
                    }
                }
            });
            if close || modal.should_close() {
                self.show_new_identity_modal = false;
                self.last_created_identity = None;
                self.last_created_watcher_note = None;
            }
        }

        if let Some(channel) = &mut self.channel_edit {
            let mut save = false;
            let mut cancel = false;
            let modal = egui::Modal::new(egui::Id::new("edit_channel")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.heading("Edit channel");
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut channel.name).desired_width(f32::INFINITY));
                let desc_valid =
                    render_channel_description_input(ui, "edit_channel", &mut channel.description);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    save = ui
                        .add_enabled(
                            !channel.name.trim().is_empty() && desc_valid,
                            egui::Button::new("Save"),
                        )
                        .clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
            if save {
                self.runtime.send(ClientCommand::UpdateChannel {
                    channel_id: channel.id.clone(),
                    request: UpdateChannelRequest {
                        name: Some(channel.name.clone()),
                        description: Some(channel.description.clone()),
                        ..Default::default()
                    },
                });
            }
            if save || cancel || modal.should_close() {
                self.channel_edit = None;
            }
        }

        if let Some(draft) = &mut self.identity_edit {
            let mut save = false;
            let mut cancel = false;
            let max_sessions_valid = draft.max_sessions_valid();
            let cooldown_valid = draft.cooldown_valid();

            let modal = egui::Modal::new(egui::Id::new("edit_agent")).show(ctx, |ui| {
                ui.set_width(460.0);
                ui.heading("Edit agent");

                egui::ScrollArea::vertical()
                    .max_height(460.0)
                    .show(ui, |ui| {
                        ui.label("Handle");
                        ui.add(
                            egui::TextEdit::singleline(&mut draft.handle)
                                .desired_width(f32::INFINITY),
                        );

                        draft.render_fields(ui, "edit_agent", false);
                    });

                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Config changes apply to new agent sessions.")
                        .size(11.5)
                        .color(ui.visuals().weak_text_color()),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    save = ui
                        .add_enabled(
                            !draft.handle.trim().is_empty() && max_sessions_valid && cooldown_valid,
                            egui::Button::new("Save"),
                        )
                        .clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
            if save {
                self.runtime.send(ClientCommand::UpdateIdentity {
                    identity_id: draft.id.clone(),
                    request: Box::new(UpdateIdentityRequest {
                        handle: Some(draft.handle.clone()),
                        agent_config: Some(draft.to_agent_config(false)),
                    }),
                });
            }
            if save || cancel || modal.should_close() {
                self.identity_edit = None;
            }
        }

        if let Some(identity) = &self.pending_delete_identity {
            let mut confirm = false;
            let mut cancel = false;
            let modal =
                egui::Modal::new(egui::Id::new("confirm_delete_identity")).show(ctx, |ui| {
                    ui.set_width(360.0);
                    ui.heading(format!("Delete @{}?", identity.handle));
                    ui.label(
                        "This revokes the identity's token and removes it from the workspace. \
                     Existing messages and mentions are preserved.",
                    );
                    ui.add_space(12.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let delete =
                            egui::Button::new(egui::RichText::new("Delete").color(Color32::WHITE))
                                .fill(Color32::from_rgb(220, 38, 38))
                                .corner_radius(CornerRadius::same(8));
                        confirm = ui.add(delete).clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            if confirm {
                if identity.kind == IdentityKind::Agent {
                    if let Some(token_name) = self
                        .state
                        .watcher
                        .token_name_for_identity(&identity.id)
                        .map(ToString::to_string)
                    {
                        if let Ok(path) = watch::default_config_path() {
                            if let Ok(true) =
                                watch::remove_watcher_token_from_config(&path, &token_name)
                            {
                                self.runtime.send(ClientCommand::ReloadWatcher {
                                    default_url: self.server_url.clone(),
                                    config_path: None,
                                });
                            }
                        }
                    }
                }
                self.runtime.send(ClientCommand::DeleteIdentity {
                    identity_id: identity.id.clone(),
                });
            }
            if confirm || cancel || modal.should_close() {
                self.pending_delete_identity = None;
            }
        }

        if let Some(identity) = &self.pending_regenerate_token {
            let mut confirm = false;
            let mut cancel = false;
            let modal =
                egui::Modal::new(egui::Id::new("confirm_regenerate_token")).show(ctx, |ui| {
                    ui.set_width(380.0);
                    ui.heading(format!("Regenerate token for @{}?", identity.handle));
                    ui.label(
                        "This immediately invalidates the current token and disconnects active \
                         sessions using it. You will be shown the new token once.",
                    );
                    ui.add_space(12.0);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let regen = egui::Button::new(
                            egui::RichText::new("Regenerate").color(Color32::WHITE),
                        )
                        .fill(Color32::from_rgb(217, 119, 6))
                        .corner_radius(CornerRadius::same(8));
                        confirm = ui.add(regen).clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            if confirm {
                self.runtime.send(ClientCommand::RegenerateIdentityToken {
                    identity_id: identity.id.clone(),
                });
            }
            if confirm || cancel || modal.should_close() {
                self.pending_regenerate_token = None;
            }
        }

        if let Some(regenerated) = &self.regenerated_token {
            let mut close = false;
            let modal = egui::Modal::new(egui::Id::new("regenerated_token")).show(ctx, |ui| {
                ui.set_width(420.0);
                ui.heading(format!("New token for @{}", regenerated.identity.handle));
                ui.horizontal(|ui| {
                    ui.colored_label(
                        Color32::LIGHT_GREEN,
                        "Save this token now — the previous token is no longer valid:",
                    );
                    if ui.small_button("Copy token").clicked() {
                        ui.ctx().copy_text(regenerated.token.clone());
                    }
                });
                let mut tok = regenerated.token.clone();
                ui.add(egui::TextEdit::singleline(&mut tok).desired_width(f32::INFINITY));
                if let Some(ref note) = self.regenerated_watcher_note {
                    ui.add_space(4.0);
                    match note {
                        Ok(msg) => {
                            ui.colored_label(Color32::LIGHT_GREEN, msg);
                        }
                        Err(err) => {
                            ui.colored_label(Color32::LIGHT_RED, err);
                        }
                    }
                }
                ui.add_space(8.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Done").clicked() {
                        close = true;
                    }
                });
            });
            if close || modal.should_close() {
                self.regenerated_token = None;
                self.regenerated_watcher_note = None;
            }
        }

        if let Some(channel) = &self.pending_delete_channel {
            let mut confirm = false;
            let mut cancel = false;
            let modal = egui::Modal::new(egui::Id::new("confirm_delete_channel")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.heading(format!("Delete #{}?", channel.name));
                ui.label("This permanently deletes the channel, its messages, and all threads.");
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let delete =
                        egui::Button::new(egui::RichText::new("Delete").color(Color32::WHITE))
                            .fill(Color32::from_rgb(220, 38, 38))
                            .corner_radius(CornerRadius::same(8));
                    confirm = ui.add(delete).clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
            if confirm {
                self.runtime.send(ClientCommand::DeleteChannel {
                    channel_id: channel.id.clone(),
                });
            }
            if confirm || cancel || modal.should_close() {
                self.pending_delete_channel = None;
            }
        }

        if let Some(msg) = self.pending_delete_message.clone() {
            let mut close_modal = false;
            let mut confirmed = false;

            let modal = egui::Modal::new(egui::Id::new("confirm_delete_modal")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.heading("Delete message?");
                ui.add_space(6.0);

                let preview = if msg.body.chars().count() > 120 {
                    let truncated: String = msg.body.chars().take(120).collect();
                    format!("{truncated}...")
                } else {
                    msg.body.clone()
                };

                egui::Frame::group(ui.style())
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.strong(format!("@{}", msg.author_handle));
                        ui.label(
                            egui::RichText::new(preview).color(ui.visuals().weak_text_color()),
                        );
                    });

                ui.add_space(8.0);
                if msg.reply_count > 0 {
                    ui.colored_label(
                        Color32::from_rgb(248, 113, 113),
                        format!(
                            "This will also permanently delete {} {} in this thread.",
                            msg.reply_count,
                            if msg.reply_count == 1 {
                                "reply"
                            } else {
                                "replies"
                            }
                        ),
                    );
                } else {
                    ui.label("This action cannot be undone.");
                }

                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let delete_btn =
                        egui::Button::new(egui::RichText::new("Delete").color(Color32::WHITE))
                            .fill(Color32::from_rgb(220, 38, 38))
                            .corner_radius(CornerRadius::same(8));
                    if ui.add(delete_btn).clicked() {
                        confirmed = true;
                        close_modal = true;
                    }
                    if ui.button("Cancel").clicked() {
                        close_modal = true;
                    }
                });
            });

            if modal.should_close() {
                close_modal = true;
            }
            if confirmed {
                self.runtime
                    .send(ClientCommand::DeleteMessage { message_id: msg.id });
            }
            if close_modal {
                self.pending_delete_message = None;
            }
        }

        if self.show_watcher_modal {
            let mut close = false;
            let watcher = self.state.watcher.clone();
            let modal = egui::Modal::new(egui::Id::new("watcher_modal")).show(ctx, |ui| {
                ui.set_width(520.0);
                ui.horizontal(|ui| {
                    ui.heading("Local Agent Watcher");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                        if ui.button("Reload config").clicked() {
                            self.watcher_auto_start = true;
                            self.runtime.send(ClientCommand::ReloadWatcher {
                                default_url: self.server_url.clone(),
                                config_path: None,
                            });
                        }
                        if watcher.running {
                            if ui.button("Stop").clicked() {
                                self.watcher_auto_start = false;
                                self.runtime.send(ClientCommand::StopWatcher);
                            }
                        } else if ui.button("Start").clicked() {
                            self.watcher_auto_start = true;
                            self.runtime.send(ClientCommand::StartWatcher {
                                default_url: self.server_url.clone(),
                                config_path: None,
                            });
                        }
                    });
                });

                let cfg_label = watcher
                    .config_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.relaypod/config.yaml".to_string());
                ui.label(
                    egui::RichText::new(format!("Config: {cfg_label}"))
                        .size(11.5)
                        .color(ui.visuals().weak_text_color()),
                );

                if let Some(ref err) = watcher.last_error {
                    ui.add_space(4.0);
                    ui.colored_label(Color32::LIGHT_RED, err);
                }

                ui.add_space(8.0);
                ui.strong(format!("Watched Agents ({})", watcher.targets.len()));
                ui.add_space(4.0);

                if watcher.targets.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "No local agents are currently running. Click '+' next to IDENTITIES \
                             to create an agent and watch it on this machine, or add entries under \
                             watcher.agent_tokens in ~/.relaypod/config.yaml and click Reload config.",
                        )
                        .color(ui.visuals().weak_text_color()),
                    );
                } else {
                    egui::Frame::group(ui.style())
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for target in &watcher.targets {
                                ui.horizontal(|ui| {
                                    let (dot_color, pulse, phase_text, err_tooltip) =
                                        match &target.phase {
                                            TargetPhase::Connecting => {
                                                (Color32::YELLOW, false, "Connecting...".into(), None)
                                            }
                                            TargetPhase::Connected => {
                                                (Color32::LIGHT_GREEN, true, "Connected".into(), None)
                                            }
                                            TargetPhase::Working { channel } => (
                                                Color32::from_rgb(96, 165, 250),
                                                true,
                                                format!("Working in #{channel}"),
                                                None,
                                            ),
                                            TargetPhase::Blocked { channel } => (
                                                Color32::from_rgb(248, 113, 113),
                                                true,
                                                format!("Blocked in #{channel}"),
                                                Some("Waiting for approval/input in Herdr".into()),
                                            ),
                                            TargetPhase::Reconnecting => (
                                                Color32::YELLOW,
                                                false,
                                                "Reconnecting...".into(),
                                                None,
                                            ),
                                            TargetPhase::Error(msg) => (
                                                Color32::LIGHT_RED,
                                                false,
                                                "Error".into(),
                                                Some(msg.clone()),
                                            ),
                                        };
                                    draw_status_dot(ui, dot_color, pulse);

                                    let primary = match &target.handle {
                                        Some(h) => format!("@{h}"),
                                        None => target.token_name.clone(),
                                    };
                                    ui.strong(&primary);

                                    if target.handle.as_deref() != Some(target.token_name.as_str())
                                        && target.handle.is_some()
                                    {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "(key: {})",
                                                target.token_name
                                            ))
                                            .size(11.5)
                                            .color(ui.visuals().weak_text_color()),
                                        );
                                    }

                                    let meta = match (&target.runner, &target.harness) {
                                        (Some(r), Some(h)) => format!("{r}/{h}"),
                                        (Some(r), None) => r.clone(),
                                        _ => String::new(),
                                    };
                                    if !meta.is_empty() {
                                        ui.label(
                                            egui::RichText::new(meta)
                                                .size(11.5)
                                                .color(ui.visuals().weak_text_color()),
                                        );
                                    }

                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            let lbl = ui.colored_label(dot_color, phase_text);
                                            if let Some(tip) = err_tooltip {
                                                lbl.on_hover_text(tip);
                                            }
                                        },
                                    );
                                });
                            }
                        });
                }

                ui.add_space(8.0);
                egui::CollapsingHeader::new(format!(
                    "Recent Activity ({})",
                    watcher.recent_logs.len()
                ))
                .id_salt("watcher_recent_activity")
                .default_open(!watcher.recent_logs.is_empty())
                .show(ui, |ui| {
                    if watcher.recent_logs.is_empty() {
                        ui.label(
                            egui::RichText::new("No watcher activity yet.")
                                .color(ui.visuals().weak_text_color()),
                        );
                    } else {
                        egui::Frame::group(ui.style())
                            .corner_radius(CornerRadius::same(6))
                            .inner_margin(Margin::symmetric(8, 6))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                egui::ScrollArea::vertical()
                                    .id_salt("watcher_logs_scroll")
                                    .max_height(160.0)
                                    .stick_to_bottom(true)
                                    .show(ui, |ui| {
                                        for line in &watcher.recent_logs {
                                            ui.label(
                                                egui::RichText::new(line)
                                                    .monospace()
                                                    .size(11.5),
                                            );
                                        }
                                    });
                            });
                    }
                });
            });
            if close || modal.should_close() {
                self.show_watcher_modal = false;
            }
        }
    }
}

impl eframe::App for RelayPodApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        let min_window_w = if !self.state.connected {
            420.0
        } else {
            const MIN_SIDEBAR_WIDTH: f32 = 180.0;
            const MIN_PANE_WIDTH: f32 = 300.0;
            let right_pane_w = if self.state.active_thread.is_some() || self.show_unread_drawer {
                MIN_PANE_WIDTH
            } else {
                0.0
            };
            MIN_SIDEBAR_WIDTH + MIN_PANE_WIDTH + right_pane_w
        };
        if (min_window_w - self.last_min_window_width).abs() > 0.5 {
            self.last_min_window_width = min_window_w;
            ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(egui::vec2(
                min_window_w,
                360.0,
            )));
        }

        if !self.state.connected {
            self.render_connect_screen(ctx);
            return;
        }

        // Top bar
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(ref tex) = self.logo_texture {
                    ui.add(
                        egui::Image::from_texture(tex)
                            .fit_to_exact_size(egui::vec2(20.0, 20.0))
                            .corner_radius(CornerRadius::same(4)),
                    );
                }
                ui.strong("RelayPod");
                if let Some(me) = &self.state.me {
                    ui.label(format!("@{}", me.handle));
                }
                let (dot_label, color) = if self.state.ws_live {
                    ("Live", Color32::LIGHT_GREEN)
                } else {
                    ("Reconnecting", Color32::YELLOW)
                };
                draw_status_dot(ui, color, self.state.ws_live);
                ui.colored_label(color, dot_label);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Disconnect").clicked() {
                        self.last_created_identity = None;
                        self.last_created_watcher_note = None;
                        self.pending_regenerate_token = None;
                        self.regenerated_token = None;
                        self.regenerated_watcher_note = None;
                        self.show_watcher_modal = false;
                        self.runtime.send(ClientCommand::Disconnect);
                    }
                    let unread_label = format!("My Unread ({})", self.state.unread.len());
                    if ui
                        .selectable_label(self.show_unread_drawer, unread_label)
                        .clicked()
                    {
                        self.show_unread_drawer = !self.show_unread_drawer;
                        if self.show_unread_drawer {
                            self.runtime.send(ClientCommand::RefreshUnread);
                        }
                    }

                    let (watcher_label, watcher_color, watcher_pulse) =
                        watcher_pill_summary(&self.state.watcher);
                    let pill =
                        egui::Button::new(egui::RichText::new(watcher_label).color(watcher_color))
                            .selected(self.show_watcher_modal);
                    if ui
                        .add(pill)
                        .on_hover_text("Local agent watcher status & controls")
                        .clicked()
                    {
                        self.show_watcher_modal = !self.show_watcher_modal;
                    }
                    draw_status_dot(ui, watcher_color, watcher_pulse);
                });
            });
            if let Some(err) = self.state.status_error.clone() {
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::LIGHT_RED, err);
                    if ui.small_button("×").on_hover_text("Dismiss").clicked() {
                        self.state.status_error = None;
                    }
                });
            }
        });

        // Left sidebar: Channels + Identities
        egui::SidePanel::left("sidebar")
            .default_width(220.0)
            .min_width(180.0)
            .show(ctx, |ui| {
                render_channel_search_box(ui, &mut self.channel_search);
                ui.add_space(6.0);

                let channels = self.state.channels.clone();
                let filtered = filter_channels(&channels, &self.channel_search);
                let is_searching = !self.channel_search.trim().is_empty();
                if !is_searching {
                    self.channels_section.clear_search_override();
                    self.archived_channels_section.clear_search_override();
                }
                let has_active_matches = !filtered.active.is_empty();
                let has_archived_matches = !filtered.archived.is_empty();
                let channels_open = self
                    .channels_section
                    .is_expanded(is_searching, has_active_matches);
                let archived_open = self
                    .archived_channels_section
                    .is_expanded(is_searching, has_archived_matches);

                egui::ScrollArea::vertical()
                    .id_salt("sidebar_channels_scroll")
                    .max_height((ui.available_height() * 0.6).max(120.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if render_collapsible_section_header(
                                ui,
                                "CHANNELS",
                                channels_open,
                                None,
                            ) {
                                self.channels_section
                                    .toggle(is_searching, has_active_matches);
                            }
                            if self.can_manage_channels() {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .small_button("+")
                                            .on_hover_text("New channel")
                                            .clicked()
                                        {
                                            self.show_new_channel_modal = true;
                                        }
                                    },
                                );
                            }
                        });

                        if channels_open {
                            for channel in &filtered.active {
                                self.render_channel_row(ui, channel);
                            }
                        }

                        if has_archived_matches {
                            ui.add_space(8.0);
                            if render_collapsible_section_header(
                                ui,
                                "ARCHIVED CHANNELS",
                                archived_open,
                                Some(filtered.archived.len()),
                            ) {
                                self.archived_channels_section
                                    .toggle(is_searching, has_archived_matches);
                            }
                            if archived_open {
                                for channel in &filtered.archived {
                                    self.render_channel_row(ui, channel);
                                }
                            }
                        }

                        if is_searching && !has_active_matches && !has_archived_matches {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new("No matching channels")
                                    .size(12.0)
                                    .color(ui.visuals().weak_text_color()),
                            );
                        }
                    });

                ui.add_space(12.0);
                ui.separator();

                ui.horizontal(|ui| {
                    ui.strong("IDENTITIES");
                    if self.can_manage_channels() {
                        let tooltip = if self.is_admin() {
                            "Register agent or human"
                        } else {
                            "Register agent"
                        };
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("+").on_hover_text(tooltip).clicked() {
                                self.show_new_identity_modal = true;
                                self.new_identity_kind = IdentityKind::Agent;
                                self.new_identity_draft = AgentEditDraft::new_blank();
                                self.new_identity_watch_locally = true;
                                self.last_created_identity = None;
                                self.last_created_watcher_note = None;
                            }
                        });
                    }
                });

                let sidebar_identities = self.state.identities.clone();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for id in &sidebar_identities {
                        let label = if id.unread_count > 0 {
                            format!("@{} ({})", id.handle, id.unread_count)
                        } else {
                            format!("@{}", id.handle)
                        };
                        ui.horizontal(|ui| {
                            draw_kind_badge(ui, id.kind);
                            let mut row = ui.add(
                                egui::Label::new(label)
                                    .selectable(false)
                                    .sense(egui::Sense::click()),
                            );
                            let mut tooltip_lines = id
                                .agent_config
                                .as_ref()
                                .map(format_agent_config_tooltip)
                                .filter(|s| !s.is_empty())
                                .into_iter()
                                .collect::<Vec<_>>();
                            if let Some(token_key) =
                                self.state.watcher.token_name_for_identity(&id.id)
                            {
                                tooltip_lines
                                    .push(format!("Watched locally (config key: {token_key})"));
                            }
                            if !tooltip_lines.is_empty() {
                                row = row.on_hover_text(tooltip_lines.join("\n"));
                            }
                            let can_edit = self.can_rename_identity(id);
                            let can_regen = self.can_regenerate_token(id);
                            let can_delete = self.can_delete_identity(id);
                            if can_edit || can_regen || can_delete {
                                row.context_menu(|ui| {
                                    if can_edit && ui.button("Edit agent").clicked() {
                                        self.identity_edit =
                                            Some(AgentEditDraft::from_identity(id));
                                        ui.close_menu();
                                    }
                                    if can_regen && ui.button("Regenerate token").clicked() {
                                        self.pending_regenerate_token = Some(id.clone());
                                        ui.close_menu();
                                    }
                                    if can_delete {
                                        let label = match id.kind {
                                            IdentityKind::Agent => "Delete agent",
                                            IdentityKind::Human => "Delete human",
                                        };
                                        if ui.button(label).clicked() {
                                            self.pending_delete_identity = Some(id.clone());
                                            ui.close_menu();
                                        }
                                    }
                                });
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    draw_presence_dot(ui, id.online);
                                },
                            );
                        });
                    }
                });
            });

        // Right panel: Unread queue or active thread
        if self.show_unread_drawer {
            egui::SidePanel::right("unread_panel")
                .default_width(360.0)
                .show(ctx, |ui| {
                    let items = self.state.unread.clone();
                    ui.horizontal(|ui| {
                        ui.strong("Unread Queue");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Close").clicked() {
                                self.show_unread_drawer = false;
                            }
                            if !items.is_empty() && ui.small_button("Ack All").clicked() {
                                for item in &items {
                                    self.runtime.send(ClientCommand::AckUnread {
                                        unread_id: item.id.clone(),
                                    });
                                }
                            }
                        });
                    });
                    ui.separator();

                    if items.is_empty() {
                        ui.label("All caught up!");
                    }
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for item in items {
                            egui::Frame::group(ui.style())
                                .corner_radius(CornerRadius::same(10))
                                .inner_margin(10.0)
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        ui.strong(format!("#{}", item.channel_name));
                                        ui.label(format!("by @{}", item.author_handle));
                                    });
                                    ui.add_space(4.0);
                                    markdown::render_markdown(ui, &item.body);
                                    ui.add_space(6.0);
                                    ui.horizontal(|ui| {
                                        if ui.small_button("Open thread").clicked() {
                                            self.show_unread_drawer = false;
                                            let target = item
                                                .parent_id
                                                .clone()
                                                .unwrap_or(item.message_id.clone());
                                            self.runtime.send(ClientCommand::LoadThread {
                                                message_id: target,
                                            });
                                        }
                                        if ui.small_button("Ack").clicked() {
                                            self.runtime.send(ClientCommand::AckUnread {
                                                unread_id: item.id.clone(),
                                            });
                                        }
                                    });
                                });
                            ui.add_space(6.0);
                        }
                    });
                });
        } else if let Some(thread) = self.state.active_thread.clone() {
            let my_id = self.state.me.as_ref().map(|m| m.id.clone());
            let my_handle = self.state.me.as_ref().map(|m| m.handle.clone());
            if self.prefilled_thread_id.as_deref() != Some(thread.parent.id.as_str()) {
                self.prefilled_thread_id = Some(thread.parent.id.clone());
                self.thread_composer = last_mentioned_handle_in_thread(
                    &thread,
                    &self.state.identities,
                    my_handle.as_deref(),
                )
                .map(|h| format!("@{h} "))
                .unwrap_or_default();
            }
            const MIN_PANE_WIDTH: f32 = 300.0;
            let available_w = ctx.available_rect().width();
            let min_thread_w = MIN_PANE_WIDTH.min((available_w * 0.5).max(160.0));
            let max_thread_w = (available_w - MIN_PANE_WIDTH)
                .min(available_w * 0.75)
                .max(min_thread_w);
            let default_thread_w = (available_w * 0.5).clamp(min_thread_w, max_thread_w);
            egui::SidePanel::right(egui::Id::new((
                "thread_drawer_panel",
                self.thread_panel_open_seq,
            )))
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(Margin::symmetric(12, 8)))
            .default_width(default_thread_w)
            .min_width(min_thread_w)
            .max_width(max_thread_w)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Thread");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Close").clicked() {
                            self.state.active_thread = None;
                            self.prefilled_thread_id = None;
                            self.thread_composer.clear();
                            self.thread_panel_open_seq = self.thread_panel_open_seq.wrapping_add(1);
                        }
                    });
                });
                ui.separator();

                let identities = self.state.identities.clone();
                egui::TopBottomPanel::bottom("thread_composer")
                    .frame(egui::Frame::new().inner_margin(Margin {
                        left: 4,
                        right: 4,
                        top: 8,
                        bottom: 4,
                    }))
                    .show_inside(ui, |ui| {
                        if !self
                            .state
                            .channels
                            .iter()
                            .any(|ch| ch.id == thread.parent.channel_id && !ch.archived)
                        {
                            ui.label("This channel is archived.");
                            return;
                        }
                        if render_composer(
                            ui,
                            "thread_composer_input",
                            &mut self.thread_composer,
                            &mut self.thread_mention,
                            "Reply in thread... (@ to mention, Shift+Enter for newline)",
                            &identities,
                        ) {
                            let body = std::mem::take(&mut self.thread_composer);
                            let next_prefill = last_mentioned_handle_in_text(
                                &body,
                                &identities,
                                my_handle.as_deref(),
                            )
                            .or_else(|| {
                                last_mentioned_handle_in_thread(
                                    &thread,
                                    &identities,
                                    my_handle.as_deref(),
                                )
                            });
                            if let Some(h) = next_prefill {
                                self.thread_composer = format!("@{h} ");
                            }
                            self.runtime.send(ClientCommand::PostMessage {
                                channel_id: thread.parent.channel_id.clone(),
                                body,
                                parent_id: Some(thread.parent.id.clone()),
                            });
                        }
                    });

                egui::ScrollArea::vertical()
                    .id_salt("thread_messages_scroll")
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        let parent_is_me =
                            my_id.as_deref() == Some(thread.parent.author_id.as_str());
                        render_thread_bubble(ui, &thread.parent, parent_is_me);

                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.add(egui::Label::new(
                                egui::RichText::new(format!(
                                    "{} {}",
                                    thread.replies.len(),
                                    if thread.replies.len() == 1 {
                                        "reply"
                                    } else {
                                        "replies"
                                    }
                                ))
                                .size(11.5)
                                .color(ui.visuals().weak_text_color()),
                            ));
                            ui.separator();
                        });
                        ui.add_space(6.0);

                        for reply in &thread.replies {
                            let is_me = my_id.as_deref() == Some(reply.author_id.as_str());
                            render_thread_bubble(ui, reply, is_me);
                            ui.add_space(8.0);
                        }

                        let thread_workers: Vec<ActiveWorker> = self
                            .state
                            .workers_for_thread(&thread.parent.channel_id, &thread.parent.id)
                            .into_iter()
                            .cloned()
                            .collect();
                        if !thread_workers.is_empty() {
                            if let Some(pane_id) =
                                render_activity_banners(ui, &thread_workers, true)
                            {
                                self.runtime.send(ClientCommand::FocusHerdrPane { pane_id });
                            }
                            ui.add_space(6.0);
                        }
                    });
            });
        }

        // Center panel: Channel feed + composer
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(ch) = self.state.active_channel().cloned() {
                let archived = ch.archived;
                ui.horizontal(|ui| {
                    ui.heading(format!("# {}", ch.name));
                    if !ch.description.trim().is_empty() {
                        let inline_desc = format_inline_channel_description(&ch.description);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(inline_desc)
                                    .color(ui.visuals().weak_text_color()),
                            )
                            .truncate(),
                        )
                        .on_hover_text(&ch.description);
                    }
                });
                ui.separator();

                let identities = self.state.identities.clone();
                egui::TopBottomPanel::bottom("channel_composer")
                    .frame(egui::Frame::new().inner_margin(Margin {
                        left: 4,
                        right: 4,
                        top: 8,
                        bottom: 4,
                    }))
                    .show_inside(ui, |ui| {
                        if archived {
                            ui.label("This channel is archived.");
                            return;
                        }
                        let hint = format!(
                            "Message #{} (@ to mention, Shift+Enter for newline)",
                            ch.name
                        );
                        if render_composer(
                            ui,
                            "channel_composer_input",
                            &mut self.channel_composer,
                            &mut self.channel_mention,
                            &hint,
                            &identities,
                        ) {
                            let body = std::mem::take(&mut self.channel_composer);
                            self.runtime.send(ClientCommand::PostMessage {
                                channel_id: ch.id.clone(),
                                body,
                                parent_id: None,
                            });
                        }
                    });

                let msgs = self.state.messages.clone();
                if msgs.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "No messages in #{} yet. Say hi or @mention an agent below.",
                                ch.name
                            ))
                            .color(ui.visuals().weak_text_color()),
                        );
                    });
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt("channel_messages_scroll")
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.add_space(4.0);
                            for msg in msgs {
                                let workers: Vec<ActiveWorker> = self
                                    .state
                                    .workers_for_thread(&msg.channel_id, &msg.id)
                                    .into_iter()
                                    .cloned()
                                    .collect();
                                let thread_unread_ids: Vec<String> = self
                                    .state
                                    .unread
                                    .iter()
                                    .filter(|u| {
                                        u.message_id == msg.id
                                            || u.parent_id.as_deref() == Some(msg.id.as_str())
                                    })
                                    .map(|u| u.id.clone())
                                    .collect();
                                let can_delete = !archived && self.can_delete_message(&msg);
                                let action = render_message_card(
                                    ui,
                                    &msg,
                                    can_delete,
                                    &workers,
                                    thread_unread_ids.len(),
                                );
                                match action {
                                    MessageCardAction::OpenThread => {
                                        self.show_unread_drawer = false;
                                        self.runtime.send(ClientCommand::LoadThread {
                                            message_id: msg.id.clone(),
                                        });
                                    }
                                    MessageCardAction::AckThread => {
                                        for unread_id in thread_unread_ids {
                                            self.runtime
                                                .send(ClientCommand::AckUnread { unread_id });
                                        }
                                    }
                                    MessageCardAction::Delete => {
                                        self.pending_delete_message = Some(msg.clone());
                                    }
                                    MessageCardAction::FocusHerdrPane(pane_id) => {
                                        self.runtime
                                            .send(ClientCommand::FocusHerdrPane { pane_id });
                                    }
                                    MessageCardAction::None => {}
                                }
                                ui.add_space(6.0);
                            }
                        });
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Select a channel from the sidebar.");
                });
            }
        });

        self.render_modals(ctx);
    }
}

fn format_agent_config_tooltip(cfg: &AgentConfig) -> String {
    let mut lines = Vec::new();
    if let Some(ref r) = cfg.runner {
        lines.push(format!("Runner: {r}"));
    }
    if let Some(ref h) = cfg.harness {
        lines.push(format!("Harness: {h}"));
    }
    if let Some(ref d) = cfg.description {
        lines.push(format!("Role: {d}"));
    }
    if let Some(ref a) = cfg.args {
        lines.push(format!("Args: {a}"));
    }
    if let Some(c) = cfg.cooldown_secs.filter(|&n| n > 0) {
        lines.push(format!("Cooldown: {c}s"));
    }
    lines.join("\n")
}

fn watcher_pill_summary(snapshot: &watch::WatcherSnapshot) -> (String, Color32, bool) {
    if snapshot.last_error.is_some() {
        return ("Watcher (err)".to_string(), Color32::LIGHT_RED, false);
    }
    if !snapshot.running || snapshot.targets.is_empty() {
        return ("Watcher: Off".to_string(), Color32::from_gray(145), false);
    }
    let total = snapshot.targets.len();
    let connected = snapshot.connected_count();
    let working = snapshot.working_count();
    let blocked = snapshot.blocked_count();
    let errors = snapshot.error_count();

    if errors > 0 {
        (
            format!("Watcher ({connected}/{total})"),
            Color32::LIGHT_RED,
            false,
        )
    } else if blocked > 0 {
        (
            format!("Watcher ({total} • blocked)"),
            Color32::from_rgb(248, 113, 113),
            true,
        )
    } else if working > 0 {
        (
            format!("Watcher ({total} • working)"),
            Color32::from_rgb(96, 165, 250),
            true,
        )
    } else if connected == total {
        (format!("Watcher ({total})"), Color32::LIGHT_GREEN, true)
    } else {
        (
            format!("Watcher ({connected}/{total})"),
            Color32::YELLOW,
            false,
        )
    }
}

fn is_non_loopback_http(url: &str) -> bool {
    let trimmed = url.trim();
    let Some(rest) = trimmed.strip_prefix("http://") else {
        return false;
    };
    let host_port = rest.split('/').next().unwrap_or("");
    let host = if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    !host.is_empty() && !matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.visuals.widgets.inactive.corner_radius = CornerRadius::same(8);
    style.visuals.widgets.hovered.corner_radius = CornerRadius::same(8);
    style.visuals.widgets.active.corner_radius = CornerRadius::same(8);
    ctx.set_style(style);
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveMention {
    start_char: usize,
    end_char: usize,
    query: String,
}

fn find_active_mention(text: &str, cursor_char: usize) -> Option<ActiveMention> {
    let chars: Vec<char> = text.chars().collect();
    let end = cursor_char.min(chars.len());
    let mut i = end;

    while i > 0 {
        let ch = chars[i - 1];
        if ch == '@' {
            let start = i - 1;
            let prev_ok = start == 0
                || !(chars[start - 1].is_ascii_alphanumeric()
                    || chars[start - 1] == '_'
                    || chars[start - 1] == '-');
            if prev_ok {
                let query: String = chars[start + 1..end].iter().collect();
                return Some(ActiveMention {
                    start_char: start,
                    end_char: end,
                    query,
                });
            }
            return None;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            i -= 1;
        } else {
            return None;
        }
    }
    None
}

fn last_mentioned_handle_in_text(
    text: &str,
    identities: &[Identity],
    exclude_handle: Option<&str>,
) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut last: Option<String> = None;

    while i < bytes.len() {
        if bytes[i] == b'@' {
            let prev_ok = i == 0
                || !(bytes[i - 1].is_ascii_alphanumeric()
                    || bytes[i - 1] == b'_'
                    || bytes[i - 1] == b'-');
            if prev_ok {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric()
                        || bytes[end] == b'_'
                        || bytes[end] == b'-')
                {
                    end += 1;
                }
                if end > start {
                    let candidate = &text[start..end];
                    let is_self =
                        exclude_handle.is_some_and(|me| me.eq_ignore_ascii_case(candidate));
                    if !is_self {
                        if let Some(id) = identities
                            .iter()
                            .find(|id| id.handle.eq_ignore_ascii_case(candidate))
                        {
                            last = Some(id.handle.clone());
                        }
                    }
                    i = end;
                    continue;
                }
            }
        }
        i += 1;
    }

    last
}

fn last_mentioned_handle_in_thread(
    thread: &relaypod_core::ThreadView,
    identities: &[Identity],
    exclude_handle: Option<&str>,
) -> Option<String> {
    for msg in thread
        .replies
        .iter()
        .rev()
        .chain(std::iter::once(&thread.parent))
    {
        if let Some(handle) = last_mentioned_handle_in_text(&msg.body, identities, exclude_handle) {
            return Some(handle);
        }
    }
    None
}

fn filter_identities<'a>(identities: &'a [Identity], query: &str) -> Vec<&'a Identity> {
    let q = query.to_lowercase();
    let mut prefix_matches = Vec::new();
    let mut contains_matches = Vec::new();

    for id in identities {
        let h = id.handle.to_lowercase();
        if q.is_empty() || h.starts_with(&q) {
            prefix_matches.push(id);
        } else if h.contains(&q) {
            contains_matches.push(id);
        }
    }

    prefix_matches.extend(contains_matches);
    prefix_matches
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CollapsibleSection {
    open: bool,
    search_override: Option<bool>,
}

impl CollapsibleSection {
    fn new(default_open: bool) -> Self {
        Self {
            open: default_open,
            search_override: None,
        }
    }

    fn is_expanded(&self, is_searching: bool, has_matches: bool) -> bool {
        if is_searching {
            self.search_override.unwrap_or(self.open || has_matches)
        } else {
            self.open
        }
    }

    fn toggle(&mut self, is_searching: bool, has_matches: bool) {
        if is_searching {
            let currently_open = self.is_expanded(true, has_matches);
            self.search_override = Some(!currently_open);
        } else {
            self.open = !self.open;
        }
    }

    fn clear_search_override(&mut self) {
        self.search_override = None;
    }
}

#[derive(Debug, Default)]
struct FilteredChannels<'a> {
    active: Vec<&'a Channel>,
    archived: Vec<&'a Channel>,
}

fn channel_matches_normalized_query(channel: &Channel, normalized_query: &str) -> bool {
    channel.name.to_lowercase().contains(normalized_query)
        || channel
            .description
            .to_lowercase()
            .contains(normalized_query)
}

fn filter_channels<'a>(channels: &'a [Channel], query: &str) -> FilteredChannels<'a> {
    let normalized = query.trim().to_lowercase();
    let mut filtered = FilteredChannels::default();
    for channel in channels {
        if !normalized.is_empty() && !channel_matches_normalized_query(channel, &normalized) {
            continue;
        }
        if channel.archived {
            filtered.archived.push(channel);
        } else {
            filtered.active.push(channel);
        }
    }
    let by_name = |a: &&Channel, b: &&Channel| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.name.cmp(&b.name))
    };
    filtered.active.sort_by(by_name);
    filtered.archived.sort_by(by_name);
    filtered
}

fn render_channel_search_box(ui: &mut egui::Ui, query: &mut String) -> egui::Response {
    let row_size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
    ui.allocate_ui_with_layout(
        row_size,
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            let has_query = !query.is_empty();
            if has_query
                && ui
                    .small_button("×")
                    .on_hover_text("Clear channel search")
                    .clicked()
            {
                query.clear();
            }
            let edit_width = ui.available_width().max(1.0);
            let search_resp = ui.add_sized(
                egui::vec2(edit_width, ui.spacing().interact_size.y),
                egui::TextEdit::singleline(query)
                    .id_salt("sidebar_channel_search")
                    .hint_text("Search channels...")
                    .desired_width(edit_width),
            );
            if (search_resp.has_focus() || search_resp.lost_focus())
                && ui.input(|i| i.key_pressed(egui::Key::Escape))
            {
                query.clear();
            }
            search_resp
        },
    )
    .inner
}

fn render_collapsible_section_header(
    ui: &mut egui::Ui,
    title: &str,
    open: bool,
    count: Option<usize>,
) -> bool {
    let resp = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            let (rect, icon_resp) =
                ui.allocate_exact_size(egui::vec2(10.0, 12.0), egui::Sense::click());
            let c = rect.center();
            let color = ui.visuals().weak_text_color();
            let pts = if open {
                vec![
                    egui::pos2(c.x - 3.5, c.y - 2.0),
                    egui::pos2(c.x + 3.5, c.y - 2.0),
                    egui::pos2(c.x, c.y + 3.0),
                ]
            } else {
                vec![
                    egui::pos2(c.x - 2.0, c.y - 3.5),
                    egui::pos2(c.x + 3.0, c.y),
                    egui::pos2(c.x - 2.0, c.y + 3.5),
                ]
            };
            ui.painter()
                .add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));

            let label = match count {
                Some(n) => format!("{title} ({n})"),
                None => title.to_string(),
            };
            let btn_resp =
                ui.add(egui::Button::new(egui::RichText::new(label).strong()).frame(false));
            icon_resp.union(btn_resp)
        })
        .inner;

    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
}

fn apply_mention_completion(
    ctx: &egui::Context,
    input_id: egui::Id,
    text: &mut String,
    mention: &ActiveMention,
    handle: &str,
) {
    let chars: Vec<char> = text.chars().collect();
    let start = mention.start_char.min(chars.len());
    let end = mention.end_char.min(chars.len());

    let before: String = chars[..start].iter().collect();
    let after: String = chars[end..].iter().collect();
    let needs_space = !after.starts_with(' ');
    let inserted = if needs_space {
        format!("@{handle} ")
    } else {
        format!("@{handle}")
    };
    let new_cursor_char = start + inserted.chars().count() + usize::from(!needs_space);

    *text = format!("{before}{inserted}{after}");

    if let Some(mut state) = egui::text_edit::TextEditState::load(ctx, input_id) {
        state
            .cursor
            .set_char_range(Some(CCursorRange::one(CCursor::new(new_cursor_char))));
        state.store(ctx, input_id);
    }
    ctx.memory_mut(|m| m.request_focus(input_id));
}

fn format_inline_channel_description(description: &str) -> String {
    description
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Renders a multiline channel description editor with a live character counter.
/// Returns `true` when the description length is within `MAX_CHANNEL_DESCRIPTION_LEN`.
fn render_channel_description_input(
    ui: &mut egui::Ui,
    id_salt: &str,
    description: &mut String,
) -> bool {
    let max_len = relaypod_core::store::MAX_CHANNEL_DESCRIPTION_LEN;
    let char_count = description.chars().count();
    let over_limit = char_count > max_len;

    ui.horizontal(|ui| {
        ui.label("Description");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let color = if over_limit {
                Color32::LIGHT_RED
            } else {
                ui.visuals().weak_text_color()
            };
            ui.label(
                egui::RichText::new(format!("{char_count}/{max_len}"))
                    .size(11.0)
                    .color(color),
            );
        });
    });

    egui::ScrollArea::vertical()
        .id_salt((id_salt, "channel_desc_scroll"))
        .max_height(200.0)
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(description)
                    .desired_rows(6)
                    .desired_width(f32::INFINITY)
                    .hint_text("Optional channel purpose or context for humans and agents"),
            );
        });

    if over_limit {
        ui.colored_label(
            Color32::LIGHT_RED,
            format!("Description must be at most {max_len} characters."),
        );
    }

    !over_limit
}

/// Renders a taller, rounded multiline composer with `@mention` autocomplete.
/// Returns `true` when the user submits a non-empty message.
fn render_composer(
    ui: &mut egui::Ui,
    id_salt: &str,
    text: &mut String,
    mention_state: &mut MentionState,
    hint_text: &str,
    identities: &[Identity],
) -> bool {
    let input_id = ui.make_persistent_id(id_salt);
    let has_focus = ui.ctx().memory(|m| m.has_focus(input_id));
    let pointer_in_popup = mention_state
        .last_popup_rect
        .zip(ui.ctx().input(|i| i.pointer.latest_pos()))
        .is_some_and(|(rect, pos)| rect.contains(pos));

    let cursor_char = egui::text_edit::TextEditState::load(ui.ctx(), input_id)
        .and_then(|s| s.cursor.char_range())
        .map(|r| r.primary.index)
        .unwrap_or_else(|| text.chars().count());

    let active = find_active_mention(text, cursor_char);
    if active.is_none() {
        mention_state.dismissed_span = None;
    } else if let Some(ref act) = active {
        if let Some((d_start, ref d_query)) = mention_state.dismissed_span {
            if d_start != act.start_char || !act.query.starts_with(d_query) {
                mention_state.dismissed_span = None;
            }
        }
    }

    let matches: Vec<&Identity> = active
        .as_ref()
        .map(|act| filter_identities(identities, &act.query))
        .unwrap_or_default();

    let mut popup_active = (has_focus || pointer_in_popup)
        && active.is_some()
        && !matches.is_empty()
        && mention_state.dismissed_span.is_none();

    let mut send_triggered = false;

    if popup_active {
        if mention_state.selected_idx >= matches.len() {
            mention_state.selected_idx = 0;
        }
        if has_focus {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
                mention_state.selected_idx = (mention_state.selected_idx + 1) % matches.len();
            }
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
                mention_state.selected_idx = if mention_state.selected_idx == 0 {
                    matches.len() - 1
                } else {
                    mention_state.selected_idx - 1
                };
            }
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                if let Some(ref act) = active {
                    mention_state.dismissed_span = Some((act.start_char, act.query.clone()));
                }
                popup_active = false;
            }
            let accept = ui.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)
                    || i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
            });
            if accept {
                if let Some(ref act) = active {
                    let chosen = matches[mention_state.selected_idx].handle.clone();
                    apply_mention_completion(ui.ctx(), input_id, text, act, &chosen);
                    mention_state.selected_idx = 0;
                    popup_active = false;
                }
            }
        }
    } else if has_focus && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
    {
        send_triggered = true;
    }

    let dark = ui.visuals().dark_mode;
    let frame_bg = if dark {
        Color32::from_rgb(30, 33, 40)
    } else {
        Color32::from_rgb(245, 247, 250)
    };
    let border_color = if has_focus {
        Color32::from_rgb(96, 150, 248)
    } else if dark {
        Color32::from_rgb(58, 63, 75)
    } else {
        Color32::from_rgb(205, 210, 220)
    };

    let frame_resp = egui::Frame::new()
        .fill(frame_bg)
        .stroke(Stroke::new(
            if has_focus { 1.5_f32 } else { 1.0_f32 },
            border_color,
        ))
        .corner_radius(CornerRadius::same(14))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                let send_btn_width = 72.0;
                let can_send = !text.trim().is_empty();
                let btn_fill = if can_send {
                    Color32::from_rgb(59, 130, 246)
                } else if dark {
                    Color32::from_rgb(48, 52, 62)
                } else {
                    Color32::from_rgb(215, 220, 228)
                };
                let btn_text = if can_send {
                    Color32::WHITE
                } else {
                    ui.visuals().weak_text_color()
                };

                let btn = egui::Button::new(egui::RichText::new("Send").color(btn_text))
                    .fill(btn_fill)
                    .corner_radius(CornerRadius::same(10))
                    .min_size(egui::vec2(send_btn_width, 32.0));

                if ui.add_enabled(can_send, btn).clicked() {
                    send_triggered = true;
                }

                ui.add_space(4.0);
                let edit_width = ui.available_width().max(40.0);
                ui.vertical(|ui| {
                    ui.set_width(edit_width);
                    ui.set_max_width(edit_width);
                    egui::ScrollArea::vertical()
                        .id_salt((id_salt, "composer_scroll"))
                        .max_height(140.0)
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(text)
                                    .id(input_id)
                                    .frame(false)
                                    .hint_text(hint_text)
                                    .desired_rows(2)
                                    .desired_width(edit_width)
                                    .min_size(egui::vec2(edit_width, 44.0)),
                            );
                        });
                });
            });
        });

    if popup_active {
        if let Some(ref act) = active {
            let anchor = frame_resp.response.rect.left_top() - egui::vec2(0.0, 6.0);
            let popup_id = ui.make_persistent_id(format!("{id_salt}_mention_popup"));
            let popup_width = frame_resp.response.rect.width().clamp(200.0, 320.0);

            let area_resp = egui::Area::new(popup_id)
                .order(egui::Order::Foreground)
                .fixed_pos(anchor)
                .pivot(egui::Align2::LEFT_BOTTOM)
                .show(ui.ctx(), |ui| {
                    let popup_bg = if dark {
                        Color32::from_rgb(26, 29, 36)
                    } else {
                        Color32::WHITE
                    };
                    let popup_border = if dark {
                        Color32::from_rgb(65, 72, 86)
                    } else {
                        Color32::from_rgb(200, 206, 218)
                    };

                    egui::Frame::new()
                        .fill(popup_bg)
                        .stroke(Stroke::new(1.0_f32, popup_border))
                        .corner_radius(CornerRadius::same(10))
                        .inner_margin(Margin::same(6))
                        .shadow(egui::Shadow {
                            offset: [0, 4],
                            blur: 12,
                            spread: 0,
                            color: Color32::from_black_alpha(90),
                        })
                        .show(ui, |ui| {
                            ui.set_width(popup_width);
                            egui::ScrollArea::vertical()
                                .max_height(200.0)
                                .show(ui, |ui| {
                                    for (idx, id) in matches.iter().enumerate() {
                                        let is_selected = idx == mention_state.selected_idx;
                                        let row_bg = if is_selected {
                                            if dark {
                                                Color32::from_rgb(47, 66, 98)
                                            } else {
                                                Color32::from_rgb(219, 234, 254)
                                            }
                                        } else {
                                            Color32::TRANSPARENT
                                        };

                                        let row_resp = egui::Frame::new()
                                            .fill(row_bg)
                                            .corner_radius(CornerRadius::same(6))
                                            .inner_margin(Margin::symmetric(8, 6))
                                            .show(ui, |ui| {
                                                ui.set_width(ui.available_width());
                                                ui.horizontal(|ui| {
                                                    draw_kind_badge(ui, id.kind);
                                                    ui.strong(format!("@{}", id.handle));
                                                });
                                            })
                                            .response
                                            .interact(egui::Sense::click());

                                        if row_resp.hovered() {
                                            mention_state.selected_idx = idx;
                                        }
                                        if row_resp.clicked() {
                                            apply_mention_completion(
                                                ui.ctx(),
                                                input_id,
                                                text,
                                                act,
                                                &id.handle,
                                            );
                                            mention_state.selected_idx = 0;
                                            mention_state.last_popup_rect = None;
                                        }
                                    }
                                });
                        });
                });
            mention_state.last_popup_rect = Some(area_resp.response.rect);
        }
    } else {
        mention_state.last_popup_rect = None;
    }

    send_triggered && !text.trim().is_empty()
}

fn draw_status_dot(ui: &mut egui::Ui, color: Color32, filled: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    let center = rect.center();
    if filled {
        ui.painter().circle_filled(center, 4.0, color);
    } else {
        ui.painter()
            .circle_stroke(center, 3.5, Stroke::new(1.5_f32, color));
    }
}

fn draw_presence_dot(ui: &mut egui::Ui, online: bool) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    let center = rect.center();
    if online {
        ui.painter()
            .circle_filled(center, 3.6, Color32::from_rgb(74, 222, 128));
        resp.on_hover_text("Live");
    } else {
        let dim = if ui.visuals().dark_mode {
            Color32::from_rgb(75, 82, 96)
        } else {
            Color32::from_rgb(185, 192, 204)
        };
        ui.painter()
            .circle_stroke(center, 3.0, Stroke::new(1.2_f32, dim));
        resp.on_hover_text("Offline");
    }
}

fn draw_kind_badge(ui: &mut egui::Ui, kind: IdentityKind) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
    let c = rect.center();
    match kind {
        IdentityKind::Human => {
            ui.painter()
                .circle_filled(c, 3.8, Color32::from_rgb(96, 165, 250));
        }
        IdentityKind::Agent => {
            let r = 4.2;
            let pts = vec![
                egui::pos2(c.x, c.y - r),
                egui::pos2(c.x + r, c.y),
                egui::pos2(c.x, c.y + r),
                egui::pos2(c.x - r, c.y),
            ];
            ui.painter().add(egui::Shape::convex_polygon(
                pts,
                Color32::from_rgb(167, 139, 250),
                Stroke::NONE,
            ));
        }
    }
}

fn draw_activity_dots(ui: &mut egui::Ui, state: ActivityState) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(26.0, 14.0), egui::Sense::hover());
    let base_y = rect.center().y + 1.0;

    match state {
        ActivityState::Working => {
            let time = ui.ctx().input(|i| i.time) as f32;
            let accent = Color32::from_rgb(167, 139, 250);
            for idx in 0..3 {
                let phase = time * 5.0_f32 - (idx as f32) * 0.75_f32;
                let bounce = phase.sin().max(0.0);
                let x = rect.left() + 4.0 + (idx as f32) * 9.0;
                let y = base_y - bounce * 4.0;
                let alpha = (130.0 + bounce * 125.0) as u8;
                let color =
                    Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha);
                ui.painter().circle_filled(egui::pos2(x, y), 2.5, color);
            }
            ui.ctx().request_repaint();
        }
        ActivityState::Blocked => {
            let color = if ui.visuals().dark_mode {
                Color32::from_rgb(248, 113, 113)
            } else {
                Color32::from_rgb(220, 38, 38)
            };
            for idx in 0..3 {
                let x = rect.left() + 4.0 + (idx as f32) * 9.0;
                ui.painter()
                    .circle_filled(egui::pos2(x, base_y), 2.5, color);
            }
        }
    }
}

fn format_workers_label(handles: &[String], state: ActivityState) -> String {
    match state {
        ActivityState::Working => match handles {
            [] => String::new(),
            [one] => format!("@{one} is working..."),
            [a, b] => format!("@{a} and @{b} are working..."),
            many => format!("@{} and {} others are working...", many[0], many.len() - 1),
        },
        ActivityState::Blocked => match handles {
            [] => String::new(),
            [one] => format!("@{one} is blocked — waiting for input"),
            [a, b] => format!("@{a} and @{b} are blocked — waiting for input"),
            many => format!(
                "@{} and {} others are blocked — waiting for input",
                many[0],
                many.len() - 1
            ),
        },
    }
}

fn render_activity_banner(ui: &mut egui::Ui, handles: &[String], state: ActivityState) {
    let dark = ui.visuals().dark_mode;
    let (fill, border, text_color) = match state {
        ActivityState::Working => {
            if dark {
                (
                    Color32::from_rgb(42, 38, 56),
                    Color32::from_rgb(92, 74, 140),
                    Color32::from_rgb(216, 204, 255),
                )
            } else {
                (
                    Color32::from_rgb(243, 238, 255),
                    Color32::from_rgb(196, 181, 253),
                    Color32::from_rgb(91, 33, 182),
                )
            }
        }
        ActivityState::Blocked => {
            if dark {
                (
                    Color32::from_rgb(56, 30, 34),
                    Color32::from_rgb(165, 56, 66),
                    Color32::from_rgb(254, 202, 202),
                )
            } else {
                (
                    Color32::from_rgb(254, 242, 242),
                    Color32::from_rgb(252, 165, 165),
                    Color32::from_rgb(153, 27, 27),
                )
            }
        }
    };

    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                draw_activity_dots(ui, state);
                ui.label(
                    egui::RichText::new(format_workers_label(handles, state))
                        .size(12.0)
                        .color(text_color),
                );
            });
        });
}

fn render_blocked_banner(
    ui: &mut egui::Ui,
    worker: &ActiveWorker,
    default_peek_open: bool,
) -> Option<String> {
    let dark = ui.visuals().dark_mode;
    let (fill, border, text_color, peek_bg, peek_border, peek_text) = if dark {
        (
            Color32::from_rgb(56, 30, 34),
            Color32::from_rgb(165, 56, 66),
            Color32::from_rgb(254, 202, 202),
            Color32::from_rgb(34, 20, 24),
            Color32::from_rgb(110, 42, 50),
            Color32::from_rgb(245, 224, 224),
        )
    } else {
        (
            Color32::from_rgb(254, 242, 242),
            Color32::from_rgb(252, 165, 165),
            Color32::from_rgb(153, 27, 27),
            Color32::from_rgb(255, 250, 250),
            Color32::from_rgb(254, 202, 202),
            Color32::from_rgb(80, 20, 20),
        )
    };

    let peek_id = ui.make_persistent_id((
        "blocked_peek_open",
        &worker.identity_id,
        &worker.channel_id,
        &worker.thread_id,
        default_peek_open,
    ));
    let mut peek_open = ui
        .data_mut(|d| d.get_temp::<bool>(peek_id))
        .unwrap_or(default_peek_open);
    let mut focus_pane: Option<String> = None;

    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                draw_activity_dots(ui, ActivityState::Blocked);
                ui.label(
                    egui::RichText::new(format_workers_label(
                        std::slice::from_ref(&worker.handle),
                        ActivityState::Blocked,
                    ))
                    .size(12.0)
                    .color(text_color),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(ref pane_id) = worker.pane_id {
                        if ui
                            .small_button("Focus in Herdr")
                            .on_hover_text(format!("Focus pane {pane_id} in Herdr"))
                            .clicked()
                        {
                            focus_pane = Some(pane_id.clone());
                        }
                    }
                    if worker.detail.is_some() {
                        let toggle_label = if peek_open { "Hide peek" } else { "Peek" };
                        if ui
                            .small_button(toggle_label)
                            .on_hover_text("Toggle read-only preview of the blocking prompt")
                            .clicked()
                        {
                            peek_open = !peek_open;
                            ui.data_mut(|d| d.insert_temp(peek_id, peek_open));
                        }
                    }
                });
            });

            if peek_open {
                if let Some(ref detail) = worker.detail {
                    ui.add_space(6.0);
                    egui::Frame::new()
                        .fill(peek_bg)
                        .stroke(Stroke::new(1.0_f32, peek_border))
                        .corner_radius(CornerRadius::same(6))
                        .inner_margin(Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            egui::ScrollArea::both()
                                .id_salt(peek_id.with("scroll"))
                                .max_height(180.0)
                                .auto_shrink([false, true])
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(detail)
                                                .monospace()
                                                .size(11.5)
                                                .color(peek_text),
                                        )
                                        .selectable(true),
                                    );
                                });
                        });
                }
            }
        });

    focus_pane
}

fn render_activity_banners(
    ui: &mut egui::Ui,
    workers: &[ActiveWorker],
    default_peek_open: bool,
) -> Option<String> {
    let blocked: Vec<&ActiveWorker> = workers
        .iter()
        .filter(|w| w.state == ActivityState::Blocked)
        .collect();
    let working: Vec<String> = workers
        .iter()
        .filter(|w| w.state == ActivityState::Working)
        .map(|w| w.handle.clone())
        .collect();

    let mut focus_pane: Option<String> = None;
    for (idx, worker) in blocked.iter().enumerate() {
        if idx > 0 {
            ui.add_space(4.0);
        }
        if let Some(pane_id) = render_blocked_banner(ui, worker, default_peek_open) {
            focus_pane = Some(pane_id);
        }
    }
    if !working.is_empty() {
        if !blocked.is_empty() {
            ui.add_space(4.0);
        }
        render_activity_banner(ui, &working, ActivityState::Working);
    }
    focus_pane
}

enum MessageCardAction {
    None,
    OpenThread,
    AckThread,
    Delete,
    FocusHerdrPane(String),
}

fn render_message_card(
    ui: &mut egui::Ui,
    msg: &Message,
    can_delete: bool,
    workers: &[ActiveWorker],
    unread_count: usize,
) -> MessageCardAction {
    let mut action = MessageCardAction::None;
    let frame_resp = egui::Frame::group(ui.style())
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                draw_kind_badge(ui, msg.author_kind);
                ui.strong(format!("@{}", msg.author_handle));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if msg.reply_count > 0 {
                        format!("{} replies", msg.reply_count)
                    } else {
                        "Thread".to_string()
                    };
                    if ui.small_button(label).clicked() {
                        action = MessageCardAction::OpenThread;
                    }
                });
            });
            ui.add_space(4.0);
            if unread_count > 0 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    let ack_label = if unread_count > 1 {
                        format!("Ack ({unread_count})")
                    } else {
                        "Ack".to_string()
                    };
                    if ui
                        .small_button(ack_label)
                        .on_hover_text("Acknowledge all unread items in this thread")
                        .clicked()
                    {
                        action = MessageCardAction::AckThread;
                    }
                    ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                        ui.set_width(ui.available_width());
                        markdown::render_markdown(ui, &msg.body);
                    });
                });
            } else {
                markdown::render_markdown(ui, &msg.body);
            }
            if !workers.is_empty() {
                ui.add_space(6.0);
                if let Some(pane_id) = render_activity_banners(ui, workers, false) {
                    action = MessageCardAction::FocusHerdrPane(pane_id);
                }
            }
        });

    let popup_id = ui.make_persistent_id(("msg_card_ctx", &msg.id));
    let pos_id = popup_id.with("pos");
    if ui.rect_contains_pointer(frame_resp.response.rect)
        && ui.input(|i| i.pointer.secondary_clicked())
    {
        if let Some(pos) = ui.input(|i| i.pointer.interact_pos()) {
            ui.data_mut(|d| d.insert_temp(pos_id, pos));
            ui.memory_mut(|m| m.open_popup(popup_id));
        }
    }

    if ui.memory(|m| m.is_popup_open(popup_id)) {
        let pos = ui
            .data(|d| d.get_temp::<egui::Pos2>(pos_id))
            .unwrap_or(frame_resp.response.rect.left_top());
        let area_resp = egui::Area::new(popup_id)
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .constrain(true)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_min_width(120.0);
                    if ui.button("Open thread").clicked() {
                        action = MessageCardAction::OpenThread;
                        ui.memory_mut(|m| m.close_popup());
                    }
                    if can_delete && ui.button("Delete").clicked() {
                        action = MessageCardAction::Delete;
                        ui.memory_mut(|m| m.close_popup());
                    }
                });
            });

        if ui.input(|i| i.key_pressed(egui::Key::Escape))
            || (ui.input(|i| i.pointer.any_pressed())
                && !area_resp.response.contains_pointer()
                && !ui.input(|i| i.pointer.secondary_clicked()))
        {
            ui.memory_mut(|m| m.close_popup());
        }
    }

    action
}

/// Renders a thread message as a chat bubble:
/// - Own messages (`is_me == true`): left-aligned with space on the right.
/// - Other party's messages (`is_me == false`): right-aligned with space on the left.
fn render_thread_bubble(ui: &mut egui::Ui, msg: &Message, is_me: bool) {
    let dark = ui.visuals().dark_mode;
    let total_width = ui.available_width();
    let side_gap = (total_width * 0.16).clamp(32.0, 72.0).round() as i8;

    let (fill, border) = if is_me {
        if dark {
            (
                Color32::from_rgb(32, 50, 74),
                Color32::from_rgb(56, 86, 128),
            )
        } else {
            (
                Color32::from_rgb(219, 234, 254),
                Color32::from_rgb(147, 197, 253),
            )
        }
    } else if dark {
        (Color32::from_rgb(38, 41, 49), Color32::from_rgb(60, 65, 78))
    } else {
        (
            Color32::from_rgb(241, 243, 247),
            Color32::from_rgb(212, 217, 226),
        )
    };

    let outer = if is_me {
        Margin {
            left: 0,
            right: side_gap,
            top: 0,
            bottom: 0,
        }
    } else {
        Margin {
            left: side_gap,
            right: 0,
            top: 0,
            bottom: 0,
        }
    };

    egui::Frame::new()
        .outer_margin(outer)
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                draw_kind_badge(ui, msg.author_kind);
                ui.strong(format!("@{}", msg.author_handle));
            });
            ui.add_space(3.0);
            markdown::render_markdown(ui, &msg.body);
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_active_mention_tokens() {
        assert_eq!(
            find_active_mention("@", 1),
            Some(ActiveMention {
                start_char: 0,
                end_char: 1,
                query: "".into()
            })
        );
        assert_eq!(
            find_active_mention("hello @cl", 9),
            Some(ActiveMention {
                start_char: 6,
                end_char: 9,
                query: "cl".into()
            })
        );
        assert_eq!(find_active_mention("email@example.com", 10), None);
        assert_eq!(find_active_mention("hello @claude ", 14), None);
    }

    #[test]
    fn finds_last_mentioned_handle_excluding_self() {
        let mk = |handle: &str, kind: IdentityKind| Identity {
            id: handle.into(),
            handle: handle.into(),
            kind,
            owner_id: None,
            unread_count: 0,
            online: false,
            busy: false,
            is_admin: false,
            created_at: 0,
            agent_config: None,
        };
        let ids = vec![
            mk("alice", IdentityKind::Human),
            mk("codex-local", IdentityKind::Agent),
            mk("claude", IdentityKind::Agent),
        ];

        assert_eq!(
            last_mentioned_handle_in_text(
                "Hey @codex-local and @claude, check with @alice",
                &ids,
                Some("alice"),
            )
            .as_deref(),
            Some("claude")
        );
        assert_eq!(
            last_mentioned_handle_in_text("email@codex-local.com @unknown", &ids, Some("alice")),
            None
        );
    }

    #[test]
    fn filters_channels_case_insensitively_by_name_and_description() {
        let mk = |id: &str, name: &str, description: &str, archived: bool| Channel {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            archived,
            archived_at: archived.then_some(1),
            created_at: 0,
            created_by: None,
        };
        let channels = vec![
            mk("c2", "relaypod", "Backend and UI work", false),
            mk("c4", "Alpha", "Top alphabetically", false),
            mk("c1", "general", "Shared workspace", false),
            mk("c5", "zeta-old", "Oldest archive", true),
            mk("c3", "legacy-ops", "Old incident triage", true),
        ];

        let all = filter_channels(&channels, "   ");
        assert_eq!(
            all.active
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Alpha", "general", "relaypod"]
        );
        assert_eq!(
            all.archived
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["legacy-ops", "zeta-old"]
        );

        let by_name = filter_channels(&channels, "RELAY");
        assert_eq!(by_name.active.len(), 1);
        assert_eq!(by_name.active[0].id, "c2");
        assert!(by_name.archived.is_empty());

        let by_desc = filter_channels(&channels, "TRIAGE");
        assert!(by_desc.active.is_empty());
        assert_eq!(by_desc.archived.len(), 1);
        assert_eq!(by_desc.archived[0].id, "c3");

        let none = filter_channels(&channels, "nonexistent");
        assert!(none.active.is_empty());
        assert!(none.archived.is_empty());
    }

    #[test]
    fn collapsible_section_stays_foldable_during_search_and_restores_preference() {
        let mut archived = CollapsibleSection::new(false);
        assert!(!archived.is_expanded(false, true));

        // Auto-expands during search when matches exist.
        assert!(archived.is_expanded(true, true));

        // Clicking during search folds immediately without mutating saved preference.
        archived.toggle(true, true);
        assert!(!archived.is_expanded(true, true));
        assert!(!archived.open);

        // Clicking again unfolds during search.
        archived.toggle(true, true);
        assert!(archived.is_expanded(true, true));
        assert!(!archived.open);

        // Clearing search restores pre-search folded state.
        archived.clear_search_override();
        assert!(!archived.is_expanded(false, true));

        // Clicking outside search updates saved preference.
        archived.toggle(false, true);
        assert!(archived.is_expanded(false, true));
    }

    #[test]
    fn escape_clears_focused_channel_search_input() {
        let ctx = egui::Context::default();
        let mut query = "legacy".to_string();

        // First pass: render search input and focus it.
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let resp = render_channel_search_box(ui, &mut query);
                resp.request_focus();
            });
        });
        assert_eq!(query, "legacy");

        // Second pass: press Escape while focused.
        let esc_input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let _ = ctx.run(esc_input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render_channel_search_box(ui, &mut query);
            });
        });
        assert!(query.is_empty(), "expected Escape to clear search query");
    }

    #[test]
    fn channel_search_box_does_not_expand_sidebar_width() {
        let ctx = egui::Context::default();
        configure_style(&ctx);

        let run_frame = |query: &mut String| -> f32 {
            let mut panel_width = 0.0;
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                let resp = egui::SidePanel::left("sidebar")
                    .default_width(220.0)
                    .show(ctx, |ui| {
                        render_channel_search_box(ui, query);
                    });
                panel_width = resp.response.rect.width();
            });
            panel_width
        };

        let mut query = String::new();
        let initial_width = run_frame(&mut query);
        let baseline_width = run_frame(&mut query);
        assert!(
            (baseline_width - initial_width).abs() < 0.1,
            "baseline width drifted: {initial_width} -> {baseline_width}"
        );

        query = "a".to_string();
        for frame in 0..10 {
            let width = run_frame(&mut query);
            assert!(
                (width - baseline_width).abs() < 0.1,
                "sidebar expanded on frame {frame} with query {query:?}: baseline {baseline_width}, got {width}"
            );
        }

        query = "very-long-channel-search-query-that-exceeds-sidebar-width".to_string();
        for frame in 0..5 {
            let width = run_frame(&mut query);
            assert!(
                (width - baseline_width).abs() < 0.1,
                "sidebar expanded on frame {frame} with long query: baseline {baseline_width}, got {width}"
            );
        }
    }

    #[test]
    fn composer_never_overflows_into_thread_pane_on_narrow_window() {
        let ctx = egui::Context::default();
        configure_style(&ctx);

        let mut channel_text = String::new();
        let mut channel_mention = MentionState::default();
        let mut thread_text = String::new();
        let mut thread_mention = MentionState::default();

        for screen_w in [820.0_f32, 640.0_f32] {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(screen_w, 600.0),
                )),
                ..Default::default()
            };

            let mut thread_left = 0.0_f32;
            let mut composer_right = 0.0_f32;

            for _ in 0..3 {
                let _ = ctx.run(input.clone(), |ctx| {
                    egui::SidePanel::left("sidebar")
                        .default_width(220.0)
                        .show(ctx, |ui| {
                            ui.label("Sidebar");
                        });

                    const MIN_PANE_WIDTH: f32 = 300.0;
                    let available_w = ctx.available_rect().width();
                    let min_thread_w = MIN_PANE_WIDTH.min((available_w * 0.5).max(160.0));
                    let max_thread_w = (available_w - MIN_PANE_WIDTH)
                        .min(available_w * 0.75)
                        .max(min_thread_w);
                    let default_thread_w = (available_w * 0.5).clamp(min_thread_w, max_thread_w);

                    let thread_resp = egui::SidePanel::right("thread_drawer_test")
                        .default_width(default_thread_w)
                        .min_width(min_thread_w)
                        .max_width(max_thread_w)
                        .show(ctx, |ui| {
                            egui::TopBottomPanel::bottom("thread_composer_test")
                                .frame(egui::Frame::new().inner_margin(Margin {
                                    left: 4,
                                    right: 4,
                                    top: 8,
                                    bottom: 4,
                                }))
                                .show_inside(ui, |ui| {
                                    render_composer(
                                        ui,
                                        "thread_input_test",
                                        &mut thread_text,
                                        &mut thread_mention,
                                        "Reply in thread... (@ to mention, Shift+Enter for newline)",
                                        &[],
                                    );
                                });
                        });
                    thread_left = thread_resp.response.rect.left();

                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            ui.heading("# general");
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(
                                        "Shared workspace for humans and agents with a very long description",
                                    )
                                    .color(ui.visuals().weak_text_color()),
                                )
                                .truncate(),
                            );
                        });
                        ui.separator();

                        let composer_resp = egui::TopBottomPanel::bottom("channel_composer_test")
                            .frame(egui::Frame::new().inner_margin(Margin {
                                left: 4,
                                right: 4,
                                top: 8,
                                bottom: 4,
                            }))
                            .show_inside(ui, |ui| {
                                render_composer(
                                    ui,
                                    "channel_input_test",
                                    &mut channel_text,
                                    &mut channel_mention,
                                    "Message #general (@ to mention, Shift+Enter for newline)",
                                    &[],
                                );
                            });
                        composer_right = composer_resp.response.rect.right();
                    });
                });
            }

            assert!(
                composer_right <= thread_left + 0.5,
                "at window width {screen_w}, channel composer right ({composer_right}) overflowed into thread pane left ({thread_left})"
            );
        }
    }

    #[test]
    fn composer_height_stabilizes_for_multiline_text() {
        let ctx = egui::Context::default();
        configure_style(&ctx);

        let mut mention = MentionState::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(500.0, 600.0),
            )),
            ..Default::default()
        };

        let samples = [
            "short message".to_string(),
            "This is a longer message that wraps across multiple lines inside the composer box so we can verify that the bottom panel settles cleanly without growing on every frame.".to_string(),
            (0..15).map(|i| format!("Line {i}: some content here")).collect::<Vec<_>>().join("\n"),
        ];

        for mut text in samples {
            let mut heights = Vec::new();
            for _ in 0..12 {
                let mut h = 0.0_f32;
                let _ = ctx.run(input.clone(), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let resp = egui::TopBottomPanel::bottom("composer_stability_test")
                            .frame(egui::Frame::new().inner_margin(Margin {
                                left: 4,
                                right: 4,
                                top: 8,
                                bottom: 4,
                            }))
                            .show_inside(ui, |ui| {
                                render_composer(
                                    ui,
                                    "composer_stability_input",
                                    &mut text,
                                    &mut mention,
                                    "Message #general",
                                    &[],
                                );
                            });
                        h = resp.response.rect.height();
                    });
                });
                heights.push(h);
            }

            let settled = heights[3];
            for (frame, &h) in heights.iter().enumerate().skip(4) {
                assert!(
                    (h - settled).abs() < 0.5,
                    "composer height kept drifting on frame {frame}: settled at {settled}, got {h} (all: {heights:?})"
                );
            }
            assert!(
                settled <= 200.0,
                "composer height should be capped by scroll area, got {settled}"
            );
        }
    }

    #[test]
    fn formats_watcher_pill_summary_for_all_states() {
        let off = watch::WatcherSnapshot::default();
        assert_eq!(watcher_pill_summary(&off).0, "Watcher: Off");

        let err = watch::WatcherSnapshot {
            last_error: Some("bad yaml".into()),
            ..Default::default()
        };
        assert_eq!(watcher_pill_summary(&err).0, "Watcher (err)");

        let mk_target = |name: &str, phase: TargetPhase| watch::WatchedTargetState {
            token_name: name.into(),
            identity_id: Some("id1".into()),
            handle: Some(name.into()),
            runner: Some("herdr".into()),
            harness: Some("claude".into()),
            phase,
        };

        let connected = watch::WatcherSnapshot {
            running: true,
            targets: vec![
                mk_target("claude", TargetPhase::Connected),
                mk_target("codex", TargetPhase::Connected),
            ],
            ..Default::default()
        };
        let (label, color, pulse) = watcher_pill_summary(&connected);
        assert_eq!(label, "Watcher (2)");
        assert_eq!(color, Color32::LIGHT_GREEN);
        assert!(pulse);

        let working = watch::WatcherSnapshot {
            running: true,
            targets: vec![
                mk_target(
                    "claude",
                    TargetPhase::Working {
                        channel: "general".into(),
                    },
                ),
                mk_target("codex", TargetPhase::Connected),
            ],
            ..Default::default()
        };
        let (label, _, pulse) = watcher_pill_summary(&working);
        assert_eq!(label, "Watcher (2 • working)");
        assert!(pulse);

        let blocked = watch::WatcherSnapshot {
            running: true,
            targets: vec![
                mk_target(
                    "claude",
                    TargetPhase::Blocked {
                        channel: "general".into(),
                    },
                ),
                mk_target(
                    "codex",
                    TargetPhase::Working {
                        channel: "general".into(),
                    },
                ),
            ],
            ..Default::default()
        };
        let (label, color, pulse) = watcher_pill_summary(&blocked);
        assert_eq!(label, "Watcher (2 • blocked)");
        assert_eq!(color, Color32::from_rgb(248, 113, 113));
        assert!(pulse);

        assert_eq!(
            format_workers_label(&["codex".into()], ActivityState::Working),
            "@codex is working..."
        );
        assert_eq!(
            format_workers_label(&["codex".into()], ActivityState::Blocked),
            "@codex is blocked — waiting for input"
        );
    }

    #[test]
    fn channel_description_input_is_multiline_and_validates_limit() {
        let ctx = egui::Context::default();
        configure_style(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(520.0, 600.0),
            )),
            ..Default::default()
        };

        let mut desc = "Repo: ~/projects/relaypod\nFocus: watcher & UI".to_string();
        let mut valid = false;
        let mut box_height = 0.0_f32;
        let _ = ctx.run(input.clone(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_width(460.0);
                let top = ui.cursor().top();
                valid = render_channel_description_input(ui, "test_desc", &mut desc);
                box_height = ui.cursor().top() - top;
            });
        });
        assert!(valid);
        assert!(
            box_height >= 90.0,
            "expected spacious multiline description editor, got height {box_height}"
        );

        let mut too_long = "x".repeat(relaypod_core::store::MAX_CHANNEL_DESCRIPTION_LEN + 1);
        let mut over_valid = true;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_width(460.0);
                over_valid = render_channel_description_input(ui, "test_desc_over", &mut too_long);
            });
        });
        assert!(!over_valid);

        assert_eq!(
            format_inline_channel_description("  Line 1 \n\n  Line 2  "),
            "Line 1 Line 2"
        );
    }
}
