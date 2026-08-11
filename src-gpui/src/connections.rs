use std::path::PathBuf;
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::form::{field, v_form};
use gpui_component::input::{Input, InputState};
use gpui_component::notification::Notification;
use gpui_component::radio::{Radio, RadioGroup};
use gpui_component::select::{Select, SelectState};
use gpui_component::switch::Switch;
use gpui_component::{
  ActiveTheme, Disableable, IndexPath, Sizable, StyledExt, WindowExt, h_flex, v_flex,
};
use soquel_core::AppState;
use soquel_core::error::{Error, SecretSubject};
use soquel_core::profiles::{
  AgentAccess, ConnectionInput, ConnectionProfile, ConnectorParams, CredentialSource, Env,
  SqlServerParams, SslMode,
};
use soquel_core::transfer::{DuplicateStrategy, ImportPreview};

use crate::command_approval::{self, CommandApprovalPrompt};
use crate::core::{self, Db};
use crate::host_key::{self, HostKeyPrompt};
use crate::icons::SoquelIcon;
use crate::transfer::{self, EntryKind};
use crate::tunnels::{
  CREDENTIAL_MODES, CredentialMode, TunnelsView, command_preview, credential_mode_hint,
  credential_mode_label,
};

pub enum ConnectionsEvent {
  Connected { db: Db, profile: ConnectionProfile },
}

const ENVS: [Env; 3] = [Env::Dev, Env::Staging, Env::Prod];
const SSL_MODES: [SslMode; 4] = [
  SslMode::Disable,
  SslMode::Prefer,
  SslMode::Require,
  SslMode::VerifyFull,
];

fn env_label(env: Env) -> &'static str {
  match env {
    Env::Dev => "dev",
    Env::Staging => "staging",
    Env::Prod => "prod",
  }
}

fn ssl_label(mode: SslMode) -> &'static str {
  match mode {
    SslMode::Disable => "disable",
    SslMode::Prefer => "prefer",
    SslMode::Require => "require",
    SslMode::VerifyFull => "verify-full",
  }
}

const CONNECTION_COMMAND_HINT: &str =
  "No shell: {host} {port} {user} {database} are substituted, pipes and $(...) are not supported.";

fn dsn(params: &ConnectorParams) -> String {
  match params {
    ConnectorParams::Postgres(p) | ConnectorParams::Mysql(p) => format!(
      "{}://{}@{}:{}/{}",
      match params.kind() {
        soquel_core::profiles::ConnectorKind::Mysql => "mysql",
        _ => "postgres",
      },
      p.user,
      p.host,
      p.port,
      p.database
    ),
    ConnectorParams::Sqlite { path } => format!("sqlite://{path}"),
    ConnectorParams::Redis(p) => format!("redis://{}:{}/{}", p.host, p.port, p.db),
    ConnectorParams::Mongo(p) => format!("mongodb://{}:{}", p.host, p.port),
  }
}

/// Ungrouped first, then groups alphabetically; profiles keep their order.
pub fn group_connections(
  profiles: &[ConnectionProfile],
) -> Vec<(Option<String>, Vec<ConnectionProfile>)> {
  let mut sections: Vec<(Option<String>, Vec<ConnectionProfile>)> = Vec::new();
  for profile in profiles {
    let key = profile.group.clone();
    match sections.iter_mut().find(|(group, _)| *group == key) {
      Some((_, list)) => list.push(profile.clone()),
      None => sections.push((key, vec![profile.clone()])),
    }
  }
  // Option orders None before Some, which is exactly the webview's contract.
  sections.sort_by(|a, b| a.0.cmp(&b.0));
  sections
}

pub struct ConnectionsView {
  state: Arc<AppState>,
  profiles: Vec<ConnectionProfile>,
  connecting: Option<String>,
  status: SharedString,
  editing: Option<String>,
  form_name: Entity<InputState>,
  form_group: Entity<InputState>,
  form_host: Entity<InputState>,
  form_port: Entity<InputState>,
  form_database: Entity<InputState>,
  form_user: Entity<InputState>,
  form_password: Entity<InputState>,
  form_command: Entity<InputState>,
  form_env: Entity<SelectState<Vec<String>>>,
  form_ssl: Entity<SelectState<Vec<String>>>,
  form_credential: Entity<SelectState<Vec<String>>>,
  form_tunnel: Entity<SelectState<Vec<String>>>,
  /// Index-coupled with the picker's items: names collide, ids do not.
  form_tunnel_ids: Vec<Option<String>>,
  tunnels_section: Entity<TunnelsView>,
  prompt_password: Entity<InputState>,
  prompt_remember: bool,
  export_include_secrets: bool,
  export_passphrase: Entity<InputState>,
  export_confirm: Entity<InputState>,
  export_busy: bool,
  export_error: Option<SharedString>,
  import_path: Option<PathBuf>,
  import_preview: Option<ImportPreview>,
  import_passphrase: Entity<InputState>,
  import_with_secrets: bool,
  import_strategy: usize,
  /// Sticky: a rejected passphrase keeps the field on screen to retry in.
  import_locked: bool,
  import_busy: bool,
  import_error: Option<SharedString>,
  _task: Task<()>,
}

impl EventEmitter<ConnectionsEvent> for ConnectionsView {}

impl ConnectionsView {
  pub fn new(state: Arc<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
    let profiles = core::list_connections(&state);
    let mut text = |cx: &mut Context<Self>, placeholder: &str| -> Entity<InputState> {
      let placeholder = placeholder.to_string();
      cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
    };
    let form_name = text(cx, "name");
    let form_group = text(cx, "group (optional)");
    let form_host = text(cx, "localhost");
    let form_port = text(cx, "5432");
    let form_database = text(cx, "database");
    let form_user = text(cx, "user");
    let form_command = text(cx, "");
    let form_password = cx.new(|cx| {
      InputState::new(window, cx)
        .placeholder("password")
        .masked(true)
    });
    let prompt_password = cx.new(|cx| {
      InputState::new(window, cx)
        .placeholder("password")
        .masked(true)
    });
    let export_passphrase = cx.new(|cx| InputState::new(window, cx).masked(true));
    let export_confirm = cx.new(|cx| InputState::new(window, cx).masked(true));
    let import_passphrase = cx.new(|cx| {
      InputState::new(window, cx)
        .placeholder("Passphrase")
        .masked(true)
    });
    let form_env = cx.new(|cx| {
      SelectState::new(
        ENVS
          .iter()
          .map(|e| env_label(*e).to_string())
          .collect::<Vec<_>>(),
        Some(IndexPath::default()),
        window,
        cx,
      )
    });
    let form_ssl = cx.new(|cx| {
      SelectState::new(
        SSL_MODES
          .iter()
          .map(|m| ssl_label(*m).to_string())
          .collect::<Vec<_>>(),
        Some(IndexPath::new(1)),
        window,
        cx,
      )
    });
    let form_credential = cx.new(|cx| {
      SelectState::new(
        CREDENTIAL_MODES
          .iter()
          .map(|m| credential_mode_label(*m).to_string())
          .collect::<Vec<_>>(),
        Some(IndexPath::default()),
        window,
        cx,
      )
    });
    let form_tunnel = cx.new(|cx| {
      SelectState::new(
        vec!["none".to_string()],
        Some(IndexPath::default()),
        window,
        cx,
      )
    });
    let tunnels_section = cx.new(|cx| TunnelsView::new(state.clone(), window, cx));

    Self {
      state,
      profiles,
      connecting: None,
      status: SharedString::default(),
      editing: None,
      form_name,
      form_group,
      form_host,
      form_port,
      form_database,
      form_user,
      form_password,
      form_command,
      form_env,
      form_ssl,
      form_credential,
      form_tunnel,
      form_tunnel_ids: vec![None],
      tunnels_section,
      prompt_password,
      prompt_remember: false,
      export_include_secrets: false,
      export_passphrase,
      export_confirm,
      export_busy: false,
      export_error: None,
      import_path: None,
      import_preview: None,
      import_passphrase,
      import_with_secrets: false,
      import_strategy: 0,
      import_locked: false,
      import_busy: false,
      import_error: None,
      _task: Task::ready(()),
    }
  }

  fn refresh(&mut self, cx: &mut Context<Self>) {
    self.profiles = core::list_connections(&self.state);
    cx.notify();
  }

  pub(crate) fn connect(&mut self, id: String, cx: &mut Context<Self>) {
    if self.connecting.is_some() {
      return;
    }
    self.connecting = Some(id.clone());
    self.status = SharedString::default();
    cx.notify();
    let rx = core::connect_id(self.state.clone(), id.clone());
    self._task = cx.spawn(async move |this, cx| {
      let result = rx.await;
      let _ = this.update(cx, |this, cx| {
        this.connecting = None;
        match result {
          Ok(Ok(db)) => {
            if let Ok(profile) = this.state.profiles.lock().unwrap().get(&id) {
              cx.emit(ConnectionsEvent::Connected { db, profile });
            }
          }
          Ok(Err(Error::SecretRequired {
            subject,
            target_id,
            target_name,
            ..
          })) => {
            this.open_secret_prompt(subject, target_id, target_name, id.clone(), cx);
          }
          Ok(Err(Error::HostKeyUntrusted {
            host,
            port,
            fingerprint,
            key,
            previously_trusted,
            ..
          })) => {
            host_key::open_host_key_dialog(
              cx.entity(),
              this.state.clone(),
              HostKeyPrompt {
                host,
                port,
                fingerprint,
                key,
                previously_trusted,
              },
              cx,
              move |view: &mut Self, result, cx| match result {
                Ok(()) => view.connect(id.clone(), cx),
                Err(error) => {
                  view.status = format!("error: {error}").into();
                  cx.notify();
                }
              },
            );
          }
          // The subject can be the connection's own command or its tunnel's;
          // either way the retry is the same connect.
          Ok(Err(Error::CommandApprovalRequired {
            subject,
            target_id,
            target_name,
            program,
            args,
            ..
          })) => {
            command_approval::open_command_approval_dialog(
              cx.entity(),
              this.state.clone(),
              CommandApprovalPrompt {
                subject,
                target_id,
                target_name,
                program,
                args,
              },
              cx,
              move |view: &mut Self, result, cx| match result {
                Ok(()) => view.connect(id.clone(), cx),
                Err(error) => {
                  view.status = format!("error: {error}").into();
                  cx.notify();
                }
              },
            );
          }
          Ok(Err(error)) => {
            this.status = format!("error: {error}").into();
          }
          Err(_) => {}
        }
        cx.notify();
      });
    });
  }

