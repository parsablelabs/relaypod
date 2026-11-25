mod markdown;

use eframe::egui;
use egui::text::{CCursor, CCursorRange};
use egui::{Color32, CornerRadius, Margin, Stroke};
use relaypod_client::{
    mcp_config_snippet, spawn_runtime, ClientCommand, ClientRuntime, WorkspaceState,
};
use relaypod_core::{
    Channel, CreatedIdentity, Identity, IdentityKind, Message, UpdateChannelRequest,
};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_title("RelayPod"),
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

/// Pure `egui` presentation layer over `relaypod_client::{WorkspaceState, ClientRuntime}`.
struct RelayPodApp {
    runtime: ClientRuntime,
    state: WorkspaceState,

    // View-only form inputs & modal flags
    server_url: String,
    token: String,
    show_unread_drawer: bool,

    channel_composer: String,
    channel_mention: MentionState,
    thread_composer: String,
    thread_mention: MentionState,
    prefilled_thread_id: Option<String>,

    show_new_channel_modal: bool,
    new_channel_name: String,
    new_channel_desc: String,

    show_new_agent_modal: bool,
    new_agent_handle: String,
    new_agent_display: String,
    new_agent_kind: IdentityKind,
    last_created_identity: Option<CreatedIdentity>,

    pending_delete_message: Option<Message>,
    pending_delete_channel: Option<Channel>,
    channel_edit: Option<Channel>,

    channel_search: String,
    channels_section_open: bool,
    archived_channels_section_open: bool,
}

