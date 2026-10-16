//! Connection sidebar and the new/edit connection modal.

use std::collections::HashMap;
use std::path::PathBuf;

use eframe::egui;

use crate::app::{AppState, ConnectionState, Environment, SavedQuery, saved_groups};
use crate::db::{ConnectionConfig, ConnectionId, MysqlSslMode, PostgresSslMode, parse_connection_url};
use crate::ssh::{SshAuth, SshConfig};

use super::{Palette, Theme};

enum ConnectionStateKind {
    Live,
    Connecting,
    Offline,
}

enum SidebarAction {
    Connect(ConnectionId),
    Disconnect(ConnectionId),
    NewQuery(ConnectionId),
    Edit(ConnectionId),
    Delete(ConnectionId),
    RefreshSchema(ConnectionId),
    PreviewTable(ConnectionId, String, String),
    TableCount(ConnectionId, String, String),
    OpenHistory(ConnectionId, String),
    ClearHistory,
    OpenSaved(u64),
    DeleteSaved(u64),
}

fn history_title(sql: &str) -> String {
    let first = sql.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("SQL");
    let mut title: String = first.chars().take(28).collect();
    if first.chars().count() > 28 {
        title += "…";
    }
    title
}

fn env_badge(ui: &mut egui::Ui, env: Environment) {
    let color = env.badge_color(Theme::from_egui(ui.ctx().theme()));
    egui::Frame::NONE
        .fill(color.gamma_multiply(0.2))
        .stroke(egui::Stroke::new(1.0, color.gamma_multiply(0.6)))
        .corner_radius(egui::CornerRadius::same(3))
        .inner_margin(egui::Margin::symmetric(4, 1))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(env.label()).small().strong().color(color));
        });
}

/// One saved-query row: pin mark, open button, connection name, delete.
fn saved_row(
    ui: &mut egui::Ui,
    palette: &Palette,
    query: &SavedQuery,
    conn_names: &HashMap<ConnectionId, String>,
    actions: &mut Vec<SidebarAction>,
) {
    ui.horizontal(|ui| {
        if query.pinned {
            ui.label(egui::RichText::new("★").small().color(palette.accent));
        }
        if ui.small_button(&query.name).clicked() {
            actions.push(SidebarAction::OpenSaved(query.id));
        }
        let conn_name = conn_names.get(&query.conn).map(String::as_str).unwrap_or("deleted");
        ui.label(egui::RichText::new(conn_name).small().weak());
        if ui.small_button("Delete").clicked() {
            actions.push(SidebarAction::DeleteSaved(query.id));
        }
    });
}

