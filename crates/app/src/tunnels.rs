//! SSH tunnels: the list section, the form dialog, and the form logic.

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::form::{field, v_form};
use gpui_component::input::{Input, InputState};
use gpui_component::notification::Notification;
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::{ActiveTheme, IndexPath, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use soquel_core::AppState;
use soquel_core::error::{Error, SecretSubject};
use soquel_core::profiles::CredentialSource;
use soquel_core::tunnels::{SshAuth, TunnelInput, TunnelProfile};

use crate::core;
use crate::dialogs;
use crate::host_key::{self, HostKeyPrompt};
use crate::icons::SoquelIcon;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthMethod {
  #[default]
  Agent,
  KeyFile,
  Password,
  // `None` would fight Option::None in matches.
  NoAuth,
}

pub const AUTH_METHODS: [AuthMethod; 4] = [
  AuthMethod::Agent,
  AuthMethod::KeyFile,
  AuthMethod::Password,
  AuthMethod::NoAuth,
];

pub fn auth_label(method: AuthMethod) -> &'static str {
  match method {
    AuthMethod::Agent => "SSH agent",
    AuthMethod::KeyFile => "Key file",
    AuthMethod::Password => "Password",
    AuthMethod::NoAuth => "None",
  }
}

pub fn auth_hint(method: AuthMethod) -> Option<&'static str> {
  match method {
    AuthMethod::NoAuth => {
      Some("No credential is sent: the server authorizes the connection on its own.")
    }
    _ => None,
  }
}

/// Methods whose credential lives in the SecretStore.
pub fn needs_secret(method: AuthMethod) -> bool {
  matches!(method, AuthMethod::KeyFile | AuthMethod::Password)
}

fn secret_label(method: AuthMethod) -> &'static str {
  match method {
    AuthMethod::KeyFile => "Key passphrase",
    _ => "Password",
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CredentialMode {
  #[default]
  Keychain,
  Prompt,
  Command,
}

pub const CREDENTIAL_MODES: [CredentialMode; 3] = [
  CredentialMode::Keychain,
  CredentialMode::Prompt,
  CredentialMode::Command,
];

/// The modes a form may offer: a keyring-less session has nothing to save into,
/// so keychain drops out of the picker entirely.
pub fn available_credential_modes(keychain: bool) -> Vec<CredentialMode> {
  CREDENTIAL_MODES
    .into_iter()
    .filter(|mode| keychain || *mode != CredentialMode::Keychain)
    .collect()
}

/// What a new profile opens on: keychain when it works, else ask-every-time. A
/// new profile must not open on a mode that cannot store anything.
pub fn default_credential_mode(keychain: bool) -> CredentialMode {
  if keychain {
    CredentialMode::Keychain
  } else {
    CredentialMode::Prompt
  }
}

pub fn credential_mode_label(mode: CredentialMode) -> &'static str {
  match mode {
    CredentialMode::Keychain => "Saved in the keychain",
    CredentialMode::Prompt => "Ask every time",
    CredentialMode::Command => "From a command",
  }
}

pub(crate) fn credential_mode_hint(mode: CredentialMode) -> Option<&'static str> {
  match mode {
    CredentialMode::Keychain => Some("Stored in the OS keychain and reused on every connection."),
    CredentialMode::Prompt => Some("Nothing is stored: soquel asks when you connect."),
    CredentialMode::Command => None,
  }
}

#[derive(Debug, Clone, Default)]
pub struct TunnelFormValues {
  pub name: String,
  pub host: String,
  /// Bound to a text input: parsed on submit.
  pub port: String,
  pub user: String,
  pub method: AuthMethod,
  pub key_path: String,
  pub secret: String,
  pub credential_mode: CredentialMode,
  pub credential_command: String,
}

/// Validation and mapping in one pass.
pub fn to_tunnel_input(values: &TunnelFormValues) -> Result<TunnelInput, String> {
  let name = values.name.trim().to_string();
  if name.is_empty() {
    return Err("Name is required".to_string());
  }
  let host = values.host.trim().to_string();
  if host.is_empty() {
    return Err("Host is required".to_string());
  }
  let port = match values.port.trim().parse::<u32>() {
    Ok(0) => return Err("Port is required".to_string()),
    Ok(port) if port > 65535 => return Err("Port must be below 65536".to_string()),
    Ok(port) => port as u16,
    Err(_) => return Err("Port must be a whole number".to_string()),
  };
  let user = values.user.trim().to_string();
  if user.is_empty() {
    return Err("User is required".to_string());
  }
  let key_path = values.key_path.trim().to_string();
  if values.method == AuthMethod::KeyFile && key_path.is_empty() {
    return Err("Key path is required".to_string());
  }
  let needs = needs_secret(values.method);
  let command = values.credential_command.trim().to_string();
  if needs && values.credential_mode == CredentialMode::Command && command.is_empty() {
    return Err("Command is required".to_string());
  }
  let auth = match values.method {
    AuthMethod::Agent => SshAuth::Agent,
    AuthMethod::KeyFile => SshAuth::KeyFile { path: key_path },
    AuthMethod::Password => SshAuth::Password,
    AuthMethod::NoAuth => SshAuth::None,
  };
  // An agent or a credential-less server has no secret of ours to source.
  let credential = if needs {
    match values.credential_mode {
      CredentialMode::Keychain => CredentialSource::Keychain,
      CredentialMode::Prompt => CredentialSource::Prompt,
      CredentialMode::Command => CredentialSource::Command {
        command,
        refresh_after_secs: None,
      },
    }
  } else {
    CredentialSource::Keychain
  };
  let secret = if needs {
    values.secret.clone()
  } else {
    String::new()
  };
  Ok(TunnelInput {
    name,
    host,
    port,
    user,
    auth,
    credential,
    secret: (!secret.is_empty()).then_some(secret),
  })
}

