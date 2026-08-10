use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::form::{field, v_form};
use gpui_component::input::{Input, InputState};
use gpui_component::select::{Select, SelectState};
use gpui_component::{ActiveTheme, IndexPath, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use soquel_core::AppState;
use soquel_core::error::{Error, SecretSubject};
use soquel_core::profiles::{
  AgentAccess, ConnectionInput, ConnectionProfile, ConnectorParams, CredentialSource, Env,
  SqlServerParams, SslMode,
};

use crate::core::{self, Db};

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

/// The form only speaks postgres for now; the other kinds arrive with their UI.
const CREDENTIAL_MODES: [&str; 2] = ["Saved in the keychain", "Ask every time"];

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
  form_env: Entity<SelectState<Vec<String>>>,
  form_ssl: Entity<SelectState<Vec<String>>>,
  form_credential: Entity<SelectState<Vec<String>>>,
  prompt_password: Entity<InputState>,
  prompt_remember: bool,
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
          .map(|m| m.to_string())
          .collect::<Vec<_>>(),
        Some(IndexPath::default()),
        window,
        cx,
      )
    });

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
      form_env,
      form_ssl,
      form_credential,
      prompt_password,
      prompt_remember: false,
      _task: Task::ready(()),
    }
  }

  fn refresh(&mut self, cx: &mut Context<Self>) {
    self.profiles = core::list_connections(&self.state);
    cx.notify();
  }

  fn connect(&mut self, id: String, cx: &mut Context<Self>) {
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
          let input_for_footer = input.clone();
          let (subject, target_id, target_name, connect_id) = (
            subject,
            target_id.clone(),
            target_name.clone(),
            connect_id.clone(),
          );
          let remember = this.read(cx).prompt_remember;
          dialog
            .title(format!("Password for {target_name}"))
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
                    .on_click(move |_, window, cx| {
                      let secret = input_for_footer.read(cx).value().to_string();
                      window.close_dialog(cx);
                      let (subject, target_id, connect_id) =
                        (subject, target_id.clone(), connect_id.clone());
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
                    }),
                ),
            )
        });
      });
    });
  }

  fn open_form(&mut self, editing: Option<ConnectionProfile>, cx: &mut Context<Self>) {
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
            let (name, group, host, port, database, user, env_ix, ssl_ix, cred_ix) = match &editing
            {
              Some(profile) => {
                let (host, port, database, user, ssl) = match &profile.params {
                  ConnectorParams::Postgres(p) => (
                    p.host.clone(),
                    p.port.to_string(),
                    p.database.clone(),
                    p.user.clone(),
                    p.ssl_mode,
                  ),
                  _ => (
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    SslMode::Prefer,
                  ),
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
                  match profile.credential {
                    CredentialSource::Prompt => 1,
                    _ => 0,
                  },
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
              ),
            };
            view
              .form_name
              .update(cx, |i, cx| i.set_value(name, window, cx));
            view
              .form_group
              .update(cx, |i, cx| i.set_value(group, window, cx));
            view
              .form_host
              .update(cx, |i, cx| i.set_value(host, window, cx));
            view
              .form_port
              .update(cx, |i, cx| i.set_value(port, window, cx));
            view
              .form_database
              .update(cx, |i, cx| i.set_value(database, window, cx));
            view
              .form_user
              .update(cx, |i, cx| i.set_value(user, window, cx));
            view
              .form_password
              .update(cx, |i, cx| i.set_value("", window, cx));
            view.form_env.update(cx, |s, cx| {
              s.set_selected_index(Some(IndexPath::new(env_ix)), window, cx)
            });
            view.form_ssl.update(cx, |s, cx| {
              s.set_selected_index(Some(IndexPath::new(ssl_ix)), window, cx)
            });
            view.form_credential.update(cx, |s, cx| {
              s.set_selected_index(Some(IndexPath::new(cred_ix)), window, cx)
            });
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
                      .label("Password")
                      .child(Select::new(&view.form_credential)),
                  )
                  .child(field().label("").child(Input::new(&view.form_password)))
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
    let credential = match self
      .form_credential
      .read(cx)
      .selected_value()
      .map(String::as_str)
    {
      Some("Ask every time") => CredentialSource::Prompt,
      _ => CredentialSource::Keychain,
    };
    let password = self.form_password.read(cx).value().to_string();
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
        tunnel_id: None,
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
        this.status = match result {
          Ok(Ok(())) => "connection ok".into(),
          Ok(Err(error)) => format!("error: {error}").into(),
          Err(_) => "error: test canceled".into(),
        };
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

impl Render for ConnectionsView {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    // Grouped, ungrouped first, both sides alphabetical like the webview.
    let mut groups: Vec<(Option<String>, Vec<ConnectionProfile>)> = Vec::new();
    let mut sorted = self.profiles.clone();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for profile in sorted {
      let key = profile.group.clone();
      match groups.iter_mut().find(|(group, _)| *group == key) {
        Some((_, list)) => list.push(profile),
        None => groups.push((key, vec![profile])),
      }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));

    let connecting = self.connecting.clone();

    v_flex()
      .size_full()
      .bg(cx.theme().background)
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
            Button::new("new-connection")
              .primary()
              .small()
              .label("New connection")
              .on_click(cx.listener(|this, _, _, cx| this.open_form(None, cx))),
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
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("No connections yet. Add your first database."),
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
              let delete_id = profile.id.clone();
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
                      .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_form(Some(edit_profile.clone()), cx);
                      })),
                  )
                  .child(
                    Button::new(SharedString::from(format!("delete-{}", profile.id)))
                      .ghost()
                      .xsmall()
                      .label("Delete")
                      .on_click(cx.listener(move |this, _, _, cx| {
                        this.delete(delete_id.clone(), cx);
                      })),
                  )
                  .into_any_element(),
              );
            }
            rows
          })),
      )
  }
}