impl RelayPodApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_style(&cc.egui_ctx);

        let egui_ctx = cc.egui_ctx.clone();
        let runtime = spawn_runtime(move || egui_ctx.request_repaint());

        let server_url =
            std::env::var("RELAYPOD_URL").unwrap_or_else(|_| "http://127.0.0.1:8787".to_string());
        let token = std::env::var("RELAYPOD_TOKEN").unwrap_or_default();

        if !token.is_empty() {
            runtime.send(ClientCommand::Connect {
                base_url: server_url.clone(),
                token: token.clone(),
            });
        }

        Self {
            runtime,
            state: WorkspaceState::default(),
            server_url,
            token,
            show_unread_drawer: false,
            channel_composer: String::new(),
            channel_mention: MentionState::default(),
            thread_composer: String::new(),
            thread_mention: MentionState::default(),
            prefilled_thread_id: None,
            show_new_channel_modal: false,
            new_channel_name: String::new(),
            new_channel_desc: String::new(),
            show_new_agent_modal: false,
            new_agent_handle: String::new(),
            new_agent_display: String::new(),
            new_agent_kind: IdentityKind::Agent,
            last_created_identity: None,
            pending_delete_message: None,
            pending_delete_channel: None,
            channel_edit: None,
            channel_search: String::new(),
            channels_section_open: true,
            archived_channels_section_open: false,
        }
    }

    fn drain_events(&mut self) {
        while let Some(ev) = self.runtime.try_recv() {
            let effects = self.state.apply(ev);
            for cmd in effects.commands {
                self.runtime.send(cmd);
            }
            if effects.channel_just_created {
                self.show_new_channel_modal = false;
                self.new_channel_name.clear();
                self.new_channel_desc.clear();
            }
            if let Some(created) = effects.identity_just_created {
                self.last_created_identity = Some(created);
                self.new_agent_handle.clear();
                self.new_agent_display.clear();
            }
        }
    }

    fn select_channel(&mut self, channel_id: String) {
        let cmd = self.state.select_channel(channel_id);
        self.runtime.send(cmd);
    }

    fn can_manage_channels(&self) -> bool {
        self.state
            .me
            .as_ref()
            .is_some_and(|me| me.kind == IdentityKind::Human)
    }

    fn render_channel_row(&mut self, ui: &mut egui::Ui, channel: &Channel) {
        let selected = self.state.selected_channel_id.as_deref() == Some(channel.id.as_str());
        let row = ui.selectable_label(selected, format!("# {}", channel.name));
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
                if ui.button("Delete").clicked() {
                    self.pending_delete_channel = Some(channel.clone());
                    ui.close_menu();
                }
            });
        }
    }

    fn render_connect_screen(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(120.0);
                ui.heading("RelayPod");
                ui.label("Shared message board for humans & AI agents");
                ui.add_space(20.0);

                egui::Frame::group(ui.style())
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(16.0)
                    .show(ui, |ui| {
                        ui.set_width(420.0);
                        ui.label("Server URL");
                        ui.text_edit_singleline(&mut self.server_url);
                        ui.add_space(8.0);
                        ui.label("Bearer Token (RELAYPOD_TOKEN)");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.token)
                                .password(true)
                                .hint_text("rp_..."),
                        );
                        ui.add_space(12.0);
                        if ui.button("Connect").clicked() && !self.token.trim().is_empty() {
                            self.state.status_error = None;
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
            egui::Window::new("New Channel")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("Channel name (without #)");
                    ui.text_edit_singleline(&mut self.new_channel_name);
                    ui.label("Description");
                    ui.text_edit_singleline(&mut self.new_channel_desc);
                    ui.horizontal(|ui| {
                        if ui.button("Create").clicked() && !self.new_channel_name.trim().is_empty()
                        {
                            self.runtime.send(ClientCommand::CreateChannel {
                                name: self.new_channel_name.clone(),
                                description: self.new_channel_desc.clone(),
                            });
                        }
                        if ui.button("Cancel").clicked() {
                            self.show_new_channel_modal = false;
                        }
                    });
                });
        }

        if self.show_new_agent_modal {
            egui::Window::new("Register Identity / Agent")
                .collapsible(false)
                .default_width(460.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Kind:");
                        ui.selectable_value(&mut self.new_agent_kind, IdentityKind::Agent, "Agent");
                        ui.selectable_value(&mut self.new_agent_kind, IdentityKind::Human, "Human");
                    });
                    ui.label("Handle (e.g. claude-local)");
                    ui.text_edit_singleline(&mut self.new_agent_handle);
                    ui.label("Display name (optional)");
                    ui.text_edit_singleline(&mut self.new_agent_display);

                    ui.horizontal(|ui| {
                        if ui.button("Create").clicked() && !self.new_agent_handle.trim().is_empty()
                        {
                            let display_name = if self.new_agent_display.trim().is_empty() {
                                None
                            } else {
                                Some(self.new_agent_display.clone())
                            };
                            self.runtime.send(ClientCommand::CreateIdentity {
                                handle: self.new_agent_handle.clone(),
                                display_name,
                                kind: self.new_agent_kind,
                            });
                        }
                        if ui.button("Close").clicked() {
                            self.show_new_agent_modal = false;
                            self.last_created_identity = None;
                        }
                    });

                    if let Some(created) = &self.last_created_identity {
                        ui.separator();
                        ui.colored_label(
                            Color32::LIGHT_GREEN,
                            format!(
                                "Created @{} — save this token now:",
                                created.identity.handle
                            ),
                        );
                        let mut tok = created.token.clone();
                        ui.add(egui::TextEdit::singleline(&mut tok).desired_width(f32::INFINITY));

                        if created.identity.kind == IdentityKind::Agent {
                            ui.label("MCP config snippet:");
                            let mut snippet = mcp_config_snippet(&self.server_url, &created.token);
                            ui.add(
                                egui::TextEdit::multiline(&mut snippet)
                                    .code_editor()
                                    .desired_rows(8)
                                    .desired_width(f32::INFINITY),
                            );
                        }
                    }
                });
        }

        if let Some(channel) = &mut self.channel_edit {
            let mut save = false;
            let mut cancel = false;
            let modal = egui::Modal::new(egui::Id::new("edit_channel")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.heading("Edit channel");
                ui.label("Name");
                ui.text_edit_singleline(&mut channel.name);
                ui.label("Description");
                ui.text_edit_singleline(&mut channel.description);
                ui.horizontal(|ui| {
                    save = ui
                        .add_enabled(!channel.name.trim().is_empty(), egui::Button::new("Save"))
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
    }
}

impl eframe::App for RelayPodApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        if !self.state.connected {
            self.render_connect_screen(ctx);
            return;
        }

        // Top bar
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
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
                        self.runtime.send(ClientCommand::Disconnect);
                    }
                    let unread_label = format!("Unread ({})", self.state.unread.len());
                    if ui
                        .selectable_label(self.show_unread_drawer, unread_label)
                        .clicked()
                    {
                        self.show_unread_drawer = !self.show_unread_drawer;
                        if self.show_unread_drawer {
                            self.runtime.send(ClientCommand::RefreshUnread);
                        }
                    }
                });
            });
            if let Some(err) = &self.state.status_error {
                ui.colored_label(Color32::LIGHT_RED, err);
            }
        });

        // Left sidebar: Channels + Identities
        egui::SidePanel::left("sidebar")
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let has_query = !self.channel_search.is_empty();
                    let clear_space = if has_query { 26.0 } else { 0.0 };
                    let input_width = (ui.available_width() - clear_space).max(60.0);
                    let search_resp = ui.add(
                        egui::TextEdit::singleline(&mut self.channel_search)
                            .hint_text("Search channels...")
                            .desired_width(input_width),
                    );
                    if search_resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        self.channel_search.clear();
                    }
                    if has_query
                        && ui
                            .small_button("×")
                            .on_hover_text("Clear channel search")
                            .clicked()
                    {
                        self.channel_search.clear();
                    }
                });
                ui.add_space(6.0);

                let channels = self.state.channels.clone();
                let filtered = filter_channels(&channels, &self.channel_search);
                let is_searching = !self.channel_search.trim().is_empty();
                let channels_open = section_is_expanded(
                    self.channels_section_open,
                    &self.channel_search,
                    !filtered.active.is_empty(),
                );
                let archived_open = section_is_expanded(
                    self.archived_channels_section_open,
                    &self.channel_search,
                    !filtered.archived.is_empty(),
                );

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
                                self.channels_section_open = !self.channels_section_open;
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("+").clicked() {
                                        self.show_new_channel_modal = true;
                                    }
                                },
                            );
                        });

                        if channels_open {
                            for channel in &filtered.active {
                                self.render_channel_row(ui, channel);
                            }
                        }

                        if !filtered.archived.is_empty() {
                            ui.add_space(8.0);
                            if render_collapsible_section_header(
                                ui,
                                "ARCHIVED CHANNELS",
                                archived_open,
                                Some(filtered.archived.len()),
                            ) {
                                self.archived_channels_section_open =
                                    !self.archived_channels_section_open;
                            }
                            if archived_open {
                                for channel in &filtered.archived {
                                    self.render_channel_row(ui, channel);
                                }
                            }
                        }

                        if is_searching
                            && filtered.active.is_empty()
                            && filtered.archived.is_empty()
                        {
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
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("+ Agent").clicked() {
                            self.show_new_agent_modal = true;
                            self.last_created_identity = None;
                        }
                    });
                });

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for id in &self.state.identities {
                        let label = if id.unread_count > 0 {
                            format!("@{} ({})", id.handle, id.unread_count)
                        } else {
                            format!("@{}", id.handle)
                        };
                        ui.horizontal(|ui| {
                            draw_kind_badge(ui, id.kind);
                            if ui.selectable_label(false, label).clicked() {
                                self.channel_composer.push_str(&format!("@{} ", id.handle));
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
                    ui.horizontal(|ui| {
                        ui.strong("Unread Queue");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Close").clicked() {
                                self.show_unread_drawer = false;
                            }
                        });
                    });
                    ui.separator();

                    let items = self.state.unread.clone();
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
            let max_thread_w = (ctx.screen_rect().width() * 0.5).max(340.0);
            egui::SidePanel::right("thread_panel")
                .default_width(400.0)
                .min_width(300.0)
                .max_width(max_thread_w)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Thread");
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Close").clicked() {
                                self.state.active_thread = None;
                                self.prefilled_thread_id = None;
                                self.thread_composer.clear();
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

                            let thread_workers: Vec<String> = self
                                .state
                                .workers_for_thread(&thread.parent.channel_id, &thread.parent.id)
                                .into_iter()
                                .map(|w| w.handle.clone())
                                .collect();
                            if !thread_workers.is_empty() {
                                render_working_banner(ui, &thread_workers);
                                ui.add_space(6.0);
                            }
                        });
                });
        }

        // Center panel: Channel feed + composer
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(ch) = self.state.active_channel().cloned() {
                let can_manage = self.can_manage_channels();
                let archived = ch.archived;
                ui.horizontal(|ui| {
                    ui.heading(format!("# {}", ch.name));
                    if !ch.description.is_empty() {
                        ui.label(
                            egui::RichText::new(&ch.description)
                                .color(ui.visuals().weak_text_color()),
                        );
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
                egui::ScrollArea::vertical()
                    .id_salt("channel_messages_scroll")
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        for msg in msgs {
                            let workers: Vec<String> = self
                                .state
                                .workers_for_thread(&msg.channel_id, &msg.id)
                                .into_iter()
                                .map(|w| w.handle.clone())
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
                            let action = render_message_card(
                                ui,
                                &msg,
                                can_manage && !archived,
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
                                        self.runtime.send(ClientCommand::AckUnread { unread_id });
                                    }
                                }
                                MessageCardAction::Delete => {
                                    self.pending_delete_message = Some(msg.clone());
                                }
                                MessageCardAction::None => {}
                            }
                            ui.add_space(6.0);
                        }
                    });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.label("Select a channel from the sidebar.");
                });
            }
        });

        self.render_modals(ctx);
    }
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
        let d = id.display_name.to_lowercase();
        if q.is_empty() || h.starts_with(&q) {
            prefix_matches.push(id);
        } else if h.contains(&q) || d.contains(&q) {
            contains_matches.push(id);
        }
    }

    prefix_matches.extend(contains_matches);
    prefix_matches
}