  /// The prompt hands the password to the core and retries the same connect.
  fn open_secret_prompt(
    &mut self,
    subject: SecretSubject,
    target_id: String,
    target_name: String,
    connect_id: String,
    cx: &mut Context<Self>,
  ) {
    self.prompt_remember = false;
    let this = cx.entity();
    let input = self.prompt_password.clone();
    cx.defer(move |cx| {
      let Some(window_handle) = cx.active_window() else {
        return;
      };
      let _ = cx.update_window(window_handle, |_, window, cx| {
        input.update(cx, |input, cx| {
          input.set_value("", window, cx);
          input.focus(window, cx);
        });
        let input = input.clone();
        let this = this.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
          let this = this.clone();
          let (subject, target_id, target_name, connect_id) = (
            subject,
            target_id.clone(),
            target_name.clone(),
            connect_id.clone(),
          );
          let remember = this.read(cx).prompt_remember;
          // Shared by the Connect button and Enter (the dialog's ConfirmDialog).
          let submit = {
            let this = this.clone();
            let input = input.clone();
            move |_: &mut Window, cx: &mut App| {
              let secret = input.read(cx).value().to_string();
              let (target_id, connect_id) = (target_id.clone(), connect_id.clone());
              this.update(cx, |this, cx| {
                core::unlock_secret(
                  &this.state,
                  subject,
                  target_id,
                  secret,
                  this.prompt_remember,
                );
                this.connect(connect_id, cx);
              });
            }
          };
          dialog
            .title(match subject {
              SecretSubject::Tunnel => format!("Credential for the tunnel {target_name}"),
              _ => format!("Password for {target_name}"),
            })
            .on_ok({
              let submit = submit.clone();
              move |_, window, cx| {
                submit(window, cx);
                true
              }
            })
            .child(
              v_flex().gap_3().child(Input::new(&input)).child(
                Checkbox::new("remember")
                  .label("Keep for this session")
                  .checked(remember)
                  .on_click({
                    let this = this.clone();
                    move |checked, _, cx| {
                      let checked = *checked;
                      this.update(cx, |this, cx| {
                        this.prompt_remember = checked;
                        cx.notify();
                      });
                    }
                  }),
              ),
            )
            .footer(
              h_flex()
                .gap_2()
                .justify_end()
                .child(
                  Button::new("prompt-cancel")
                    .label("Cancel")
                    .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                  Button::new("prompt-connect")
                    .primary()
                    .label("Connect")
                    .debug_selector(|| "prompt-connect".into())
                    .on_click(move |_, window, cx| {
                      window.close_dialog(cx);
                      submit(window, cx);
                    }),
                ),
            )
        });
      });
    });
  }

  pub(crate) fn open_form(&mut self, editing: Option<ConnectionProfile>, cx: &mut Context<Self>) {
    self.editing = editing.as_ref().map(|p| p.id.clone());
    self.status = SharedString::default();
    let this = cx.entity();
    cx.defer(move |cx| {
      let Some(window_handle) = cx.active_window() else {
        return;
      };
      let _ =
        cx.update_window(window_handle, |_, window, cx| {
          this.update(cx, |view, cx| {
            view.prefill_form(editing.as_ref(), window, cx);
          });

          let this = this.clone();
          window.open_dialog(cx, move |dialog, _, cx| {
            let view = this.read(cx);
            let title = if view.editing.is_some() {
              "Edit connection"
            } else {
              "New connection"
            };
            let status = view.status.clone();
            let mode = view.selected_mode(cx);
            let command = view.form_command.read(cx).value().trim().to_string();
            let this_test = this.clone();
            let this_save = this.clone();
            dialog
              .title(title)
              .w(px(520.))
              .child(
                v_form()
                  .label_width(px(90.))
                  .child(field().label("Name").child(Input::new(&view.form_name)))
                  .child(field().label("Group").child(Input::new(&view.form_group)))
                  .child(field().label("Env").child(Select::new(&view.form_env)))
                  .child(field().label("Host").child(Input::new(&view.form_host)))
                  .child(field().label("Port").child(Input::new(&view.form_port)))
                  .child(
                    field()
                      .label("Database")
                      .child(Input::new(&view.form_database)),
                  )
                  .child(field().label("User").child(Input::new(&view.form_user)))
                  .child(field().label("SSL").child(Select::new(&view.form_ssl)))
                  .child(
                    field()
                      .label("SSH tunnel")
                      .child(Select::new(&view.form_tunnel)),
                  )
                  .child(
                    field()
                      .label("Password")
                      .child(Select::new(&view.form_credential)),
                  )
                  .when(mode != CredentialMode::Command, |form| {
                    form
                      .child(
                        field()
                          .label(if mode == CredentialMode::Prompt {
                            "(for Test only)"
                          } else {
                            ""
                          })
                          .child(Input::new(&view.form_password)),
                      )
                      .when_some(credential_mode_hint(mode), |form, hint| {
                        form.child(
                          field().label("").child(
                            div()
                              .text_xs()
                              .text_color(cx.theme().muted_foreground)
                              .child(hint),
                          ),
                        )
                      })
                  })
                  .when(mode == CredentialMode::Command, |form| {
                    form
                      .child(
                        field()
                          .label("Command")
                          .child(Input::new(&view.form_command)),
                      )
                      .child(field().label("").child(command_preview(
                        &command,
                        CONNECTION_COMMAND_HINT,
                        cx,
                      )))
                  })
                  .when(!status.is_empty(), |this| {
                    this.child(
                      field().label("").child(
                        div()
                          .text_sm()
                          .text_color(cx.theme().muted_foreground)
                          .child(status),
                      ),
                    )
                  }),
              )
              .footer(
                h_flex()
                  .gap_2()
                  .justify_end()
                  .child(Button::new("form-test").ghost().label("Test").on_click(
                    move |_, _, cx| {
                      this_test.update(cx, |this, cx| this.test_form(cx));
                    },
                  ))
                  .child(
                    Button::new("form-cancel")
                      .label("Cancel")
                      .on_click(|_, window, cx| window.close_dialog(cx)),
                  )
                  .child(Button::new("form-save").primary().label("Save").on_click(
                    move |_, window, cx| {
                      this_save.update(cx, |this, cx| this.save_form(cx));
                      window.close_dialog(cx);
                    },
                  )),
              )
          });
        });
    });
  }

  fn prefill_form(
    &mut self,
    editing: Option<&ConnectionProfile>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let (name, group, host, port, database, user, env_ix, ssl_ix, cred_ix, command, tunnel_id) =
      match editing {
        Some(profile) => {
          let (host, port, database, user, ssl, tunnel_id) = match &profile.params {
            ConnectorParams::Postgres(p) => (
              p.host.clone(),
              p.port.to_string(),
              p.database.clone(),
              p.user.clone(),
              p.ssl_mode,
              p.tunnel_id.clone(),
            ),
            _ => (
              String::new(),
              String::new(),
              String::new(),
              String::new(),
              SslMode::Prefer,
              None,
            ),
          };
          let (mode, command) = match &profile.credential {
            CredentialSource::Keychain => (CredentialMode::Keychain, String::new()),
            CredentialSource::Prompt => (CredentialMode::Prompt, String::new()),
            CredentialSource::Command { command, .. } => (CredentialMode::Command, command.clone()),
          };
          (
            profile.name.clone(),
            profile.group.clone().unwrap_or_default(),
            host,
            port,
            database,
            user,
            ENVS.iter().position(|e| *e == profile.env).unwrap_or(0),
            SSL_MODES.iter().position(|m| *m == ssl).unwrap_or(1),
            CREDENTIAL_MODES
              .iter()
              .position(|m| *m == mode)
              .unwrap_or(0),
            command,
            tunnel_id,
          )
        }
        None => (
          String::new(),
          String::new(),
          String::new(),
          "5432".to_string(),
          String::new(),
          String::new(),
          0,
          1,
          0,
          String::new(),
          None,
        ),
      };
    self.refresh_tunnel_picker(tunnel_id.as_deref(), window, cx);
    self
      .form_name
      .update(cx, |i, cx| i.set_value(name, window, cx));
    self
      .form_group
      .update(cx, |i, cx| i.set_value(group, window, cx));
    self
      .form_host
      .update(cx, |i, cx| i.set_value(host, window, cx));
    self
      .form_port
      .update(cx, |i, cx| i.set_value(port, window, cx));
    self
      .form_database
      .update(cx, |i, cx| i.set_value(database, window, cx));
    self
      .form_user
      .update(cx, |i, cx| i.set_value(user, window, cx));
    self
      .form_password
      .update(cx, |i, cx| i.set_value("", window, cx));
    self
      .form_command
      .update(cx, |i, cx| i.set_value(command, window, cx));
    self.form_env.update(cx, |s, cx| {
      s.set_selected_index(Some(IndexPath::new(env_ix)), window, cx)
    });
    self.form_ssl.update(cx, |s, cx| {
      s.set_selected_index(Some(IndexPath::new(ssl_ix)), window, cx)
    });
    self.form_credential.update(cx, |s, cx| {
      s.set_selected_index(Some(IndexPath::new(cred_ix)), window, cx)
    });
  }

  fn selected_mode(&self, cx: &App) -> CredentialMode {
    let ix = self
      .form_credential
      .read(cx)
      .selected_index(cx)
      .map_or(0, |ix| ix.row);
    CREDENTIAL_MODES.get(ix).copied().unwrap_or_default()
  }

  /// Ids and labels rebuilt together: the picker maps by index, since tunnel
  /// names can collide.
  fn refresh_tunnel_picker(
    &mut self,
    current: Option<&str>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let tunnels = core::list_tunnels(&self.state);
    self.form_tunnel_ids = std::iter::once(None)
      .chain(tunnels.iter().map(|t| Some(t.id.clone())))
      .collect();
    let labels: Vec<String> = std::iter::once("none".to_string())
      .chain(tunnels.iter().map(|t| t.name.clone()))
      .collect();
    let ix = current
      .and_then(|id| {
        self
          .form_tunnel_ids
          .iter()
          .position(|t| t.as_deref() == Some(id))
      })
      .unwrap_or(0);
    self.form_tunnel.update(cx, |s, cx| {
      s.set_items(labels, window, cx);
      s.set_selected_index(Some(IndexPath::new(ix)), window, cx);
    });
  }

  fn form_input(&self, cx: &Context<Self>) -> Result<ConnectionInput, String> {
    let name = self.form_name.read(cx).value().trim().to_string();
    let host = self.form_host.read(cx).value().trim().to_string();
    let database = self.form_database.read(cx).value().trim().to_string();
    let user = self.form_user.read(cx).value().trim().to_string();
    if name.is_empty() || host.is_empty() || database.is_empty() || user.is_empty() {
      return Err("name, host, database and user are required".to_string());
    }
    let port: u16 = self
      .form_port
      .read(cx)
      .value()
      .trim()
      .parse()
      .map_err(|_| "the port is not a number".to_string())?;
    let group = self.form_group.read(cx).value().trim().to_string();
    let env = self
      .form_env
      .read(cx)
      .selected_value()
      .and_then(|label| ENVS.iter().find(|e| env_label(**e) == label))
      .copied()
      .unwrap_or(Env::Dev);
    let ssl_mode = self
      .form_ssl
      .read(cx)
      .selected_value()
      .and_then(|label| SSL_MODES.iter().find(|m| ssl_label(**m) == label))
      .copied()
      .unwrap_or(SslMode::Prefer);
    let credential = match self.selected_mode(cx) {
      CredentialMode::Keychain => CredentialSource::Keychain,
      CredentialMode::Prompt => CredentialSource::Prompt,
      CredentialMode::Command => {
        let command = self.form_command.read(cx).value().trim().to_string();
        if command.is_empty() {
          return Err("Command is required".to_string());
        }
        CredentialSource::Command {
          command,
          refresh_after_secs: None,
        }
      }
    };
    let password = self.form_password.read(cx).value().to_string();
    let tunnel_ix = self
      .form_tunnel
      .read(cx)
      .selected_index(cx)
      .map_or(0, |ix| ix.row);
    let tunnel_id = self.form_tunnel_ids.get(tunnel_ix).cloned().flatten();
    Ok(ConnectionInput {
      name,
      env,
      group: (!group.is_empty()).then_some(group),
      agent_access: AgentAccess::None,
      credential,
      params: ConnectorParams::Postgres(SqlServerParams {
        host,
        port,
        database,
        user,
        ssl_mode,
        ssl_root_cert: None,
        tunnel_id,
      }),
      password: (!password.is_empty()).then_some(password),
    })
  }

  fn test_form(&mut self, cx: &mut Context<Self>) {
    let input = match self.form_input(cx) {
      Ok(input) => input,
      Err(message) => {
        self.status = message.into();
        cx.notify();
        return;
      }
    };
    self.status = "testing...".into();
    cx.notify();
    let rx = core::test_input(self.state.clone(), input, self.editing.clone());
    self._task = cx.spawn(async move |this, cx| {
      let result = rx.await;
      let _ = this.update(cx, |this, cx| {
        match result {
          Ok(Ok(())) => this.status = "connection ok".into(),
          Ok(Err(Error::HostKeyUntrusted {
            host,
            port,
            fingerprint,
            key,
            previously_trusted,
            ..
          })) => {
            // The trust dialog owns this failure; retry re-reads the live form.
            this.status = SharedString::default();
            host_key::open_host_key_dialog(
              cx.entity(),
              this.state.clone(),
              HostKeyPrompt {
                host,
                port,
                fingerprint,
                key,
                previously_trusted,
              },
              cx,
              |view: &mut Self, result, cx| match result {
                Ok(()) => view.test_form(cx),
                Err(error) => {
                  view.status = format!("error: {error}").into();
                  cx.notify();
                }
              },
            );
          }
          Ok(Err(error)) => this.status = format!("error: {error}").into(),
          Err(_) => this.status = "error: test canceled".into(),
        }
        cx.notify();
      });
    });
  }

  fn save_form(&mut self, cx: &mut Context<Self>) {
    let input = match self.form_input(cx) {
      Ok(input) => input,
      Err(message) => {
        self.status = message.into();
        cx.notify();
        return;
      }
    };
    let rx = core::save_connection(self.state.clone(), self.editing.clone(), input);
    self._task = cx.spawn(async move |this, cx| {
      let result = rx.await;
      let _ = this.update(cx, |this, cx| {
        match result {
          Ok(Ok(_)) => this.refresh(cx),
          Ok(Err(error)) => this.status = format!("error: {error}").into(),
          Err(_) => {}
        }
        cx.notify();
      });
    });
  }

  fn revoke_command(
    &mut self,
    subject: SecretSubject,
    id: String,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    match core::revoke_credential_command(&self.state, subject, id) {
      Ok(()) => window.push_notification(
        Notification::info("The command will ask before running again"),
        cx,
      ),
      Err(error) => {
        self.status = format!("error: {error}").into();
        cx.notify();
      }
    }
  }

  pub(crate) fn open_export_dialog(&mut self, cx: &mut Context<Self>) {
    self.export_include_secrets = false;
    self.export_busy = false;
    self.export_error = None;
    let this = cx.entity();
    cx.defer(move |cx| {
      let Some(window_handle) = cx.active_window() else {
        return;
      };
      let _ = cx.update_window(window_handle, |_, window, cx| {
        this.update(cx, |view, cx| {
          view
            .export_passphrase
            .update(cx, |i, cx| i.set_value("", window, cx));
          view
            .export_confirm
            .update(cx, |i, cx| i.set_value("", window, cx));
        });
        let this = this.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
          let view = this.read(cx);
          let include = view.export_include_secrets;
          let busy = view.export_busy;
          let error = view.export_error.clone();
          let this_toggle = this.clone();
          let this_run = this.clone();
          dialog
            .title("Export connections")
            .w(px(460.))
            .on_ok(|_, _, _| false)
            .child(
              v_flex()
                .gap_3()
                .child(div().text_sm().child(
                  "Every connection, group and SSH tunnel in one file. Host keys stay on \
                   this machine: you confirm them again on the first connect.",
                ))
                .child(
                  v_flex()
                    .gap_2()
                    .p_3()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .child(
                      h_flex()
                        .justify_between()
                        .items_center()
                        .child(div().text_sm().child("Include passwords"))
                        .child(Switch::new("export-secrets").checked(include).on_click(
                          move |checked, _, cx| {
                            let checked = *checked;
                            this_toggle.update(cx, |view, cx| {
                              view.export_include_secrets = checked;
                              cx.notify();
                            });
                          },
                        )),
                    )
                    .child(
                      div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(if include {
                          "The file is encrypted with your passphrase. Lose it and the file \
                           is unreadable."
                        } else {
                          "Off: the file is plain text and safe to share, passwords get \
                           re-entered on the other side."
                        }),
                    ),
                )
                .when(include, |form| {
                  form.child(
                    v_form()
                      .child(
                        field()
                          .label("Passphrase")
                          .child(Input::new(&view.export_passphrase)),
                      )
                      .child(
                        field()
                          .label("Confirm passphrase")
                          .child(Input::new(&view.export_confirm)),
                      ),
                  )
                })
                .when_some(error, |form, error| {
                  form.child(
                    div()
                      .text_xs()
                      .font_family("IBM Plex Mono")
                      .text_color(cx.theme().danger)
                      .child(error),
                  )
                }),
            )
            .footer(
              h_flex()
                .gap_2()
                .justify_end()
                .child(
                  Button::new("export-cancel")
                    .label("Cancel")
                    .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                  Button::new("run-export")
                    .primary()
                    .label(if busy {
                      "Exporting…"
                    } else {
                      "Choose a file…"
                    })
                    .disabled(busy)
                    .debug_selector(|| "run-export".into())
                    .on_click(move |_, _, cx| {
                      this_run.update(cx, |this, cx| this.run_export(cx));
                    }),
                ),
            )
        });
      });
    });
  }

  fn run_export(&mut self, cx: &mut Context<Self>) {
    if self.export_busy {
      return;
    }
    let include = self.export_include_secrets;
    let passphrase = self.export_passphrase.read(cx).value().to_string();
    if include {
      let confirmation = self.export_confirm.read(cx).value().to_string();
      if let Some(issue) = transfer::passphrase_issue(&passphrase, &confirmation) {
        self.export_error = Some(issue.into());
        cx.notify();
        return;
      }
    }
    self.export_error = None;
    cx.notify();
    let home = std::env::var_os("HOME")
      .or_else(|| std::env::var_os("USERPROFILE"))
      .map(PathBuf::from)
      .unwrap_or_default();
    // The picker opens only once the passphrase would survive the round-trip.
    let picked = cx.prompt_for_new_path(&home, Some(transfer::DEFAULT_EXPORT_NAME));
    let state = self.state.clone();
    self._task = cx.spawn(async move |this, cx| {
      let path = match picked.await {
        Ok(Ok(Some(path))) => transfer::ensure_soquel_extension(path),
        Ok(Ok(None)) => return,
        Ok(Err(error)) => {
          let _ = this.update(cx, |this, cx| {
            this.export_error = Some(format!("{error}").into());
            cx.notify();
          });
          return;
        }
        Err(_) => return,
      };
      let _ = this.update(cx, |this, cx| {
        this.export_busy = true;
        cx.notify();
      });
      let rx = core::export_connections(state, path, include, include.then_some(passphrase));
      let result = rx.await;
      let mut done = None;
      let _ = this.update(cx, |this, cx| {
        this.export_busy = false;
        match result {
          Ok(Ok(summary)) => done = Some(transfer::export_summary_message(&summary)),
          Ok(Err(error)) => this.export_error = Some(format!("{error}").into()),
          Err(_) => {}
        }
        cx.notify();
      });
      if let Some(message) = done
        && let Some(handle) = cx.update(|cx| cx.active_window())
      {
        let _ = cx.update_window(handle, |_, window, cx| {
          window.close_dialog(cx);
          window.push_notification(Notification::success(message), cx);
        });
      }
    });
  }

  pub(crate) fn import_via_picker(&mut self, cx: &mut Context<Self>) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
      files: true,
      directories: false,
      multiple: false,
      prompt: None,
    });
    self._task = cx.spawn(async move |this, cx| {
      match picked.await {
        Ok(Ok(Some(paths))) => {
          if let Some(path) = paths.into_iter().next() {
            let _ = this.update(cx, |this, cx| this.open_import_dialog(path, cx));
          }
        }
        Ok(Ok(None)) => {}
        // No portal (WSLg): the drop path still works.
        Ok(Err(error)) => {
          let _ = this.update(cx, |this, cx| {
            this.status = format!("error: {error}; drop the file on the window instead").into();
            cx.notify();
          });
        }
        Err(_) => {}
      }
    });
  }

  fn open_import_dialog(&mut self, path: PathBuf, cx: &mut Context<Self>) {
    self.import_path = Some(path);
    self.import_preview = None;
    self.import_with_secrets = false;
    self.import_strategy = 0;
    self.import_locked = false;
    self.import_busy = false;
    self.import_error = None;
    let this = cx.entity();
    cx.defer(move |cx| {
      let Some(window_handle) = cx.active_window() else {
        return;
      };
      let _ = cx.update_window(window_handle, |_, window, cx| {
        this.update(cx, |view, cx| {
          view
            .import_passphrase
            .update(cx, |i, cx| i.set_value("", window, cx));
          view.load_import_preview(cx);
        });
        let this = this.clone();
        window.open_dialog(cx, move |dialog, _, cx| import_dialog(dialog, &this, cx));
      });
    });
  }

  fn load_import_preview(&mut self, cx: &mut Context<Self>) {
    let Some(path) = self.import_path.clone() else {
      return;
    };
    self.import_busy = true;
    self.import_error = None;
    cx.notify();
    let passphrase = {
      let typed = self.import_passphrase.read(cx).value().to_string();
      (!typed.is_empty()).then_some(typed)
    };
    let rx = core::preview_import(self.state.clone(), path, passphrase);
    self._task = cx.spawn(async move |this, cx| {
      let result = rx.await;
      let _ = this.update(cx, |this, cx| {
        this.import_busy = false;
        match result {
          Ok(Ok(preview)) => {
            this.import_locked = preview.needs_passphrase;
            this.import_preview = Some(preview);
          }
          // The lock stays as it was: a rejected passphrase keeps its field.
          Ok(Err(error)) => {
            this.import_preview = None;
            this.import_error = Some(format!("{error}").into());
          }
          Err(_) => {}
        }
        cx.notify();
      });
    });
  }

  fn run_import(&mut self, cx: &mut Context<Self>) {
    if self.import_busy || self.import_locked {
      return;
    }
    let Some(path) = self.import_path.clone() else {
      return;
    };
    let Some(preview) = &self.import_preview else {
      return;
    };
    if transfer::import_plan(preview).problems > 0 {
      return;
    }
    self.import_busy = true;
    self.import_error = None;
    cx.notify();
    let passphrase = {
      let typed = self.import_passphrase.read(cx).value().to_string();
      (!typed.is_empty()).then_some(typed)
    };
    let strategy = transfer::DUPLICATE_STRATEGIES
      .get(self.import_strategy)
      .copied()
      .unwrap_or(DuplicateStrategy::Skip);
    let rx = core::import_connections(
      self.state.clone(),
      path,
      passphrase,
      self.import_with_secrets,
      strategy,
    );
    self._task = cx.spawn(async move |this, cx| {
      let result = rx.await;
      let mut done = None;
      let _ = this.update(cx, |this, cx| {
        this.import_busy = false;
        match result {
          Ok(Ok(outcome)) => {
            done = Some(transfer::import_outcome_message(&outcome));
            this.refresh(cx);
            this
              .tunnels_section
              .update(cx, |tunnels, cx| tunnels.refresh(cx));
          }
          Ok(Err(error)) => this.import_error = Some(format!("{error}").into()),
          Err(_) => {}
        }
        cx.notify();
      });
      if let Some(message) = done
        && let Some(handle) = cx.update(|cx| cx.active_window())
      {
        let _ = cx.update_window(handle, |_, window, cx| {
          window.close_dialog(cx);
          window.push_notification(Notification::success(message), cx);
        });
      }
    });
  }

  fn delete(&mut self, id: String, cx: &mut Context<Self>) {
    let rx = core::delete_connection(self.state.clone(), id);
    self._task = cx.spawn(async move |this, cx| {
      let result = rx.await;
      let _ = this.update(cx, |this, cx| {
        if let Ok(Err(error)) = result {
          this.status = format!("error: {error}").into();
        }
        this.refresh(cx);
      });
    });
  }

  fn env_badge(&self, env: Env, cx: &Context<Self>) -> Div {
    let (bg, fg) = match env {
      Env::Dev => (cx.theme().muted, cx.theme().muted_foreground),
      Env::Staging => (cx.theme().yellow.opacity(0.15), cx.theme().yellow),
      Env::Prod => (cx.theme().danger.opacity(0.15), cx.theme().danger),
    };
    div()
      .px_1p5()
      .rounded(cx.theme().radius)
      .bg(bg)
      .text_color(fg)
      .text_xs()
      .child(env_label(env))
  }
}