pub fn sidebar(state: &mut AppState, ui: &mut egui::Ui) {
    // root() pumps this module through the sidebar, so the prod confirm
    // renders with the connect gestures that raise it.
    prod_confirm_window(state, ui);
    let palette = Palette::of(ui.ctx().theme());
    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
        if ui
            .add_sized(
                [ui.available_width(), 28.0],
                egui::Button::new(egui::RichText::new("+ New connection").strong()).fill(palette.accent),
            )
            .clicked()
        {
            state.open_draft(None);
        }
        ui.separator();
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), |ui| {
                ui.label(egui::RichText::new("CONNECTIONS").small().weak());
                ui.add_space(4.0);

                if state.connections().is_empty() {
                    ui.label("No connections yet.");
                    ui.small("Add one below to start querying.");
                    ui.add_space(8.0);
                }

                let active_conn = state
                    .active()
                    .and_then(|id| state.tab(id))
                    .and_then(|tab| tab.conn);

                let mut actions: Vec<SidebarAction> = Vec::new();
                let snapshot: Vec<(ConnectionId, String, Environment, String, bool)> = state
                    .connections()
                    .iter()
                    .map(|c| {
                        let (dot, text) = match c.state() {
                            ConnectionState::Disconnected => ("○", "disconnected".to_owned()),
                            ConnectionState::Connecting { .. } => {
                                ("◌", "connecting".to_owned())
                            }
                            ConnectionState::Connected { session } => {
                                ("●", session.server_version.clone())
                            }
                            ConnectionState::QueryRunning { session, .. } => {
                                ("●", session.server_version.clone())
                            }
                            ConnectionState::Error { message } => ("●", message.clone()),
                        };
                        let live = matches!(
                            c.state(),
                            ConnectionState::Connected { .. }
                                | ConnectionState::QueryRunning { .. }
                        );
                        (c.id, c.name.clone(), c.env, format!("{dot} {text}"), live)
                    })
                    .collect();
                let dot_colors: std::collections::HashMap<ConnectionId, egui::Color32> = state
                    .connections()
                    .iter()
                    .map(|c| {
                        let color = match c.state() {
                            ConnectionState::Disconnected => egui::Color32::GRAY,
                            ConnectionState::Connecting { .. } => egui::Color32::YELLOW,
                            ConnectionState::Connected { .. }
                            | ConnectionState::QueryRunning { .. } => palette.success,
                            ConnectionState::Error { .. } => egui::Color32::RED,
                        };
                        (c.id, color)
                    })
                    .collect();
                for (conn_id, conn_name, env, state_label, _is_live) in snapshot {
                    let selected = Some(conn_id) == active_conn;
                    let dot_color =
                        dot_colors.get(&conn_id).copied().unwrap_or(egui::Color32::GRAY);
                    let (dot, state_text) = match state_label.split_once(' ') {
                        Some((d, t)) => (d.to_owned(), t.to_owned()),
                        None => ("○".to_owned(), state_label.clone()),
                    };
                    let frame_fill = if selected {
                        ui.visuals().selection.bg_fill.gamma_multiply(0.35)
                    } else {
                        ui.visuals().faint_bg_color
                    };
                    let frame_stroke = if selected {
                        egui::Stroke::new(1.0, palette.accent)
                    } else {
                        ui.visuals().widgets.noninteractive.bg_stroke
                    };
                    let kind: ConnectionStateKind = {
                        let conn = state
                            .connections()
                            .iter()
                            .find(|c| c.id == conn_id)
                            .map(|c| match c.state() {
                                ConnectionState::Connected { .. } => ConnectionStateKind::Live,
                                ConnectionState::QueryRunning { .. } => ConnectionStateKind::Live,
                                ConnectionState::Connecting { .. } => {
                                    ConnectionStateKind::Connecting
                                }
                                ConnectionState::Disconnected => ConnectionStateKind::Offline,
                                ConnectionState::Error { .. } => ConnectionStateKind::Offline,
                            })
                            .unwrap_or(ConnectionStateKind::Offline);
                        conn
                    };
                    let transaction_open = state
                        .connections()
                        .iter()
                        .find(|c| c.id == conn_id)
                        .is_some_and(|c| c.transaction().is_open());
                    egui::Frame::group(ui.style())
                        .fill(frame_fill)
                        .stroke(frame_stroke)
                        .corner_radius(egui::CornerRadius::same(6))
                        .inner_margin(egui::Margin::same(8))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width() - 16.0);
                            ui.horizontal(|ui| {
                                ui.colored_label(dot_color, &dot);
                                ui.vertical(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new(&conn_name).strong());
                                        env_badge(ui, env);
                                    });
                                    ui.label(
                                        egui::RichText::new(&state_text).small().weak(),
                                    );
                                    if transaction_open {
                                        ui.label(
                                            egui::RichText::new("transaction open")
                                                .small()
                                                .color(palette.success),
                                        );
                                    }
                                });
                            });
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                match kind {
                                    ConnectionStateKind::Live => {
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new("New query")
                                                        .small()
                                                        .strong(),
                                                )
                                                .fill(palette.accent)
                                                .small(),
                                            )
                                            .clicked()
                                        {
                                            actions.push(SidebarAction::NewQuery(conn_id));
                                        }
                                        if ui.small_button("Disconnect").clicked() {
                                            actions.push(SidebarAction::Disconnect(conn_id));
                                        }
                                    }
                                    ConnectionStateKind::Connecting => {
                                        if ui.small_button("Stop").clicked() {
                                            actions.push(SidebarAction::Disconnect(conn_id));
                                        }
                                    }
                                    ConnectionStateKind::Offline => {
                                        if ui.small_button("Connect").clicked() {
                                            actions.push(SidebarAction::Connect(conn_id));
                                        }
                                    }
                                }
                                if ui.small_button("Edit").clicked() {
                                    actions.push(SidebarAction::Edit(conn_id));
                                }
                                if ui.small_button("Delete").clicked() {
                                    actions.push(SidebarAction::Delete(conn_id));
                                }
                            });
                            if matches!(kind, ConnectionStateKind::Live) {
                                ui.add_space(4.0);
                                ui.separator();
                                ui.add_space(4.0);
                                let schema_actions = super::schema::connection_schema(
                                    state, ui, conn_id,
                                );
                                for sa in schema_actions {
                                    match sa {
                                        super::schema::SchemaAction::Refresh(id) => {
                                            actions.push(SidebarAction::RefreshSchema(id));
                                        }
                                        super::schema::SchemaAction::Preview(id, schema, table) => {
                                            actions.push(SidebarAction::PreviewTable(
                                                id, schema, table,
                                            ));
                                        }
                                        super::schema::SchemaAction::Count(id, schema, table) => {
                                            actions.push(SidebarAction::TableCount(
                                                id, schema, table,
                                            ));
                                        }
                                    }
                                }
                            }
                        });
                    ui.add_space(4.0);
                }

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("HISTORY").small().weak());
                    if ui.small_button("Clear").clicked() {
                        actions.push(SidebarAction::ClearHistory);
                    }
                });
                ui.add_space(4.0);
                if state.history().is_empty() {
                    ui.label(egui::RichText::new("No runs yet.").small().weak());
                } else {
                    for entry in state.history().iter().take(crate::app::HISTORY_SHOWN) {
                        let dot = if entry.succeeded { palette.success } else { egui::Color32::RED };
                        ui.horizontal(|ui| {
                            ui.colored_label(dot, "●");
                            if ui.small_button(history_title(&entry.sql)).clicked() {
                                actions.push(SidebarAction::OpenHistory(
                                    entry.conn,
                                    entry.sql.clone(),
                                ));
                            }
                        });
                    }
                }
                ui.add_space(8.0);

                ui.label(egui::RichText::new("SAVED").small().weak());
                ui.add_space(4.0);
                if state.saved_queries().is_empty() {
                    ui.label(egui::RichText::new("No saved queries.").small().weak());
                } else {
                    let conn_names: HashMap<ConnectionId, String> = state
                        .connections()
                        .iter()
                        .map(|c| (c.id, c.name.clone()))
                        .collect();
                    for group in saved_groups(state.saved_queries()) {
                        let Some(folder) = group.folder else {
                            for query in &group.entries {
                                saved_row(ui, &palette, query, &conn_names, &mut actions);
                            }
                            continue;
                        };
                        let open_id = egui::Id::new(("crusty_saved_folder", folder));
                        let mut open: bool =
                            ui.ctx().data_mut(|d| d.get_temp(open_id)).unwrap_or(true);
                        ui.horizontal(|ui| {
                            let arrow = if open { "▾" } else { "▸" };
                            if ui.small_button(format!("{arrow} {folder}")).clicked() {
                                open = !open;
                            }
                        });
                        ui.ctx().data_mut(|d| d.insert_temp(open_id, open));
                        if !open {
                            continue;
                        }
                        ui.indent(egui::Id::new(("crusty_saved_entries", folder)), |ui| {
                            for query in &group.entries {
                                saved_row(ui, &palette, query, &conn_names, &mut actions);
                            }
                        });
                    }
                }
                ui.add_space(8.0);

                for action in actions {
                    match action {
                        SidebarAction::Connect(id) => {
                            let needs_prompt = state
                                .connections()
                                .iter()
                                .find(|c| c.id == id)
                                .is_some_and(|c| {
                                    c.config.prompts_for_password() && c.password.is_empty()
                                });
                            if needs_prompt {
                                state.open_password_prompt(id);
                            } else {
                                state.connect(id);
                            }
                        }
                        SidebarAction::Disconnect(id) => state.disconnect(id),
                        SidebarAction::NewQuery(id) => {
                            let tab = state.new_tab(id);
                            state.activate(tab);
                        }
                        SidebarAction::Edit(id) => state.open_draft(Some(id)),
                        SidebarAction::Delete(id) => state.delete_connection(id),
                        SidebarAction::RefreshSchema(id) => state.refresh_schema(id),
                        SidebarAction::PreviewTable(id, schema, table) => {
                            state.preview_table(id, &schema, &table)
                        }
                        SidebarAction::TableCount(id, schema, table) => {
                            state.request_row_count(id, &schema, &table)
                        }
                        SidebarAction::OpenHistory(conn, sql) => state.open_history(conn, &sql),
                        SidebarAction::ClearHistory => state.clear_history(),
                        SidebarAction::OpenSaved(id) => state.open_saved(id),
                        SidebarAction::DeleteSaved(id) => {
                            state.delete_saved(id);
                        }
                    }
                }
            });
        });
    });
}