#[derive(Debug, Default)]
struct FilteredChannels<'a> {
    active: Vec<&'a Channel>,
    archived: Vec<&'a Channel>,
}

fn channel_matches_query(channel: &Channel, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    channel.name.to_lowercase().contains(&q) || channel.description.to_lowercase().contains(&q)
}

fn filter_channels<'a>(channels: &'a [Channel], query: &str) -> FilteredChannels<'a> {
    let mut filtered = FilteredChannels::default();
    for channel in channels {
        if !channel_matches_query(channel, query) {
            continue;
        }
        if channel.archived {
            filtered.archived.push(channel);
        } else {
            filtered.active.push(channel);
        }
    }
    filtered
}

fn section_is_expanded(user_open: bool, search_query: &str, has_matches: bool) -> bool {
    user_open || (!search_query.trim().is_empty() && has_matches)
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
    } else if has_focus {
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
            send_triggered = true;
        }
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
            ui.horizontal(|ui| {
                let send_btn_width = 72.0;
                let edit_width = (ui.available_width() - send_btn_width - 10.0).max(100.0);

                ui.add(
                    egui::TextEdit::multiline(text)
                        .id(input_id)
                        .frame(false)
                        .hint_text(hint_text)
                        .desired_rows(2)
                        .desired_width(edit_width)
                        .min_size(egui::vec2(edit_width, 44.0)),
                );

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Max), |ui| {
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
                });
            });
        });

    if popup_active {
        if let Some(ref act) = active {
            let anchor = frame_resp.response.rect.left_top() - egui::vec2(0.0, 6.0);
            let popup_id = ui.make_persistent_id(format!("{id_salt}_mention_popup"));
            let popup_width = frame_resp.response.rect.width().min(320.0).max(200.0);

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
                                                    if !id.display_name.is_empty()
                                                        && id.display_name != id.handle
                                                    {
                                                        ui.label(
                                                            egui::RichText::new(&id.display_name)
                                                                .size(12.0)
                                                                .color(
                                                                    ui.visuals().weak_text_color(),
                                                                ),
                                                        );
                                                    }
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

fn draw_working_dots(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(26.0, 14.0), egui::Sense::hover());
    let time = ui.ctx().input(|i| i.time) as f32;
    let base_y = rect.center().y + 1.0;
    let accent = Color32::from_rgb(167, 139, 250);

    for idx in 0..3 {
        let phase = time * 5.0_f32 - (idx as f32) * 0.75_f32;
        let bounce = phase.sin().max(0.0);
        let x = rect.left() + 4.0 + (idx as f32) * 9.0;
        let y = base_y - bounce * 4.0;
        let alpha = (130.0 + bounce * 125.0) as u8;
        let color = Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), alpha);
        ui.painter().circle_filled(egui::pos2(x, y), 2.5, color);
    }
    ui.ctx().request_repaint();
}