fn outline_badge(text: String, color: Hsla, cx: &App) -> Div {
  div()
    .px_1p5()
    .rounded(cx.theme().radius)
    .border_1()
    .border_color(color.opacity(0.3))
    .text_color(color)
    .text_xs()
    .font_family("IBM Plex Mono")
    .child(text)
}

/// The import dialog body, re-read from the view every frame like the forms.
fn import_dialog(
  dialog: gpui_component::dialog::Dialog,
  this: &Entity<ConnectionsView>,
  cx: &App,
) -> gpui_component::dialog::Dialog {
  let view = this.read(cx);
  let path_text = view
    .import_path
    .as_ref()
    .map(|p| p.to_string_lossy().to_string())
    .unwrap_or_default();
  let plan = view.import_preview.as_ref().map(transfer::import_plan);
  let has_plan = plan.is_some();
  let blocked = plan.as_ref().is_some_and(|p| p.problems > 0);
  let locked = view.import_locked;
  let busy = view.import_busy;
  let error = view.import_error.clone();
  let counts = view
    .import_preview
    .as_ref()
    .map(|p| (p.connections.len(), p.tunnels.len(), p.encrypted));
  let with_secrets = view.import_with_secrets;
  let strategy_ix = view.import_strategy;
  let unlockable = !busy && !view.import_passphrase.read(cx).value().is_empty();
  let this_unlock = this.clone();
  let this_secrets = this.clone();
  let this_strategy = this.clone();
  let this_run = this.clone();
  let this_ok = this.clone();
  dialog
    .title("Import connections")
    .w(px(520.))
    // Enter unlocks while locked; it never runs the import.
    .on_ok(move |_, _, cx| {
      this_ok.update(cx, |view, cx| {
        if view.import_locked && !view.import_busy {
          view.load_import_preview(cx);
        }
      });
      false
    })
    .child(
      v_flex()
        .gap_3()
        .child(
          div()
            .text_xs()
            .font_family("IBM Plex Mono")
            .text_color(cx.theme().muted_foreground)
            .truncate()
            .child(path_text),
        )
        .when(locked, |body| {
          body.child(
            v_flex()
              .gap_2()
              .child(
                h_flex()
                  .gap_2()
                  .items_center()
                  .child(
                    div()
                      .text_color(cx.theme().muted_foreground)
                      .child(SoquelIcon::Lock),
                  )
                  .child(div().text_sm().child("This file is encrypted")),
              )
              .child(
                h_flex()
                  .gap_2()
                  .child(div().flex_1().child(Input::new(&view.import_passphrase)))
                  .child(
                    Button::new("import-unlock")
                      .outline()
                      .label("Unlock")
                      .disabled(!unlockable)
                      .debug_selector(|| "import-unlock".into())
                      .on_click(move |_, _, cx| {
                        this_unlock.update(cx, |view, cx| view.load_import_preview(cx));
                      }),
                  ),
              ),
          )
        })
        .when_some(plan.filter(|_| !locked), |body, plan| {
          let (connections, tunnels, encrypted) = counts.unwrap_or_default();
          let entries = plan.entries.len();
          body
            .child(
              h_flex()
                .flex_wrap()
                .gap_2()
                .items_center()
                .child(
                  div()
                    .text_sm()
                    .font_family("IBM Plex Mono")
                    .child(format!("{connections} connections, {tunnels} tunnels")),
                )
                .when(encrypted, |row| {
                  row.child(outline_badge(
                    "encrypted".to_string(),
                    cx.theme().muted_foreground,
                    cx,
                  ))
                })
                .when(plan.secrets > 0, |row| {
                  row.child(outline_badge(
                    format!("{} passwords", plan.secrets),
                    cx.theme().muted_foreground,
                    cx,
                  ))
                })
                .when(plan.commands > 0, |row| {
                  row.child(outline_badge(
                    format!("{} run a command", plan.commands),
                    cx.theme().yellow,
                    cx,
                  ))
                }),
            )
            .child(
              v_flex()
                .id("import-entries")
                .max_h(px(224.))
                .overflow_y_scroll()
                .border_1()
                .border_color(cx.theme().border)
                .rounded(cx.theme().radius)
                .children(plan.entries.iter().enumerate().map(|(ix, entry)| {
                  h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .when(ix + 1 < entries, |row| {
                      row.border_b_1().border_color(cx.theme().border)
                    })
                    .child(
                      div()
                        .text_color(cx.theme().muted_foreground)
                        .child(match entry.kind {
                          EntryKind::Connection => SoquelIcon::Database,
                          EntryKind::Tunnel => SoquelIcon::Cable,
                        }),
                    )
                    .child(
                      v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().truncate().child(entry.entry.name.clone()))
                        .child(
                          div()
                            .text_xs()
                            .font_family("IBM Plex Mono")
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(entry.entry.target.clone()),
                        ),
                    )
                    .when_some(entry.entry.problem.clone(), |row, problem| {
                      row.child(outline_badge(problem, cx.theme().danger, cx))
                    })
                    .when(
                      entry.entry.problem.is_none() && entry.entry.has_command,
                      |row| row.child(outline_badge("command".to_string(), cx.theme().yellow, cx)),
                    )
                    .when(
                      entry.entry.problem.is_none()
                        && !entry.entry.has_command
                        && entry.entry.duplicate,
                      |row| {
                        row.child(outline_badge(
                          "exists".to_string(),
                          cx.theme().muted_foreground,
                          cx,
                        ))
                      },
                    )
                })),
            )
            .when(plan.secrets > 0, |body| {
              body.child(
                v_flex()
                  .gap_2()
                  .p_3()
                  .border_1()
                  .border_color(cx.theme().border)
                  .rounded(cx.theme().radius)
                  .child(
                    h_flex()
                      .justify_between()
                      .items_center()
                      .child(div().text_sm().child("Bring the passwords"))
                      .child(
                        Switch::new("import-secrets")
                          .checked(with_secrets)
                          .on_click({
                            let this = this_secrets.clone();
                            move |checked, _, cx| {
                              let checked = *checked;
                              this.update(cx, |view, cx| {
                                view.import_with_secrets = checked;
                                cx.notify();
                              });
                            }
                          }),
                      ),
                  )
                  .child(
                    div()
                      .text_xs()
                      .text_color(cx.theme().muted_foreground)
                      .child(if with_secrets {
                        format!("{} passwords land in the keychain.", plan.secrets)
                      } else {
                        "Off: the connections arrive without them, ready to re-enter.".to_string()
                      }),
                  ),
              )
            })
            .when(plan.duplicates > 0 && !blocked, |body| {
              body.child(
                v_flex()
                  .gap_2()
                  .child(
                    div()
                      .text_sm()
                      .child(format!("{} already here", plan.duplicates)),
                  )
                  .child(
                    RadioGroup::vertical("import-strategy")
                      .selected_index(Some(strategy_ix))
                      .on_click({
                        let this = this_strategy.clone();
                        move |ix, _, cx| {
                          let ix = *ix;
                          this.update(cx, |view, cx| {
                            view.import_strategy = ix;
                            cx.notify();
                          });
                        }
                      })
                      .children(transfer::DUPLICATE_STRATEGIES.iter().enumerate().map(
                        |(ix, strategy)| {
                          Radio::new(("strategy", ix))
                            .label(transfer::strategy_label(*strategy))
                            .child(
                              div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(transfer::strategy_hint(*strategy)),
                            )
                        },
                      )),
                  ),
              )
            })
            .child(if blocked {
              div().text_xs().text_color(cx.theme().danger).child(
                "Nothing is imported while an entry is invalid: fix the file, or remove \
                 those entries.",
              )
            } else {
              div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!(
                  "Imported connections stay hidden from agents whatever the file says.{}",
                  if plan.commands > 0 {
                    " A credential command runs nothing until you read it and approve it."
                  } else {
                    ""
                  }
                ))
            })
        })
        .when_some(error, |body, error| {
          body.child(
            div()
              .text_xs()
              .font_family("IBM Plex Mono")
              .text_color(cx.theme().danger)
              .child(error),
          )
        }),
    )
    .footer(
      h_flex()
        .gap_2()
        .justify_end()
        .child(
          Button::new("import-cancel")
            .label("Cancel")
            .on_click(|_, window, cx| window.close_dialog(cx)),
        )
        .child(
          Button::new("run-import")
            .primary()
            .label(if busy { "Working…" } else { "Import" })
            .disabled(busy || blocked || !has_plan || locked)
            .debug_selector(|| "run-import".into())
            .on_click(move |_, _, cx| {
              this_run.update(cx, |this, cx| this.run_import(cx));
            }),
        ),
    )
}