/// Flatten a stored tunnel back into the form's editable shape.
pub fn form_values(tunnel: &TunnelProfile) -> TunnelFormValues {
  let (method, key_path) = match &tunnel.auth {
    SshAuth::Agent => (AuthMethod::Agent, String::new()),
    SshAuth::KeyFile { path } => (AuthMethod::KeyFile, path.clone()),
    SshAuth::Password => (AuthMethod::Password, String::new()),
    SshAuth::None => (AuthMethod::NoAuth, String::new()),
  };
  let (credential_mode, credential_command) = match &tunnel.credential {
    CredentialSource::Keychain => (CredentialMode::Keychain, String::new()),
    CredentialSource::Prompt => (CredentialMode::Prompt, String::new()),
    CredentialSource::Command { command, .. } => (CredentialMode::Command, command.clone()),
  };
  TunnelFormValues {
    name: tunnel.name.clone(),
    host: tunnel.host.clone(),
    port: tunnel.port.to_string(),
    user: tunnel.user.clone(),
    method,
    key_path,
    secret: String::new(),
    credential_mode,
    credential_command,
  }
}

fn ssh_dsn(tunnel: &TunnelProfile) -> String {
  format!("ssh://{}@{}:{}", tunnel.user, tunnel.host, tunnel.port)
}

/// Fires on every save, delete or import refresh: the connection form's
/// tunnel picker listens so it never shows a stale list.
pub enum TunnelsEvent {
  Changed,
}

impl EventEmitter<TunnelsEvent> for TunnelsView {}

pub struct TunnelsView {
  state: Arc<AppState>,
  tunnels: Vec<TunnelProfile>,
  editing: Option<String>,
  status: SharedString,
  default_keys: Vec<String>,
  form_name: Entity<InputState>,
  form_host: Entity<InputState>,
  form_port: Entity<InputState>,
  form_user: Entity<InputState>,
  form_auth: Entity<SelectState<Vec<String>>>,
  form_key_path: Entity<InputState>,
  form_secret: Entity<InputState>,
  form_credential: Entity<SelectState<Vec<String>>>,
  form_command: Entity<InputState>,
  /// Probed once at load; false hides keychain from the credential picker.
  keychain_available: bool,
  _subscriptions: Vec<Subscription>,
  _task: Task<()>,
}

impl TunnelsView {
  pub fn new(state: Arc<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
    let tunnels = core::list_tunnels(&state);
    let mut text = |cx: &mut Context<Self>, placeholder: &str| -> Entity<InputState> {
      let placeholder = placeholder.to_string();
      cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
    };
    let form_name = text(cx, "prod bastion");
    let form_host = text(cx, "bastion.internal");
    let form_port = text(cx, "22");
    let form_user = text(cx, "user");
    let form_key_path = text(cx, "~/.ssh/id_ed25519");
    let form_command = text(cx, "vault-ssh-password --host {host} --user {user}");
    let form_secret = cx.new(|cx| InputState::new(window, cx).masked(true));
    let form_auth = cx.new(|cx| {
      SelectState::new(
        AUTH_METHODS
          .iter()
          .map(|m| auth_label(*m).to_string())
          .collect::<Vec<_>>(),
        Some(IndexPath::default()),
        window,
        cx,
      )
    });
    let keychain_available = state.secrets_problem.is_none();
    let form_credential = cx.new(|cx| {
      SelectState::new(
        available_credential_modes(keychain_available)
          .iter()
          .map(|m| credential_mode_label(*m).to_string())
          .collect::<Vec<_>>(),
        Some(IndexPath::default()),
        window,
        cx,
      )
    });

    let subscriptions = vec![
      // Switching to a key file prefills the first discovered key.
      cx.subscribe_in(
        &form_auth,
        window,
        |this: &mut Self, _, _: &SelectEvent<Vec<String>>, window, cx| {
          if this.selected_method(cx) == AuthMethod::KeyFile
            && this.form_key_path.read(cx).value().is_empty()
            && let Some(first) = this.default_keys.first().cloned()
          {
            this
              .form_key_path
              .update(cx, |i, cx| i.set_value(first, window, cx));
          }
          this.update_secret_placeholder(window, cx);
        },
      ),
      cx.subscribe_in(
        &form_credential,
        window,
        |this: &mut Self, _, _: &SelectEvent<Vec<String>>, window, cx| {
          this.update_secret_placeholder(window, cx);
        },
      ),
    ];

    Self {
      state,
      tunnels,
      editing: None,
      status: SharedString::default(),
      default_keys: Vec::new(),
      form_name,
      form_host,
      form_port,
      form_user,
      form_auth,
      form_key_path,
      form_secret,
      form_credential,
      form_command,
      keychain_available,
      _subscriptions: subscriptions,
      _task: Task::ready(()),
    }
  }

  pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
    self.tunnels = core::list_tunnels(&self.state);
    cx.emit(TunnelsEvent::Changed);
    cx.notify();
  }

  fn selected_method(&self, cx: &App) -> AuthMethod {
    let ix = self
      .form_auth
      .read(cx)
      .selected_index(cx)
      .map_or(0, |ix| ix.row);
    AUTH_METHODS.get(ix).copied().unwrap_or_default()
  }

  fn selected_mode(&self, cx: &App) -> CredentialMode {
    let ix = self
      .form_credential
      .read(cx)
      .selected_index(cx)
      .map_or(0, |ix| ix.row);
    available_credential_modes(self.keychain_available)
      .get(ix)
      .copied()
      .unwrap_or(CredentialMode::Prompt)
  }

  pub fn read_values(&self, cx: &App) -> TunnelFormValues {
    TunnelFormValues {
      name: self.form_name.read(cx).value().to_string(),
      host: self.form_host.read(cx).value().to_string(),
      port: self.form_port.read(cx).value().to_string(),
      user: self.form_user.read(cx).value().to_string(),
      method: self.selected_method(cx),
      key_path: self.form_key_path.read(cx).value().to_string(),
      secret: self.form_secret.read(cx).value().to_string(),
      credential_mode: self.selected_mode(cx),
      credential_command: self.form_command.read(cx).value().to_string(),
    }
  }

  fn update_secret_placeholder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let placeholder = if self.selected_mode(cx) == CredentialMode::Prompt {
      "not stored"
    } else if self.editing.is_some() {
      "unchanged"
    } else if self.selected_method(cx) == AuthMethod::KeyFile {
      "empty if none"
    } else {
      ""
    };
    self.form_secret.update(cx, |input, cx| {
      input.set_placeholder(placeholder, window, cx);
    });
  }

  pub fn open_form(&mut self, editing: Option<TunnelProfile>, cx: &mut Context<Self>) {
    self.editing = editing.as_ref().map(|t| t.id.clone());
    self.status = SharedString::default();
    let keys = core::default_ssh_keys(cx);
    cx.spawn(async move |this, cx| {
      let keys = keys.await;
      this
        .update(cx, |this, cx| {
          this.default_keys = keys;
          cx.notify();
        })
        .ok();
    })
    .detach();
    let this = cx.entity().downgrade();
    dialogs::defer_on_active_window(cx, move |window, cx| {
      let _ = this.update(cx, |view, cx| {
        let values = editing
          .as_ref()
          .map(form_values)
          .unwrap_or(TunnelFormValues {
            port: "22".to_string(),
            credential_mode: default_credential_mode(view.keychain_available),
            ..Default::default()
          });
        view
          .form_name
          .update(cx, |i, cx| i.set_value(values.name, window, cx));
        view
          .form_host
          .update(cx, |i, cx| i.set_value(values.host, window, cx));
        view
          .form_port
          .update(cx, |i, cx| i.set_value(values.port, window, cx));
        view
          .form_user
          .update(cx, |i, cx| i.set_value(values.user, window, cx));
        view
          .form_key_path
          .update(cx, |i, cx| i.set_value(values.key_path, window, cx));
        view
          .form_secret
          .update(cx, |i, cx| i.set_value("", window, cx));
        view.form_command.update(cx, |i, cx| {
          i.set_value(values.credential_command, window, cx)
        });
        let method_ix = AUTH_METHODS
          .iter()
          .position(|m| *m == values.method)
          .unwrap_or(0);
        let mode_ix = available_credential_modes(view.keychain_available)
          .iter()
          .position(|m| *m == values.credential_mode)
          .unwrap_or(0);
        view.form_auth.update(cx, |s, cx| {
          s.set_selected_index(Some(IndexPath::new(method_ix)), window, cx)
        });
        view.form_credential.update(cx, |s, cx| {
          s.set_selected_index(Some(IndexPath::new(mode_ix)), window, cx)
        });
        view.update_secret_placeholder(window, cx);
      });

      let this = this.clone();
      window.open_dialog(cx, move |dialog, window, cx| {
        let Some(strong) = this.upgrade() else {
          return dialog;
        };
        let view = strong.read(cx);
        let title = if view.editing.is_some() {
          "Edit tunnel"
        } else {
          "New SSH tunnel"
        };
        let save_label = if view.editing.is_some() {
          "Save changes"
        } else {
          "Create tunnel"
        };
        let this_test = this.clone();
        let this_save = this.clone();
        dialogs::styled(dialog, window, cx)
          .title(title)
          .w(px(460.))
          .child(TunnelForm {
            view: strong.clone(),
          })
          .footer(
            h_flex()
              .gap_2()
              .justify_between()
              .child(
                Button::new("tunnel-test")
                  .outline()
                  .label("Test connection")
                  .on_click(move |_, _, cx| {
                    this_test.update(cx, |this, cx| this.run_test(cx)).ok();
                  }),
              )
              .child(
                h_flex()
                  .gap_2()
                  .child(
                    Button::new("tunnel-cancel")
                      .label("Cancel")
                      .on_click(|_, window, cx| window.close_dialog(cx)),
                  )
                  .child(
                    Button::new("tunnel-save")
                      .primary()
                      .label(save_label)
                      .on_click(move |_, window, cx| {
                        this_save.update(cx, |this, cx| this.save_form(cx)).ok();
                        window.close_dialog(cx);
                      }),
                  ),
              ),
          )
      });
    });
  }

  fn run_test(&mut self, cx: &mut Context<Self>) {
    let input = match to_tunnel_input(&self.read_values(cx)) {
      Ok(input) => input,
      Err(message) => {
        self.status = message.into();
        cx.notify();
        return;
      }
    };
    self.status = "testing...".into();
    cx.notify();
    let task = core::test_tunnel(self.state.clone(), input, self.editing.clone(), cx);
    self._task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        match result {
          Ok(()) => this.status = "Tunnel OK".into(),
          Err(Error::HostKeyUntrusted {
            host,
            port,
            fingerprint,
            key,
            previously_trusted,
            ..
          }) => {
            // The dialog owns this failure; retry re-reads the live form.
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
                Ok(()) => view.run_test(cx),
                Err(error) => {
                  view.status = crate::status::error(&error);
                  cx.notify();
                }
              },
            );
          }
          Err(error) => this.status = crate::status::error(&error),
        }
        cx.notify();
      });
    });
  }

  fn save_form(&mut self, cx: &mut Context<Self>) {
    let input = match to_tunnel_input(&self.read_values(cx)) {
      Ok(input) => input,
      Err(message) => {
        self.status = message.into();
        cx.notify();
        return;
      }
    };
    let task = core::save_tunnel(self.state.clone(), self.editing.clone(), input, cx);
    self._task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        match result {
          Ok(_) => {
            this.status = SharedString::default();
            this.refresh(cx);
          }
          Err(error) => this.status = crate::status::error(&error),
        }
        cx.notify();
      });
    });
  }

  fn revoke_command(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
    let task = core::revoke_credential_command(self.state.clone(), SecretSubject::Tunnel, id, cx);
    let handle = window.window_handle();
    self._task = cx.spawn(async move |this, cx| match task.await {
      Ok(()) => {
        let _ = cx.update_window(handle, |_, window, cx| {
          window.push_notification(
            Notification::info("The command will ask before running again"),
            cx,
          );
        });
      }
      Err(error) => {
        let _ = this.update(cx, |this, cx| {
          this.status = crate::status::error(&error);
          cx.notify();
        });
      }
    });
  }

  fn confirm_delete(&mut self, id: String, cx: &mut Context<Self>) {
    let Some(tunnel) = self.tunnels.iter().find(|t| t.id == id) else {
      return;
    };
    let name = tunnel.name.clone();
    let references = core::list_connections(&self.state)
      .iter()
      .filter(|p| {
        p.params
          .remote()
          .is_some_and(|remote| remote.tunnel_id == Some(id.as_str()))
      })
      .count();
    let this = cx.entity().downgrade();
    dialogs::defer_on_active_window(cx, move |window, cx| {
      let this = this.clone();
      let (id, name) = (id.clone(), name.clone());
      window.open_dialog(cx, move |dialog, window, cx| {
        let this = this.clone();
        let id = id.clone();
        dialogs::styled(dialog, window, cx)
          .title(format!("Delete {name}?"))
          .w(px(400.))
          .child(
            v_flex()
              .gap_1()
              .text_sm()
              .text_color(cx.theme().muted_foreground)
              .child("The tunnel and its stored credential are removed.")
              .when(references > 0, |body| {
                body.child(div().text_color(cx.theme().danger).child(format!(
                  "{references} connection{} reference{} this tunnel and will fail to connect.",
                  if references == 1 { "" } else { "s" },
                  if references == 1 { "s" } else { "" },
                )))
              }),
          )
          .footer(
            h_flex()
              .gap_2()
              .justify_end()
              .child(
                Button::new("delete-tunnel-cancel")
                  .label("Cancel")
                  .on_click(|_, window, cx| window.close_dialog(cx)),
              )
              .child(
                Button::new("delete-tunnel-confirm")
                  .danger()
                  .label("Delete")
                  .debug_selector(|| "delete-tunnel-confirm".into())
                  .on_click(move |_, window, cx| {
                    window.close_dialog(cx);
                    this.update(cx, |this, cx| this.delete(id.clone(), cx)).ok();
                  }),
              ),
          )
      });
    });
  }

  fn delete(&mut self, id: String, cx: &mut Context<Self>) {
    let task = core::delete_tunnel(self.state.clone(), id, cx);
    self._task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        if let Err(error) = result {
          this.status = crate::status::error(&error);
        }
        this.refresh(cx);
      });
    });
  }
}