fn format_workers_label(handles: &[String]) -> String {
    match handles {
        [] => String::new(),
        [one] => format!("@{one} is working..."),
        [a, b] => format!("@{a} and @{b} are working..."),
        many => format!("@{} and {} others are working...", many[0], many.len() - 1),
    }
}

fn render_working_banner(ui: &mut egui::Ui, handles: &[String]) {
    let dark = ui.visuals().dark_mode;
    let fill = if dark {
        Color32::from_rgb(42, 38, 56)
    } else {
        Color32::from_rgb(243, 238, 255)
    };
    let border = if dark {
        Color32::from_rgb(92, 74, 140)
    } else {
        Color32::from_rgb(196, 181, 253)
    };
    let text_color = if dark {
        Color32::from_rgb(216, 204, 255)
    } else {
        Color32::from_rgb(91, 33, 182)
    };

    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, border))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                draw_working_dots(ui);
                ui.label(
                    egui::RichText::new(format_workers_label(handles))
                        .size(12.0)
                        .color(text_color),
                );
            });
        });
}

enum MessageCardAction {
    None,
    OpenThread,
    AckThread,
    Delete,
}

fn render_message_card(
    ui: &mut egui::Ui,
    msg: &Message,
    can_delete: bool,
    working_handles: &[String],
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
            if !working_handles.is_empty() {
                ui.add_space(6.0);
                render_working_banner(ui, working_handles);
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
            display_name: handle.into(),
            kind,
            owner_id: None,
            unread_count: 0,
            online: false,
            created_at: 0,
        };
        let ids = vec![
            mk("marcin", IdentityKind::Human),
            mk("marcin-codex", IdentityKind::Agent),
            mk("claude", IdentityKind::Agent),
        ];

        assert_eq!(
            last_mentioned_handle_in_text(
                "Hey @marcin-codex and @claude, check with @marcin",
                &ids,
                Some("marcin"),
            )
            .as_deref(),
            Some("claude")
        );
        assert_eq!(
            last_mentioned_handle_in_text("email@marcin-codex.com @unknown", &ids, Some("marcin")),
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
        };
        let channels = vec![
            mk("c1", "general", "Shared workspace", false),
            mk("c2", "relaypod", "Backend and UI work", false),
            mk("c3", "legacy-ops", "Old incident triage", true),
        ];

        let all = filter_channels(&channels, "   ");
        assert_eq!(all.active.len(), 2);
        assert_eq!(all.archived.len(), 1);

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
    fn expands_folded_section_only_when_search_has_matches() {
        assert!(!section_is_expanded(false, "", true));
        assert!(section_is_expanded(true, "", false));
        assert!(section_is_expanded(false, "ops", true));
        assert!(!section_is_expanded(false, "ops", false));
        assert!(!section_is_expanded(false, "   ", true));
    }
}