impl Render for ConnectionsView {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let mut sorted = self.profiles.clone();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let groups = group_connections(&sorted);

    let connecting = self.connecting.clone();

    v_flex()
      .size_full()
      .bg(cx.theme().background)
      // A .soquel handed to the app from outside lands here: no file
      // association or single instance yet, the drop is the outside door.
      .drag_over::<ExternalPaths>(|style, _, _, cx| style.bg(cx.theme().accent))
      .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, _, cx| {
        let Some(path) = paths.paths().first().cloned() else {
          return;
        };
        let known = path
          .extension()
          .and_then(|e| e.to_str())
          .is_some_and(|e| e == "soquel" || e == "json");
        if known {
          this.open_import_dialog(path, cx);
        } else {
          this.status = "error: drop a .soquel export".into();
          cx.notify();
        }
      }))
      .child(
        h_flex()
          .px_4()
          .py_3()
          .justify_between()
          .items_center()
          .border_b_1()
          .border_color(cx.theme().border)
          .child(div().font_semibold().child("Connections"))
          .child(
            h_flex()
              .gap_2()
              .child(
                Button::new("open-import")
                  .ghost()
                  .small()
                  .label("Import…")
                  .debug_selector(|| "open-import".into())
                  .on_click(cx.listener(|this, _, _, cx| this.import_via_picker(cx))),
              )
              .child(
                Button::new("open-export")
                  .ghost()
                  .small()
                  .label("Export…")
                  .disabled(self.profiles.is_empty())
                  .debug_selector(|| "open-export".into())
                  .on_click(cx.listener(|this, _, _, cx| this.open_export_dialog(cx))),
              )
              .child(
                Button::new("new-connection")
                  .primary()
                  .small()
                  .label("New connection")
                  .on_click(cx.listener(|this, _, _, cx| this.open_form(None, cx))),
              ),
          ),
      )
      .when(!self.status.is_empty(), |this| {
        this.child(
          div()
            .px_4()
            .py_1()
            .text_sm()
            .text_color(cx.theme().danger)
            .child(self.status.clone()),
        )
      })
      .child(
        v_flex()
          .id("connection-list")
          .flex_1()
          .min_h_0()
          .overflow_y_scroll()
          .p_3()
          .gap_1()
          .when(self.profiles.is_empty(), |this| {
            this.child(
              v_flex()
                .items_center()
                .py_12()
                .gap_2()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("No connections yet. Add your first database.")
                .child(
                  Button::new("empty-import")
                    .ghost()
                    .small()
                    .label("Import a file")
                    .on_click(cx.listener(|this, _, _, cx| this.import_via_picker(cx))),
                ),
            )
          })
          .children(groups.into_iter().flat_map(|(group, profiles)| {
            let mut rows: Vec<AnyElement> = Vec::new();
            if let Some(group) = &group {
              rows.push(
                div()
                  .px_2()
                  .pt_2()
                  .text_xs()
                  .font_semibold()
                  .text_color(cx.theme().muted_foreground)
                  .child(group.clone())
                  .into_any_element(),
              );
            }
            for profile in profiles {
              let id = profile.id.clone();
              let edit_profile = profile.clone();
              let edit_selector_id = profile.id.clone();
              let delete_id = profile.id.clone();
              let revoke_id = profile.id.clone();
              let selector_id = profile.id.clone();
              let has_command = matches!(profile.credential, CredentialSource::Command { .. });
              let is_connecting = connecting.as_deref() == Some(profile.id.as_str());
              rows.push(
                h_flex()
                  .id(SharedString::from(format!("conn-{id}")))
                  .px_3()
                  .py_2()
                  .gap_3()
                  .items_center()
                  .rounded(cx.theme().radius)
                  .border_1()
                  .border_color(cx.theme().border)
                  .hover(|s| s.bg(cx.theme().accent))
                  .cursor_default()
                  .on_click(cx.listener(move |this, _, _, cx| this.connect(id.clone(), cx)))
                  .child(
                    v_flex()
                      .flex_1()
                      .min_w_0()
                      .child(
                        h_flex()
                          .gap_2()
                          .items_center()
                          .child(div().font_semibold().text_sm().child(profile.name.clone()))
                          .child(self.env_badge(profile.env, cx)),
                      )
                      .child(
                        div()
                          .text_xs()
                          .font_family("IBM Plex Mono")
                          .text_color(cx.theme().muted_foreground)
                          .child(dsn(&profile.params)),
                      ),
                  )
                  .when(is_connecting, |this| {
                    this.child(
                      div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("connecting..."),
                    )
                  })
                  .child(
                    Button::new(SharedString::from(format!("edit-{}", profile.id)))
                      .ghost()
                      .xsmall()
                      .label("Edit")
                      .debug_selector(move || format!("edit-{edit_selector_id}"))
                      .on_click(cx.listener(move |this, _, _, cx| {
                        // The row's own click connects: this click is ours.
                        cx.stop_propagation();
                        this.open_form(Some(edit_profile.clone()), cx);
                      })),
                  )
                  .when(has_command, |row| {
                    row.child(
                      Button::new(SharedString::from(format!("revoke-conn-{}", profile.id)))
                        .ghost()
                        .xsmall()
                        .label("Revoke command")
                        .debug_selector(move || format!("revoke-conn-{selector_id}"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                          cx.stop_propagation();
                          this.revoke_command(
                            SecretSubject::Connection,
                            revoke_id.clone(),
                            window,
                            cx,
                          );
                        })),
                    )
                  })
                  .child(
                    Button::new(SharedString::from(format!("delete-{}", profile.id)))
                      .ghost()
                      .xsmall()
                      .label("Delete")
                      .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.delete(delete_id.clone(), cx);
                      })),
                  )
                  .into_any_element(),
              );
            }
            rows
          }))
          .child(self.tunnels_section.clone()),
      )
  }
}