pub(crate) const TUNNEL_COMMAND_HINT: &str =
  "No shell: {host} {port} {user} are substituted, pipes and $(...) are not supported.";

/// What a non-empty command parses to; the static hint lives in the field's description.
pub(crate) fn command_preview(command: &str, cx: &App) -> Div {
  match soquel_core::credentials::parse_command(command) {
    Ok(spec) => h_flex()
      .flex_wrap()
      .gap_1()
      .items_center()
      .text_xs()
      .font_family("IBM Plex Mono")
      .text_color(cx.theme().muted_foreground)
      .child("runs:")
      .children(std::iter::once(spec.program).chain(spec.args).map(|arg| {
        div()
          .px_1()
          .rounded(cx.theme().radius)
          .bg(cx.theme().muted)
          .child(arg)
      })),
    Err(error) => div()
      .text_xs()
      .text_color(cx.theme().danger)
      .child(format!("{error}")),
  }
}

/// The form body; the dialog builder keeps only the chrome around it.
#[derive(IntoElement)]
struct TunnelForm {
  view: Entity<TunnelsView>,
}

impl RenderOnce for TunnelForm {
  fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
    let this = self.view.downgrade();
    let view = self.view.read(cx);
    let method = view.selected_method(cx);
    let mode = view.selected_mode(cx);
    let needs = needs_secret(method);
    let status = view.status.clone();
    let status_color = if status.starts_with("error") {
      cx.theme().danger
    } else if status == "Tunnel OK" {
      cx.theme().success
    } else {
      cx.theme().muted_foreground
    };
    let command = view.form_command.read(cx).value().trim().to_string();
    let key_chips: Vec<String> = if view.default_keys.len() > 1 {
      view.default_keys.clone()
    } else {
      Vec::new()
    };
    let this_chip = this.clone();
    // Conditional rows use .when(): the pinned rev stores
    // field().visible() but never reads it at render.
    v_form()
      .child(field().label("Name").child(Input::new(&view.form_name)))
      .child(field().label("Host").child(Input::new(&view.form_host)))
      .child(field().label("Port").child(Input::new(&view.form_port)))
      .child(field().label("User").child(Input::new(&view.form_user)))
      .child(
        field()
          .label("Authentication")
          .child(Select::new(&view.form_auth)),
      )
      .when_some(auth_hint(method), |form, hint| {
        form.child(
          field().child(
            div()
              .text_xs()
              .text_color(cx.theme().muted_foreground)
              .child(hint),
          ),
        )
      })
      .when(method == AuthMethod::KeyFile, |form| {
        form
          .child(
            field()
              .label("Key file")
              .child(Input::new(&view.form_key_path)),
          )
          .when(!key_chips.is_empty(), |form| {
            form.child(field().child(h_flex().flex_wrap().gap_1().children(
              key_chips.into_iter().enumerate().map(|(ix, key)| {
                let this_chip = this_chip.clone();
                let value = key.clone();
                Button::new(("key-chip", ix))
                  .ghost()
                  .xsmall()
                  .label(key)
                  .on_click(move |_, window, cx| {
                    let value = value.clone();
                    this_chip
                      .update(cx, |view, cx| {
                        view
                          .form_key_path
                          .update(cx, |i, cx| i.set_value(value, window, cx));
                      })
                      .ok();
                  })
              }),
            )))
          })
          .when(view.default_keys.is_empty(), |form| {
            form.child(
              field().child(
                div()
                  .text_xs()
                  .text_color(cx.theme().muted_foreground)
                  .child(
                    "No key found in ~/.ssh. Generate one with ssh-keygen -t ed25519, \
                               or pick another authentication method.",
                  ),
              ),
            )
          })
      })
      .when(needs, |form| {
        form.child(
          field()
            .label(format!("{} from", secret_label(method)))
            .child(Select::new(&view.form_credential)),
        )
      })
      .when_some(
        needs.then(|| view.state.secrets_problem.clone()).flatten(),
        |form, problem| {
          // Amber, not destructive: one mode is gone, nothing is broken.
          form.child(field().child(div().text_xs().text_color(cx.theme().yellow).child(problem)))
        },
      )
      .when(needs && mode != CredentialMode::Command, |form| {
        form
          .child(
            field()
              .label(secret_label(method))
              .child(Input::new(&view.form_secret)),
          )
          .when_some(credential_mode_hint(mode), |form, hint| {
            form.child(
              field().child(
                div()
                  .text_xs()
                  .text_color(cx.theme().muted_foreground)
                  .child(hint),
              ),
            )
          })
      })
      .when(needs && mode == CredentialMode::Command, |form| {
        form
          .child(
            field()
              .label("Command")
              .description(TUNNEL_COMMAND_HINT)
              .child(Input::new(&view.form_command)),
          )
          .when(!command.is_empty(), |form| {
            form.child(field().child(command_preview(&command, cx)))
          })
      })
      .when(!status.is_empty(), |form| {
        form.child(field().child(div().text_sm().text_color(status_color).child(status)))
      })
  }
}