pub fn password_window(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(id) = state.password_prompt() else { return };
    let Some(name) =
        state.connections().iter().find(|c| c.id == id).map(|c| c.name.clone())
    else {
        state.cancel_password_prompt();
        return;
    };
    let mut submit = false;
    let mut cancel = false;
    egui::Window::new(format!("Password for {name}")).show(ui.ctx(), |ui| {
        ui.label(format!("Enter password for {name}"));
        ui.add(egui::TextEdit::singleline(state.prompt_password_mut()).password(true));
        ui.horizontal(|ui| {
            if ui.button("Connect").clicked() {
                submit = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    if submit {
        state.submit_password();
    } else if cancel {
        state.cancel_password_prompt();
    }
}

pub fn prod_confirm_window(state: &mut AppState, ui: &mut egui::Ui) {
    let Some(id) = state.pending_prod() else { return };
    let Some(name) =
        state.connections().iter().find(|c| c.id == id).map(|c| c.name.clone())
    else {
        state.cancel_prod();
        return;
    };
    let mut confirm = false;
    let mut cancel = false;
    egui::Window::new("Connect to prod?").show(ui.ctx(), |ui| {
        ui.label(format!("Connect to {name}?"));
        ui.horizontal(|ui| {
            if ui.button("Connect").clicked() {
                confirm = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });
    });
    if confirm {
        state.confirm_prod();
    } else if cancel {
        state.cancel_prod();
    }
}

pub fn draft_window(state: &mut AppState, ui: &mut egui::Ui) {
    if state.draft().is_none() {
        return;
    }
    let title = if state.draft().is_some_and(|d| d.editing.is_some()) {
        "Edit connection"
    } else {
        "New connection"
    };
    let mut save = false;
    let mut close = false;
    egui::Window::new(title).show(ui.ctx(), |ui| {
        let registry = state.registry();
        let Some(draft) = state.draft_mut() else { return };
        let draft_scope = draft.editing.map(|id| id.raw());
        let url_id = egui::Id::new(("connection_url_paste", draft_scope));
        let note_id = egui::Id::new(("connection_url_note", draft_scope));
        let mut url_text: String =
            ui.ctx().memory(|m| m.data.get_temp(url_id)).unwrap_or_default();
        let mut parse_url = false;
        ui.horizontal(|ui| {
            ui.label("URL:");
            ui.add(
                egui::TextEdit::singleline(&mut url_text)
                    .hint_text("postgres://user:pass@host:5432/db"),
            );
            parse_url = ui.button("Parse").clicked();
        });
        ui.ctx().memory_mut(|m| m.data.insert_temp(url_id, url_text.clone()));
        if parse_url {
            match parse_connection_url(&url_text) {
                Ok(parsed) => {
                    let kind = parsed.kind;
                    draft.config = parsed.config;
                    if let Some(password) = parsed.password {
                        draft.password = password;
                    }
                    ui.ctx().memory_mut(|m| {
                        m.data.insert_temp(
                            note_id,
                            Some((
                                format!("Filled {} fields from URL.", kind.label()),
                                false,
                            )),
                        )
                    });
                }
                Err(error) => {
                    ui.ctx().memory_mut(|m| {
                        m.data.insert_temp(note_id, Some((error.to_string(), true)))
                    });
                }
            }
        }
        if let Some((note, is_error)) = ui
            .ctx()
            .memory(|m| m.data.get_temp::<Option<(String, bool)>>(note_id))
            .flatten()
        {
            if is_error {
                ui.colored_label(ui.visuals().error_fg_color, note);
            } else {
                ui.label(egui::RichText::new(note).small().weak());
            }
        }
        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.text_edit_singleline(&mut draft.name);
        });
        ui.horizontal(|ui| {
            ui.label("Kind:");
            let mut selected = draft.config.kind();
            for kind in registry.all() {
                if ui.selectable_value(&mut selected, kind, kind.label()).clicked() {
                    draft.config = kind.default_config();
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Environment:");
            for env in Environment::ALL {
                ui.selectable_value(&mut draft.env, env, env.label());
            }
        });
        let mut read_only = draft.config.is_read_only();
        if ui
            .checkbox(&mut read_only, "Read only")
            .on_hover_text("Refuse statements that write when set")
            .changed()
        {
            draft.config.set_read_only(read_only);
        }
        match &mut draft.config {
            ConnectionConfig::Sqlite { path, .. } => {
                ui.horizontal(|ui| {
                    ui.label("Path:");
                    let mut text = path.to_string_lossy().into_owned();
                    if ui.text_edit_singleline(&mut text).changed() {
                        *path = text.into();
                    }
                });
                ui.small("Use :memory: for a throwaway database.");
            }
            ConnectionConfig::Postgres { host, port, database, user, ssl_mode, ssh, .. } => {
                ui.horizontal(|ui| {
                    ui.label("Host:");
                    ui.text_edit_singleline(host);
                });
                ui.horizontal(|ui| {
                    ui.label("Port:");
                    let mut text = port.to_string();
                    if ui.text_edit_singleline(&mut text).changed() {
                        if let Ok(p) = text.parse() {
                            *port = p;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Database:");
                    ui.text_edit_singleline(database);
                });
                ui.horizontal(|ui| {
                    ui.label("User:");
                    ui.text_edit_singleline(user);
                });
                ui.horizontal(|ui| {
                    ui.label("SSL:");
                    egui::ComboBox::from_label("")
                        .selected_text(ssl_mode.label())
                        .show_ui(ui, |ui| {
                            for mode in PostgresSslMode::ALL {
                                ui.selectable_value(ssl_mode, mode, mode.label());
                            }
                        });
                });
                ssh_section(ui, ssh, &mut draft.ssh_password);
            }
            ConnectionConfig::Mysql { host, port, database, user, ssl_mode, ssh, .. } => {
                ui.horizontal(|ui| {
                    ui.label("Host:");
                    ui.text_edit_singleline(host);
                });
                ui.horizontal(|ui| {
                    ui.label("Port:");
                    let mut text = port.to_string();
                    if ui.text_edit_singleline(&mut text).changed() {
                        if let Ok(p) = text.parse() {
                            *port = p;
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Database:");
                    ui.text_edit_singleline(database);
                });
                ui.horizontal(|ui| {
                    ui.label("User:");
                    ui.text_edit_singleline(user);
                });
                ui.horizontal(|ui| {
                    ui.label("SSL:");
                    egui::ComboBox::from_label("")
                        .selected_text(ssl_mode.label())
                        .show_ui(ui, |ui| {
                            for mode in MysqlSslMode::ALL {
                                ui.selectable_value(ssl_mode, mode, mode.label());
                            }
                        });
                });
                ssh_section(ui, ssh, &mut draft.ssh_password);
            }
        }
        if draft.config.prompts_for_password() {
            ui.horizontal(|ui| {
                ui.label("Password:");
                ui.add(egui::TextEdit::singleline(&mut draft.password).password(true));
            });
        }
        if let Some(error) = state.draft().and_then(|d| d.error.clone()) {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        ui.horizontal(|ui| {
            if ui.button("Save and connect").clicked() {
                save = true;
            }
            if ui.button("Cancel").clicked() {
                close = true;
            }
        });
    });
    if save {
        let is_new = state.draft().is_some_and(|d| d.editing.is_none());
        state.save_draft();
        if state.draft().is_none() && is_new {
            if let Some(id) = state.connections().last().map(|c| c.id) {
                state.connect(id);
            }
        }
    } else if close {
        state.cancel_draft();
    }
}

/// The SSH tunnel fields, rendered for Postgres and MySQL drafts only. The
/// password box binds the draft's transient ssh secret, never the config.
fn ssh_section(ui: &mut egui::Ui, ssh: &mut Option<SshConfig>, secret: &mut String) {
    let mut enabled = ssh.is_some();
    if ui.checkbox(&mut enabled, "Use SSH tunnel").changed() {
        *ssh = enabled.then(SshConfig::default);
    }
    let Some(ssh) = ssh.as_mut() else { return };
    ui.horizontal(|ui| {
        ui.label("SSH host:");
        ui.text_edit_singleline(&mut ssh.host);
    });
    ui.horizontal(|ui| {
        ui.label("SSH port:");
        let mut text = ssh.port.to_string();
        if ui.text_edit_singleline(&mut text).changed() {
            if let Ok(port) = text.parse() {
                ssh.port = port;
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("SSH user:");
        ui.text_edit_singleline(&mut ssh.user);
    });
    ui.horizontal(|ui| {
        ui.label("SSH auth:");
        let password_auth = matches!(ssh.auth, SshAuth::Password);
        if ui.selectable_label(password_auth, "Password").clicked() && !password_auth {
            ssh.auth = SshAuth::Password;
        }
        if ui.selectable_label(!password_auth, "Key file").clicked() && password_auth {
            ssh.auth = SshAuth::KeyFile { path: PathBuf::new(), passphrase: None };
        }
    });
    match &mut ssh.auth {
        SshAuth::Password => {
            ui.horizontal(|ui| {
                ui.label("SSH password:");
                ui.add(egui::TextEdit::singleline(secret).password(true));
            });
        }
        SshAuth::KeyFile { path, passphrase } => {
            ui.horizontal(|ui| {
                ui.label("Key file:");
                let mut text = path.to_string_lossy().into_owned();
                if ui.text_edit_singleline(&mut text).changed() {
                    *path = text.into();
                }
            });
            ui.horizontal(|ui| {
                ui.label("Passphrase:");
                let mut text = passphrase.clone().unwrap_or_default();
                if ui.add(egui::TextEdit::singleline(&mut text).password(true)).changed() {
                    *passphrase = (!text.is_empty()).then_some(text);
                }
            });
        }
    }
}