#[cfg(test)]
mod tests {
  // The parent globs gpui: shadow `test` back or #[gpui::test] recurses.
  use ::core::prelude::v1::test;
  use gpui::TestAppContext;
  use soquel_core::profiles::ConnectorKind;

  use super::*;

  fn profile(name: &str, group: Option<&str>) -> ConnectionProfile {
    ConnectionProfile {
      id: name.to_string(),
      name: name.to_string(),
      env: Env::Dev,
      group: group.map(Into::into),
      agent_access: Default::default(),
      credential: Default::default(),
      params: ConnectorParams::Postgres(SqlServerParams {
        host: "h".into(),
        port: 5432,
        database: "db".into(),
        user: "u".into(),
        ssl_mode: SslMode::Prefer,
        ssl_root_cert: None,
        tunnel_id: None,
      }),
    }
  }

  #[test]
  fn puts_ungrouped_first_then_groups_alphabetically() {
    let sections = group_connections(&[
      profile("c1", Some("zeta")),
      profile("c2", None),
      profile("c3", Some("alpha")),
      profile("c4", Some("zeta")),
    ]);
    let groups: Vec<Option<&str>> = sections.iter().map(|(g, _)| g.as_deref()).collect();
    assert_eq!(groups, vec![None, Some("alpha"), Some("zeta")]);
    let zeta: Vec<&str> = sections[2].1.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(zeta, vec!["c1", "c4"]);
  }