impl Render for TunnelsView {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let status = self.status.clone();
    v_flex()
      .gap_2()
      .child(
        h_flex()
          .px_1()
          .pt_4()
          .pb_1()
          .justify_between()
          .items_center()
          .child(
            div()
              .text_xs()
              .font_semibold()
              .text_color(cx.theme().muted_foreground)
              .child("ssh tunnels"),
          )
          .child(
            Button::new("new-tunnel")
              .small()
              .label("New tunnel")
              .on_click(cx.listener(|this, _, _, cx| this.open_form(None, cx))),
          ),
      )
      .when(!status.is_empty(), |this| {
        this.child(
          div()
            .px_2()
            .text_sm()
            .text_color(cx.theme().danger)
            .child(status),
        )
      })
      .when(self.tunnels.is_empty(), |this| {
        this.child(
          div()
            .px_2()
            .py_2()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child("No tunnels. Reach databases behind a bastion by referencing a tunnel from a connection."),
        )
      })
      .children(self.tunnels.clone().into_iter().map(|tunnel| {
        let edit_tunnel = tunnel.clone();
        let delete_id = tunnel.id.clone();
        let revoke_id = tunnel.id.clone();
        let selector_id = tunnel.id.clone();
        let has_command = matches!(tunnel.credential, CredentialSource::Command { .. });
        h_flex()
          .id(SharedString::from(format!("tunnel-{}", tunnel.id)))
          .px_3()
          .py_2()
          .gap_3()
          .items_center()
          .rounded(cx.theme().radius)
          .border_1()
          .border_color(cx.theme().border)
          .bg(crate::theme::panel(cx))
          .when(!cx.theme().mode.is_dark(), |s| s.shadow_sm())
          .child(
            div()
              .text_color(cx.theme().muted_foreground)
              .child(SoquelIcon::Cable),
          )
          .child(
            v_flex()
              .flex_1()
              .min_w_0()
              .child(
                h_flex()
                  .gap_2()
                  .items_center()
                  .child(div().font_semibold().text_sm().child(tunnel.name.clone()))
                  .child(
                    div()
                      .px_1p5()
                      .rounded(cx.theme().radius)
                      .bg(cx.theme().muted)
                      .text_xs()
                      .font_family("IBM Plex Mono")
                      .text_color(cx.theme().muted_foreground)
                      .child(auth_label(form_values(&tunnel).method)),
                  ),
              )
              .child(
                div()
                  .text_xs()
                  .font_family("IBM Plex Mono")
                  .text_color(cx.theme().muted_foreground)
                  .child(ssh_dsn(&tunnel)),
              ),
          )
          .child(
            Button::new(SharedString::from(format!("edit-tunnel-{}", tunnel.id)))
              .ghost()
              .xsmall()
              .label("Edit")
              .on_click(cx.listener(move |this, _, _, cx| {
                this.open_form(Some(edit_tunnel.clone()), cx);
              })),
          )
          .when(has_command, |row| {
            row.child(
              Button::new(SharedString::from(format!("revoke-tunnel-{}", tunnel.id)))
                .ghost()
                .xsmall()
                .label("Revoke command")
                .debug_selector(move || format!("revoke-tunnel-{selector_id}"))
                .on_click(cx.listener(move |this, _, window, cx| {
                  this.revoke_command(revoke_id.clone(), window, cx);
                })),
            )
          })
          .child(
            Button::new(SharedString::from(format!("delete-tunnel-{}", tunnel.id)))
              .ghost()
              .xsmall()
              .label("Delete")
              .debug_selector({
                let id = tunnel.id.clone();
                move || format!("delete-tunnel-{id}")
              })
              .on_click(cx.listener(move |this, _, _, cx| {
                this.confirm_delete(delete_id.clone(), cx);
              })),
          )
      }))
  }
}

#[cfg(test)]
mod tests {
  // The parent globs gpui: shadow `test` back or #[gpui::test] recurses.
  use ::core::prelude::v1::test;
  use gpui::TestAppContext;

  use super::*;

  #[test]
  fn a_keyring_less_session_drops_keychain_and_defaults_to_prompt() {
    assert_eq!(
      available_credential_modes(true),
      vec![
        CredentialMode::Keychain,
        CredentialMode::Prompt,
        CredentialMode::Command
      ]
    );
    assert_eq!(
      available_credential_modes(false),
      vec![CredentialMode::Prompt, CredentialMode::Command],
      "keychain drops out when there is no keyring"
    );
    assert_eq!(default_credential_mode(true), CredentialMode::Keychain);
    assert_eq!(default_credential_mode(false), CredentialMode::Prompt);
  }

  #[gpui::test]
  fn no_keyring_drops_keychain_from_the_tunnel_form(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let dir = tempfile::tempdir().unwrap();
    let mut app_state = AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    );
    app_state.secrets_problem = Some("no keyring".to_string());
    let state = Arc::new(app_state);
    let view = cx.update(|window, cx| cx.new(|cx| TunnelsView::new(state, window, cx)));

    cx.update(|_, cx| {
      view.update(cx, |view, cx| {
        assert!(!view.keychain_available);
        // The picker dropped keychain, so its first entry is prompt.
        assert_eq!(view.selected_mode(cx), CredentialMode::Prompt);
      });
    });
  }

  fn valid() -> TunnelFormValues {
    TunnelFormValues {
      name: "bastion".to_string(),
      host: "bastion.internal".to_string(),
      port: "22".to_string(),
      user: "deploy".to_string(),
      method: AuthMethod::Agent,
      key_path: String::new(),
      secret: String::new(),
      credential_mode: CredentialMode::Keychain,
      credential_command: String::new(),
    }
  }

  #[test]
  fn coerces_the_port_and_accepts_agent_auth_without_a_key_path() {
    let input = to_tunnel_input(&valid()).unwrap();
    assert_eq!(input.port, 22);
    assert_eq!(input.auth, SshAuth::Agent);
  }

  #[test]
  fn refuses_ports_that_are_not_numbers_or_out_of_range() {
    for (port, message) in [
      ("abc", "Port must be a whole number"),
      ("0", "Port is required"),
      ("70000", "Port must be below 65536"),
    ] {
      let values = TunnelFormValues {
        port: port.to_string(),
        ..valid()
      };
      assert_eq!(to_tunnel_input(&values).unwrap_err(), message);
    }
  }

  #[test]
  fn requires_a_key_path_for_key_file_auth() {
    let values = TunnelFormValues {
      method: AuthMethod::KeyFile,
      ..valid()
    };
    assert_eq!(
      to_tunnel_input(&values).unwrap_err(),
      "Key path is required"
    );
  }

  #[test]
  fn builds_the_tagged_auth() {
    let agent = to_tunnel_input(&valid()).unwrap();
    assert_eq!(agent.auth, SshAuth::Agent);
    assert_eq!(agent.secret, None);

    let key_file = to_tunnel_input(&TunnelFormValues {
      method: AuthMethod::KeyFile,
      key_path: "~/.ssh/id_ed25519".to_string(),
      secret: "passphrase".to_string(),
      ..valid()
    })
    .unwrap();
    assert_eq!(
      key_file.auth,
      SshAuth::KeyFile {
        path: "~/.ssh/id_ed25519".to_string()
      }
    );
    assert_eq!(key_file.secret.as_deref(), Some("passphrase"));

    let password = to_tunnel_input(&TunnelFormValues {
      method: AuthMethod::Password,
      secret: "pw".to_string(),
      ..valid()
    })
    .unwrap();
    assert_eq!(password.auth, SshAuth::Password);
    assert_eq!(password.secret.as_deref(), Some("pw"));
  }

  #[test]
  fn drops_a_stale_secret_when_the_method_carries_no_credential() {
    let none = to_tunnel_input(&TunnelFormValues {
      method: AuthMethod::NoAuth,
      secret: "leftover".to_string(),
      ..valid()
    })
    .unwrap();
    assert_eq!(none.auth, SshAuth::None);
    assert_eq!(none.secret, None);
  }

  #[test]
  fn maps_the_credential_mode_and_pins_keychain_when_no_secret_is_ours() {
    let password = TunnelFormValues {
      method: AuthMethod::Password,
      secret: "pw".to_string(),
      ..valid()
    };
    assert_eq!(
      to_tunnel_input(&password).unwrap().credential,
      CredentialSource::Keychain
    );
    assert_eq!(
      to_tunnel_input(&TunnelFormValues {
        credential_mode: CredentialMode::Prompt,
        ..password.clone()
      })
      .unwrap()
      .credential,
      CredentialSource::Prompt
    );

    let from_command = TunnelFormValues {
      credential_mode: CredentialMode::Command,
      credential_command: " vault-ssh {host} ".to_string(),
      ..password
    };
    assert_eq!(
      to_tunnel_input(&from_command).unwrap().credential,
      CredentialSource::Command {
        command: "vault-ssh {host}".to_string(),
        refresh_after_secs: None
      }
    );

    // An agent holds the key: a mode left over from another method is dropped.
    assert_eq!(
      to_tunnel_input(&TunnelFormValues {
        method: AuthMethod::Agent,
        ..from_command
      })
      .unwrap()
      .credential,
      CredentialSource::Keychain
    );
  }

  #[test]
  fn requires_a_command_only_when_the_method_has_a_secret_to_source() {
    let missing = TunnelFormValues {
      method: AuthMethod::Password,
      credential_mode: CredentialMode::Command,
      ..valid()
    };
    assert_eq!(
      to_tunnel_input(&missing).unwrap_err(),
      "Command is required"
    );

    let agent = TunnelFormValues {
      credential_mode: CredentialMode::Command,
      ..valid()
    };
    assert!(to_tunnel_input(&agent).is_ok());
  }

  fn stored() -> TunnelProfile {
    TunnelProfile {
      id: "t-1".to_string(),
      name: "bastion".to_string(),
      host: "bastion.internal".to_string(),
      port: 2222,
      user: "deploy".to_string(),
      auth: SshAuth::Password,
      credential: CredentialSource::Keychain,
    }
  }

  #[test]
  fn reads_the_mode_back_and_never_retypes_the_port() {
    let values = form_values(&stored());
    assert_eq!(values.credential_mode, CredentialMode::Keychain);
    assert_eq!(values.credential_command, "");
    assert_eq!(values.port, "2222");
    assert_eq!(values.method, AuthMethod::Password);

    let from_command = TunnelProfile {
      credential: CredentialSource::Command {
        command: "vault-ssh {host}".to_string(),
        refresh_after_secs: None,
      },
      ..stored()
    };
    let values = form_values(&from_command);
    assert_eq!(values.credential_mode, CredentialMode::Command);
    assert_eq!(values.credential_command, "vault-ssh {host}");
  }

  #[test]
  fn never_carries_the_stored_secret_back_into_the_form() {
    assert_eq!(form_values(&stored()).secret, "");
  }

  #[gpui::test]
  fn revoke_shows_only_for_command_tunnels_and_revokes(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let line = "vault-ssh {host}";
    let base = TunnelInput {
      name: "bastion".to_string(),
      host: "bastion.internal".to_string(),
      port: 22,
      user: "deploy".to_string(),
      auth: SshAuth::Password,
      credential: CredentialSource::Command {
        command: line.to_string(),
        refresh_after_secs: None,
      },
      secret: None,
    };
    let with_command = soquel_core::ops::create_tunnel(&state, &base).unwrap();
    let plain = soquel_core::ops::create_tunnel(
      &state,
      &TunnelInput {
        credential: CredentialSource::Prompt,
        ..base
      },
    )
    .unwrap();

    let (_view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| TunnelsView::new(state, window, cx)
    });
    cx.run_until_parked();

    assert!(
      cx.debug_bounds(crate::test_support::selector(format!(
        "revoke-tunnel-{}",
        plain.id
      )))
      .is_none(),
      "no command, no revoke button"
    );
    let bounds = cx
      .debug_bounds(crate::test_support::selector(format!(
        "revoke-tunnel-{}",
        with_command.id
      )))
      .expect("a command tunnel carries the revoke button");
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    let key = soquel_core::secrets::SecretKey::Tunnel(with_command.id.clone());
    assert!(
      !state
        .command_approvals
        .lock()
        .unwrap()
        .is_approved(&key, line)
    );
  }

  #[gpui::test]
  fn deleting_a_tunnel_asks_for_confirmation(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let tunnel = soquel_core::ops::create_tunnel(
      &state,
      &TunnelInput {
        name: "bastion".to_string(),
        host: "bastion.internal".to_string(),
        port: 22,
        user: "deploy".to_string(),
        auth: SshAuth::Agent,
        credential: CredentialSource::Keychain,
        secret: None,
      },
    )
    .unwrap();

    let (_view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| TunnelsView::new(state, window, cx)
    });
    cx.run_until_parked();

    let bounds = cx
      .debug_bounds(crate::test_support::selector(format!(
        "delete-tunnel-{}",
        tunnel.id
      )))
      .expect("the row carries the delete button");
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    // The click only asks; nothing is deleted yet.
    assert!(cx.update(|window, cx| window.has_active_dialog(cx)));
    assert_eq!(core::list_tunnels(&state).len(), 1);

    let confirm = cx
      .debug_bounds("delete-tunnel-confirm")
      .expect("the dialog carries the confirm button");
    cx.simulate_click(confirm.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    assert!(!cx.update(|window, cx| window.has_active_dialog(cx)));
    assert!(core::list_tunnels(&state).is_empty());
  }

  #[gpui::test]
  fn tunnel_form_reads_inputs_and_validates(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let state = std::sync::Arc::new(soquel_core::AppState::for_tests(
      tempfile::tempdir().unwrap().path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    ));
    let view = cx.update(|window, cx| cx.new(|cx| TunnelsView::new(state, window, cx)));
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        // Empty form: the required fields refuse.
        assert!(to_tunnel_input(&view.read_values(cx)).is_err());

        view
          .form_name
          .update(cx, |i, cx| i.set_value("bastion", window, cx));
        view
          .form_host
          .update(cx, |i, cx| i.set_value("bastion.internal", window, cx));
        view
          .form_port
          .update(cx, |i, cx| i.set_value("22", window, cx));
        view
          .form_user
          .update(cx, |i, cx| i.set_value("deploy", window, cx));
        view.form_auth.update(cx, |s, cx| {
          let ix = AUTH_METHODS
            .iter()
            .position(|m| *m == AuthMethod::Password)
            .unwrap();
          s.set_selected_index(Some(IndexPath::new(ix)), window, cx)
        });
        view
          .form_secret
          .update(cx, |i, cx| i.set_value("pw", window, cx));

        let input = to_tunnel_input(&view.read_values(cx)).unwrap();
        assert_eq!(input.auth, SshAuth::Password);
        assert_eq!(input.port, 22);
        assert_eq!(input.secret.as_deref(), Some("pw"));
        assert_eq!(input.credential, CredentialSource::Keychain);
      });
    });
  }
}