  #[test]
  fn omits_the_ungrouped_section_when_everything_is_grouped() {
    let sections = group_connections(&[profile("c1", Some("a"))]);
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].0.as_deref(), Some("a"));
  }

  #[test]
  fn dsn_renders_per_kind() {
    let pg = profile("c", None);
    assert_eq!(dsn(&pg.params), "postgres://u@h:5432/db");
    assert_eq!(
      dsn(&ConnectorParams::Sqlite {
        path: "/tmp/x.db".into()
      }),
      "sqlite:///tmp/x.db"
    );
    let _ = ConnectorKind::Postgres;
  }

  #[gpui::test]
  fn form_input_maps_and_validates(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      tempfile::tempdir().unwrap().path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let view = cx.update(|window, cx| cx.new(|cx| ConnectionsView::new(state, window, cx)));
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        // Empty form: the required fields refuse.
        assert!(view.form_input(cx).is_err());

        view
          .form_name
          .update(cx, |i, cx| i.set_value("db1", window, cx));
        view
          .form_host
          .update(cx, |i, cx| i.set_value("h", window, cx));
        view
          .form_database
          .update(cx, |i, cx| i.set_value("app", window, cx));
        view
          .form_user
          .update(cx, |i, cx| i.set_value("u", window, cx));
        view
          .form_port
          .update(cx, |i, cx| i.set_value("not-a-port", window, cx));
        assert!(view.form_input(cx).unwrap_err().contains("port"));

        view
          .form_port
          .update(cx, |i, cx| i.set_value("5433", window, cx));
        view
          .form_password
          .update(cx, |i, cx| i.set_value("s3cret", window, cx));
        let input = view.form_input(cx).unwrap();
        assert_eq!(input.name, "db1");
        assert_eq!(input.password.as_deref(), Some("s3cret"));
        let ConnectorParams::Postgres(params) = &input.params else {
          panic!("postgres form");
        };
        assert_eq!(params.port, 5433);
        // No group typed = no group stored, not an empty string.
        assert_eq!(input.group, None);
      });
    });
  }

  fn command_input(command: &str) -> ConnectionInput {
    ConnectionInput {
      name: "imported iam".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::None,
      credential: CredentialSource::Command {
        command: command.to_string(),
        refresh_after_secs: None,
      },
      params: ConnectorParams::Postgres(SqlServerParams {
        // Nothing listens here: the retry after approval fails fast.
        host: "127.0.0.1".to_string(),
        port: 1,
        database: "app".to_string(),
        user: "u".to_string(),
        ssl_mode: SslMode::Prefer,
        ssl_root_cert: None,
        tunnel_id: None,
      }),
      password: None,
    }
  }

  fn test_state() -> (tempfile::TempDir, std::sync::Arc<soquel_core::AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    (dir, state)
  }

  #[gpui::test]
  fn an_unapproved_command_opens_the_approval_dialog_and_approve_retries(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    let (_dir, state) = test_state();
    let line = "echo swordfish";
    let profile = soquel_core::ops::create_connection(&state, &command_input(line)).unwrap();
    // Imported shape: the command sits in the store with no approval.
    core::revoke_credential_command(&state, SecretSubject::Connection, profile.id.clone()).unwrap();

    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    let id = profile.id.clone();
    cx.update(|_, cx| view.update(cx, |view, cx| view.connect(id, cx)));

    crate::test_support::wait_until(cx, "the approval dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });
    let bounds = cx
      .debug_bounds("approve-command")
      .expect("the approve button is painted inside the dialog");
    cx.simulate_click(bounds.center(), Modifiers::none());

    let key = soquel_core::secrets::SecretKey::Connection(profile.id.clone());
    crate::test_support::wait_until(cx, "the approval to land", |_| {
      state
        .command_approvals
        .lock()
        .unwrap()
        .is_approved(&key, line)
    });
    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    // The retry ran: the connect fails on the closed port, not on approval.
    crate::test_support::wait_until(cx, "the retried connect to fail", |cx| {
      cx.update(|_, cx| {
        let status = view.read(cx).status.clone();
        status.starts_with("error") && !status.contains("approved")
      })
    });
  }

  #[gpui::test]
  fn escape_leaves_the_command_unapproved(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    let (_dir, state) = test_state();
    let line = "echo swordfish";
    let profile = soquel_core::ops::create_connection(&state, &command_input(line)).unwrap();
    core::revoke_credential_command(&state, SecretSubject::Connection, profile.id.clone()).unwrap();

    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    let id = profile.id.clone();
    cx.update(|_, cx| view.update(cx, |view, cx| view.connect(id, cx)));
    crate::test_support::wait_until(cx, "the approval dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });

    // Enter approves nothing and the dialog stays up.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(cx.update(|window, cx| window.has_active_dialog(cx)));

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    let key = soquel_core::secrets::SecretKey::Connection(profile.id.clone());
    assert!(
      !state
        .command_approvals
        .lock()
        .unwrap()
        .is_approved(&key, line)
    );
  }

  #[gpui::test]
  fn revoke_shows_only_for_command_profiles_and_revokes(cx: &mut TestAppContext) {
    let (_dir, state) = test_state();
    let line = "echo swordfish";
    let with_command = soquel_core::ops::create_connection(&state, &command_input(line)).unwrap();
    let plain = soquel_core::ops::create_connection(&state, &{
      let mut input = command_input(line);
      input.credential = CredentialSource::Keychain;
      input
    })
    .unwrap();

    let (_view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    cx.run_until_parked();

    assert!(
      cx.debug_bounds(crate::test_support::selector(format!(
        "revoke-conn-{}",
        plain.id
      )))
      .is_none(),
      "no command, no revoke button"
    );
    let bounds = cx
      .debug_bounds(crate::test_support::selector(format!(
        "revoke-conn-{}",
        with_command.id
      )))
      .expect("a command profile carries the revoke button");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();

    let key = soquel_core::secrets::SecretKey::Connection(with_command.id.clone());
    assert!(
      !state
        .command_approvals
        .lock()
        .unwrap()
        .is_approved(&key, line)
    );
  }

  #[gpui::test]
  fn enter_submits_the_secret_prompt(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    let (_dir, state) = test_state();
    let profile = soquel_core::ops::create_connection(&state, &{
      let mut input = command_input("unused");
      input.credential = CredentialSource::Prompt;
      input
    })
    .unwrap();

    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    let id = profile.id.clone();
    cx.update(|_, cx| view.update(cx, |view, cx| view.connect(id, cx)));
    crate::test_support::wait_until(cx, "the secret prompt", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });

    // The prompt focuses its input on open: type, then Enter submits.
    cx.simulate_input("hunter2");
    cx.simulate_keystrokes("enter");

    crate::test_support::wait_until(cx, "the retried connect to fail on the port", |cx| {
      cx.update(|_, cx| view.read(cx).status.starts_with("error"))
    });
    // The retry got past SecretRequired: the failure is the closed port.
    cx.update(|_, cx| {
      let status = view.read(cx).status.clone();
      assert!(!status.contains("password"), "unexpected: {status}");
    });
  }

  #[gpui::test]
  fn form_input_maps_the_command_mode(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      tempfile::tempdir().unwrap().path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let view = cx.update(|window, cx| cx.new(|cx| ConnectionsView::new(state, window, cx)));
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .form_name
          .update(cx, |i, cx| i.set_value("db1", window, cx));
        view
          .form_host
          .update(cx, |i, cx| i.set_value("h", window, cx));
        view
          .form_port
          .update(cx, |i, cx| i.set_value("5432", window, cx));
        view
          .form_database
          .update(cx, |i, cx| i.set_value("app", window, cx));
        view
          .form_user
          .update(cx, |i, cx| i.set_value("u", window, cx));
        let command_ix = CREDENTIAL_MODES
          .iter()
          .position(|m| *m == CredentialMode::Command)
          .unwrap();
        view.form_credential.update(cx, |s, cx| {
          s.set_selected_index(Some(IndexPath::new(command_ix)), window, cx)
        });

        assert_eq!(view.form_input(cx).unwrap_err(), "Command is required");

        view
          .form_command
          .update(cx, |i, cx| i.set_value(" vault-db {host} ", window, cx));
        let input = view.form_input(cx).unwrap();
        assert_eq!(
          input.credential,
          CredentialSource::Command {
            command: "vault-db {host}".to_string(),
            refresh_after_secs: None
          }
        );
      });
    });
  }

  #[gpui::test]
  fn prefill_selects_command_and_keeps_the_line(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      tempfile::tempdir().unwrap().path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let stored = soquel_core::ops::create_connection(
      &state,
      &ConnectionInput {
        name: "iam".to_string(),
        env: Env::Dev,
        group: None,
        agent_access: AgentAccess::None,
        credential: CredentialSource::Command {
          command: "vault-db {host}".to_string(),
          refresh_after_secs: None,
        },
        params: profile("seed", None).params,
        password: None,
      },
    )
    .unwrap();

    let view = cx.update(|window, cx| cx.new(|cx| ConnectionsView::new(state.clone(), window, cx)));
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view.prefill_form(Some(&stored), window, cx);
        // The stored mode survives the round-trip instead of silently
        // rewriting to Keychain (and revoking the approval with it).
        assert_eq!(view.selected_mode(cx), CredentialMode::Command);
        assert_eq!(view.form_command.read(cx).value(), "vault-db {host}");
        let input = view.form_input(cx).unwrap();
        assert_eq!(input.credential, stored.credential);
      });
    });
  }

  #[gpui::test]
  fn tunnel_picker_maps_by_index_not_label(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      tempfile::tempdir().unwrap().path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    // Two tunnels with the same name: only the index tells them apart.
    let tunnel_input = || soquel_core::tunnels::TunnelInput {
      name: "bastion".to_string(),
      host: "bastion.internal".to_string(),
      port: 22,
      user: "deploy".to_string(),
      auth: soquel_core::tunnels::SshAuth::Agent,
      credential: CredentialSource::Keychain,
      secret: None,
    };
    let _first = soquel_core::ops::create_tunnel(&state, &tunnel_input()).unwrap();
    let second = soquel_core::ops::create_tunnel(&state, &tunnel_input()).unwrap();

    let view = cx.update(|window, cx| cx.new(|cx| ConnectionsView::new(state, window, cx)));
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .form_name
          .update(cx, |i, cx| i.set_value("db1", window, cx));
        view
          .form_host
          .update(cx, |i, cx| i.set_value("h", window, cx));
        view
          .form_port
          .update(cx, |i, cx| i.set_value("5432", window, cx));
        view
          .form_database
          .update(cx, |i, cx| i.set_value("app", window, cx));
        view
          .form_user
          .update(cx, |i, cx| i.set_value("u", window, cx));

        view.refresh_tunnel_picker(Some(&second.id), window, cx);
        let input = view.form_input(cx).unwrap();
        let ConnectorParams::Postgres(params) = &input.params else {
          panic!("postgres form");
        };
        assert_eq!(params.tunnel_id.as_deref(), Some(second.id.as_str()));

        // Index 0 is the "none" sentinel.
        view.form_tunnel.update(cx, |s, cx| {
          s.set_selected_index(Some(IndexPath::new(0)), window, cx)
        });
        let input = view.form_input(cx).unwrap();
        let ConnectorParams::Postgres(params) = &input.params else {
          panic!("postgres form");
        };
        assert_eq!(params.tunnel_id, None);
      });
    });
  }

  fn plain_input(name: &str, host: &str) -> ConnectionInput {
    ConnectionInput {
      name: name.to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::None,
      credential: CredentialSource::Keychain,
      params: ConnectorParams::Postgres(SqlServerParams {
        host: host.to_string(),
        port: 5432,
        database: "app".to_string(),
        user: "u".to_string(),
        ssl_mode: SslMode::Prefer,
        ssl_root_cert: None,
        tunnel_id: None,
      }),
      password: None,
    }
  }

  /// A second machine's stores, exported to a file the tests import.
  fn export_fixture(
    build: impl FnOnce(&soquel_core::AppState),
    include_secrets: bool,
    passphrase: Option<&str>,
  ) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let source = soquel_core::AppState::for_tests(
      dir.path().join("source").as_path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    );
    build(&source);
    let path = dir.path().join("fixture.soquel");
    soquel_core::transfer::export(&source, &path, include_secrets, passphrase).unwrap();
    (dir, path)
  }

  #[gpui::test]
  fn export_picks_a_file_and_writes_it(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    let (dir, state) = test_state();
    soquel_core::ops::create_connection(&state, &plain_input("pg", "db.internal")).unwrap();
    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    cx.run_until_parked();

    let bounds = cx.debug_bounds("open-export").expect("export button");
    cx.simulate_click(bounds.center(), Modifiers::none());
    crate::test_support::wait_until(cx, "the export dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });
    let bounds = cx.debug_bounds("run-export").expect("run button");
    cx.simulate_click(bounds.center(), Modifiers::none());

    // A bare name typed into the picker still lands as .soquel.
    let out = dir.path().join("out");
    cx.simulate_new_path_selection(|_| Some(out.clone()));
    let written = dir.path().join("out.soquel");
    crate::test_support::wait_until(cx, "the export to land", |cx| {
      written.exists() && !cx.update(|window, cx| window.has_active_dialog(cx))
    });

    let (fresh_dir, fresh) = test_state();
    let preview = soquel_core::transfer::preview_file(&fresh, &written, None).unwrap();
    assert!(!preview.encrypted);
    assert_eq!(preview.connections.len(), 1);
    assert_eq!(preview.connections[0].name, "pg");
    drop(fresh_dir);
    drop(view);
  }

  #[gpui::test]
  fn export_validates_the_passphrase_before_the_picker(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    let (_dir, state) = test_state();
    soquel_core::ops::create_connection(&state, &plain_input("pg", "db.internal")).unwrap();
    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    cx.update(|_, cx| view.update(cx, |view, cx| view.open_export_dialog(cx)));
    crate::test_support::wait_until(cx, "the export dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view.export_include_secrets = true;
        view
          .export_passphrase
          .update(cx, |i, cx| i.set_value("short", window, cx));
        view
          .export_confirm
          .update(cx, |i, cx| i.set_value("short", window, cx));
        cx.notify();
      });
    });
    // Re-fetch bounds each time: the error line shifts the button down.
    let click_run = |cx: &mut gpui::VisualTestContext| {
      let bounds = cx.debug_bounds("run-export").expect("run button");
      cx.simulate_click(bounds.center(), Modifiers::none());
      cx.run_until_parked();
    };
    click_run(cx);
    cx.update(|_, cx| {
      assert_eq!(
        view.read(cx).export_error.as_deref(),
        Some("Use at least 8 characters.")
      );
    });
    assert!(!cx.did_prompt_for_new_path(), "no picker before validation");

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .export_passphrase
          .update(cx, |i, cx| i.set_value("long enough", window, cx));
        view
          .export_confirm
          .update(cx, |i, cx| i.set_value("long enuff", window, cx));
      });
    });
    click_run(cx);
    cx.update(|_, cx| {
      assert_eq!(
        view.read(cx).export_error.as_deref(),
        Some("The two passphrases do not match.")
      );
    });
    assert!(!cx.did_prompt_for_new_path());

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .export_confirm
          .update(cx, |i, cx| i.set_value("long enough", window, cx));
      });
    });
    click_run(cx);
    assert!(cx.did_prompt_for_new_path(), "a sound passphrase opens it");
    // Cancelling the picker leaves the dialog up.
    cx.simulate_new_path_selection(|_| None);
    cx.run_until_parked();
    assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
  }

  #[gpui::test]
  fn export_with_secrets_writes_an_encrypted_file(cx: &mut TestAppContext) {
    let (dir, state) = test_state();
    soquel_core::ops::create_connection(
      &state,
      &ConnectionInput {
        password: Some("s3cret".to_string()),
        ..plain_input("pg", "db.internal")
      },
    )
    .unwrap();
    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    cx.update(|_, cx| view.update(cx, |view, cx| view.open_export_dialog(cx)));
    // Let the deferred reset run before setting the fields it clears.
    cx.run_until_parked();
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view.export_include_secrets = true;
        view
          .export_passphrase
          .update(cx, |i, cx| i.set_value("correct horse", window, cx));
        view
          .export_confirm
          .update(cx, |i, cx| i.set_value("correct horse", window, cx));
      });
    });
    cx.update(|_, cx| view.update(cx, |view, cx| view.run_export(cx)));
    cx.run_until_parked();
    let out = dir.path().join("sealed.soquel");
    cx.simulate_new_path_selection(|_| Some(out.clone()));
    crate::test_support::wait_until(cx, "the sealed export", |_| out.exists());

    let (fresh_dir, fresh) = test_state();
    let announced = soquel_core::transfer::preview_file(&fresh, &out, None).unwrap();
    assert!(announced.needs_passphrase);
    let opened = soquel_core::transfer::preview_file(&fresh, &out, Some("correct horse")).unwrap();
    assert!(opened.connections[0].has_secret);
    drop(fresh_dir);
  }

  #[gpui::test]
  fn export_is_dead_with_no_connections(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    let (_dir, state) = test_state();
    let (_view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    cx.run_until_parked();
    let bounds = cx.debug_bounds("open-export").expect("export button");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
  }

  #[gpui::test]
  fn import_previews_then_imports_via_the_picker(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    let (_fixture_dir, file) = export_fixture(
      |source| {
        let tunnel = soquel_core::ops::create_tunnel(
          source,
          &soquel_core::tunnels::TunnelInput {
            name: "bastion".to_string(),
            host: "bastion.internal".to_string(),
            port: 22,
            user: "deploy".to_string(),
            auth: soquel_core::tunnels::SshAuth::Agent,
            credential: CredentialSource::Keychain,
            secret: None,
          },
        )
        .unwrap();
        soquel_core::ops::create_connection(source, &plain_input("already here", "db.internal"))
          .unwrap();
        let mut riding = plain_input("new one", "10.0.0.2");
        riding.params.set_tunnel_id(Some(tunnel.id));
        soquel_core::ops::create_connection(source, &riding).unwrap();
      },
      false,
      None,
    );

    let (_dir, state) = test_state();
    soquel_core::ops::create_connection(&state, &plain_input("already here", "db.internal"))
      .unwrap();
    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    cx.run_until_parked();

    let bounds = cx.debug_bounds("open-import").expect("import button");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(cx.did_prompt_for_paths());
    cx.simulate_path_prompt_response(|options| {
      assert!(options.files && !options.directories && !options.multiple);
      Some(vec![file.clone()])
    });
    crate::test_support::wait_until(cx, "the import preview", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
        && cx.update(|_, cx| view.read(cx).import_preview.is_some())
    });
    cx.update(|_, cx| {
      let plan = transfer::import_plan(view.read(cx).import_preview.as_ref().unwrap());
      assert_eq!(plan.duplicates, 1);
      assert_eq!(plan.problems, 0);
    });

    let bounds = cx.debug_bounds("run-import").expect("run button");
    cx.simulate_click(bounds.center(), Modifiers::none());
    crate::test_support::wait_until(cx, "the import to land", |cx| {
      !cx.update(|window, cx| window.has_active_dialog(cx))
    });
    // Skip left the duplicate alone; the new connection and its tunnel landed.
    cx.update(|_, cx| assert_eq!(view.read(cx).profiles.len(), 2));
    assert_eq!(state.tunnels.lock().unwrap().list().len(), 1);
  }

  #[gpui::test]
  fn a_wrong_passphrase_stays_sticky(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    let (_fixture_dir, file) = export_fixture(
      |source| {
        soquel_core::ops::create_connection(
          source,
          &ConnectionInput {
            password: Some("s3cret".to_string()),
            ..plain_input("sealed pg", "db.internal")
          },
        )
        .unwrap();
      },
      true,
      Some("correct horse"),
    );

    let (_dir, state) = test_state();
    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    cx.update(|_, cx| {
      view.update(cx, |view, cx| view.open_import_dialog(file.clone(), cx));
    });
    crate::test_support::wait_until(cx, "the locked preview", |cx| {
      cx.update(|_, cx| view.read(cx).import_locked)
    });

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .import_passphrase
          .update(cx, |i, cx| i.set_value("wrong horse", window, cx));
        cx.notify();
      });
    });
    let unlock = cx.debug_bounds("import-unlock").expect("unlock button");
    cx.simulate_click(unlock.center(), Modifiers::none());
    crate::test_support::wait_until(cx, "the refusal", |cx| {
      cx.update(|_, cx| view.read(cx).import_error.is_some())
    });
    cx.update(|_, cx| {
      let view = view.read(cx);
      assert!(view.import_locked, "the field stays for the retry");
      assert!(view.import_preview.is_none());
      assert!(
        view
          .import_error
          .as_ref()
          .unwrap()
          .contains("wrong passphrase")
      );
    });
    assert!(
      cx.debug_bounds("import-unlock").is_some(),
      "the passphrase step is still painted"
    );

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .import_passphrase
          .update(cx, |i, cx| i.set_value("correct horse", window, cx));
      });
    });
    let unlock = cx.debug_bounds("import-unlock").expect("unlock button");
    cx.simulate_click(unlock.center(), Modifiers::none());
    crate::test_support::wait_until(cx, "the unlocked preview", |cx| {
      cx.update(|_, cx| {
        let view = view.read(cx);
        !view.import_locked && view.import_preview.is_some()
      })
    });

    cx.update(|_, cx| {
      view.update(cx, |view, cx| {
        view.import_with_secrets = true;
        view.run_import(cx);
      });
    });
    crate::test_support::wait_until(cx, "the sealed import", |cx| {
      !cx.update(|window, cx| window.has_active_dialog(cx))
    });
    let landed = state
      .profiles
      .lock()
      .unwrap()
      .list()
      .into_iter()
      .find(|p| p.name == "sealed pg")
      .expect("the sealed connection landed");
    assert_eq!(
      state
        .secrets
        .get(&soquel_core::secrets::SecretKey::Connection(landed.id))
        .unwrap()
        .as_deref(),
      Some("s3cret")
    );
  }

  #[gpui::test]
  fn a_problem_entry_blocks_the_import(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    let (_fixture_dir, file) = export_fixture(
      |source| {
        soquel_core::ops::create_connection(source, &plain_input("fine", "db.internal")).unwrap();
        soquel_core::ops::create_connection(source, &plain_input("no host", "SENTINEL")).unwrap();
      },
      false,
      None,
    );
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, text.replace("SENTINEL", "")).unwrap();

    let (_dir, state) = test_state();
    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    cx.update(|_, cx| {
      view.update(cx, |view, cx| view.open_import_dialog(file.clone(), cx));
    });
    crate::test_support::wait_until(cx, "the blocked preview", |cx| {
      cx.update(|_, cx| view.read(cx).import_preview.is_some())
    });
    cx.update(|_, cx| {
      let plan = transfer::import_plan(view.read(cx).import_preview.as_ref().unwrap());
      assert_eq!(plan.problems, 1);
    });

    let bounds = cx.debug_bounds("run-import").expect("run button");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(
      cx.update(|window, cx| window.has_active_dialog(cx)),
      "a blocked import goes nowhere"
    );
    assert!(state.profiles.lock().unwrap().list().is_empty());
  }

  #[gpui::test]
  fn dropping_a_file_opens_the_import_dialog(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    let (fixture_dir, file) = export_fixture(
      |source| {
        soquel_core::ops::create_connection(source, &plain_input("dropped", "db.internal"))
          .unwrap();
      },
      false,
      None,
    );

    let (_dir, state) = test_state();
    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    cx.run_until_parked();
    let position = cx.update(|window, _| {
      let size = window.viewport_size();
      gpui::point(size.width / 2., size.height / 2.)
    });

    // The wrong kind of file only reaches the status line.
    let stray = fixture_dir.path().join("notes.txt");
    std::fs::write(&stray, "not an export").unwrap();
    cx.simulate_event(FileDropEvent::Entered {
      position,
      paths: ExternalPaths(vec![stray].into()),
    });
    cx.simulate_event(FileDropEvent::Submit { position });
    cx.simulate_event(FileDropEvent::Exited);
    cx.run_until_parked();
    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    cx.update(|_, cx| {
      assert!(view.read(cx).status.contains(".soquel"));
    });

    cx.simulate_event(FileDropEvent::Entered {
      position,
      paths: ExternalPaths(vec![file.clone()].into()),
    });
    cx.simulate_event(FileDropEvent::Submit { position });
    cx.simulate_event(FileDropEvent::Exited);
    crate::test_support::wait_until(cx, "the dropped preview", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });
    cx.update(|_, cx| {
      assert_eq!(view.read(cx).import_path.as_ref(), Some(&file));
    });
  }

  #[gpui::test]
  fn replace_overwrites_the_duplicate(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    // Same name + target (host:port/database) so it dedupes; the user field
    // is the visible difference Replace should carry over.
    let (_fixture_dir, file) = export_fixture(
      |source| {
        let mut input = plain_input("pg", "db.internal");
        let ConnectorParams::Postgres(params) = &mut input.params else {
          unreachable!()
        };
        params.user = "from_file".to_string();
        soquel_core::ops::create_connection(source, &input).unwrap();
      },
      false,
      None,
    );

    let (_dir, state) = test_state();
    soquel_core::ops::create_connection(&state, &plain_input("pg", "db.internal")).unwrap();
    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    cx.update(|_, cx| {
      view.update(cx, |view, cx| view.open_import_dialog(file.clone(), cx));
    });
    crate::test_support::wait_until(cx, "the preview", |cx| {
      cx.update(|_, cx| view.read(cx).import_preview.is_some())
    });

    // Radio has no debug_selector: the index is pinned by the transfer units.
    cx.update(|_, cx| {
      view.update(cx, |view, cx| {
        view.import_strategy = 1;
        view.run_import(cx);
      });
    });
    crate::test_support::wait_until(cx, "the replace to land", |cx| {
      !cx.update(|window, cx| window.has_active_dialog(cx))
    });
    let profiles = state.profiles.lock().unwrap().list();
    assert_eq!(profiles.len(), 1);
    let ConnectorParams::Postgres(params) = &profiles[0].params else {
      panic!("postgres profile");
    };
    assert_eq!(params.user, "from_file", "the file version won");
  }

  #[gpui::test]
  fn a_command_tunnel_asks_for_approval_before_the_dial(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    let (_dir, state) = test_state();
    let line = "echo swordfish";
    let tunnel = soquel_core::ops::create_tunnel(
      &state,
      &soquel_core::tunnels::TunnelInput {
        name: "vault bastion".to_string(),
        host: "127.0.0.1".to_string(),
        port: 1,
        user: "deploy".to_string(),
        auth: soquel_core::tunnels::SshAuth::Password,
        credential: CredentialSource::Command {
          command: line.to_string(),
          refresh_after_secs: None,
        },
        secret: None,
      },
    )
    .unwrap();
    core::revoke_credential_command(&state, SecretSubject::Tunnel, tunnel.id.clone()).unwrap();
    let mut riding = plain_input("through it", "127.0.0.1");
    riding.params.set_tunnel_id(Some(tunnel.id.clone()));
    riding.password = Some("pw".to_string());
    let profile = soquel_core::ops::create_connection(&state, &riding).unwrap();

    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    let id = profile.id.clone();
    cx.update(|_, cx| view.update(cx, |view, cx| view.connect(id, cx)));
    crate::test_support::wait_until(cx, "the tunnel approval dialog", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });

    let bounds = cx.debug_bounds("approve-command").expect("approve button");
    cx.simulate_click(bounds.center(), Modifiers::none());
    let key = soquel_core::secrets::SecretKey::Tunnel(tunnel.id.clone());
    crate::test_support::wait_until(cx, "the tunnel approval to land", |_| {
      state
        .command_approvals
        .lock()
        .unwrap()
        .is_approved(&key, line)
    });
    // The retry got past the approval and died on the closed ssh port.
    crate::test_support::wait_until(cx, "the retried connect to fail", |cx| {
      cx.update(|_, cx| {
        let status = view.read(cx).status.clone();
        status.starts_with("error") && !status.contains("approved")
      })
    });
  }

  #[gpui::test]
  fn edit_does_not_connect(cx: &mut TestAppContext) {
    use gpui_component::WindowExt;

    let (_dir, state) = test_state();
    let profile =
      soquel_core::ops::create_connection(&state, &plain_input("pg", "127.0.0.1")).unwrap();
    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });
    cx.run_until_parked();

    let bounds = cx
      .debug_bounds(crate::test_support::selector(format!(
        "edit-{}",
        profile.id
      )))
      .expect("edit button");
    cx.simulate_click(bounds.center(), Modifiers::none());
    crate::test_support::wait_until(cx, "the edit form", |cx| {
      cx.update(|window, cx| window.has_active_dialog(cx))
    });
    std::thread::sleep(std::time::Duration::from_millis(100));
    cx.run_until_parked();
    cx.update(|_, cx| {
      let view = view.read(cx);
      assert!(view.connecting.is_none(), "edit must not connect");
      assert!(view.status.is_empty());
    });
  }
}
