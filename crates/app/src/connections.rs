use std::path::PathBuf;
use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::form::{Field, field, v_form};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::notification::Notification;
use gpui_component::radio::{Radio, RadioGroup};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::switch::Switch;
use gpui_component::{
  ActiveTheme, Disableable, IndexPath, Sizable, StyledExt, WindowExt, h_flex, v_flex,
};
use soquel_core::AppState;
use soquel_core::error::{Error, SecretSubject};
use soquel_core::profiles::{
  AgentAccess, ConnectionInput, ConnectionProfile, ConnectorKind, ConnectorParams,
  CredentialSource, Env, MongoParams, RedisParams, SqlServerParams, SslMode,
};
use soquel_core::transfer::{DuplicateStrategy, ImportPreview};

use crate::command_approval::{self, CommandApprovalPrompt};
use crate::core::{self, Db};
use crate::dialogs;
use crate::host_key::{self, HostKeyPrompt};
use crate::icons::SoquelIcon;
use crate::transfer::{self, EntryKind};
use crate::tunnels::{
  CredentialMode, TunnelsEvent, TunnelsView, available_credential_modes, command_preview,
  credential_mode_hint, credential_mode_label, default_credential_mode,
};

#[allow(clippy::large_enum_variant)]
pub enum ConnectionsEvent {
  Connected { db: Db, profile: ConnectionProfile },
  OpenMcpPanel,
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

/// The engine picker's kinds; `sqlite` has no port and no protocol.
const KINDS: [ConnectorKind; 5] = [
  ConnectorKind::Postgres,
  ConnectorKind::Mysql,
  ConnectorKind::Sqlite,
  ConnectorKind::Redis,
  ConnectorKind::Mongo,
];

fn kind_default_port(kind: ConnectorKind) -> u16 {
  match kind {
    ConnectorKind::Postgres => 5432,
    ConnectorKind::Mysql => 3306,
    ConnectorKind::Sqlite => 0,
    ConnectorKind::Redis => 6379,
    ConnectorKind::Mongo => 27017,
  }
}

fn kind_short(kind: ConnectorKind) -> &'static str {
  match kind {
    ConnectorKind::Postgres => "PG",
    ConnectorKind::Mysql => "MySQL",
    ConnectorKind::Sqlite => "SQLite",
    ConnectorKind::Redis => "Redis",
    ConnectorKind::Mongo => "Mongo",
  }
}

/// URL schemes that prefill each kind; `mongodb+srv` needs DNS discovery the
/// single-node connector does not do yet.
fn kind_protocols(kind: ConnectorKind) -> &'static [&'static str] {
  match kind {
    ConnectorKind::Postgres => &["postgres", "postgresql"],
    ConnectorKind::Mysql => &["mysql"],
    ConnectorKind::Sqlite => &[],
    ConnectorKind::Redis => &["redis", "rediss"],
    ConnectorKind::Mongo => &["mongodb"],
  }
}

/// The engine select's entries: MariaDB is a display entry riding the mysql kind
/// (wire-compatible, quirks handled at runtime via the version).
struct EngineChoice {
  id: &'static str,
  label: &'static str,
  kind: ConnectorKind,
}

const ENGINE_CHOICES: [EngineChoice; 6] = [
  EngineChoice {
    id: "postgres",
    label: "PostgreSQL",
    kind: ConnectorKind::Postgres,
  },
  EngineChoice {
    id: "mysql",
    label: "MySQL",
    kind: ConnectorKind::Mysql,
  },
  EngineChoice {
    id: "mariadb",
    label: "MariaDB",
    kind: ConnectorKind::Mysql,
  },
  EngineChoice {
    id: "sqlite",
    label: "SQLite",
    kind: ConnectorKind::Sqlite,
  },
  EngineChoice {
    id: "redis",
    label: "Redis",
    kind: ConnectorKind::Redis,
  },
  EngineChoice {
    id: "mongo",
    label: "MongoDB",
    kind: ConnectorKind::Mongo,
  },
];

/// A profile edits back to its kind's first engine entry; MariaDB is only ever
/// chosen by hand.
fn engine_choice_for_kind(kind: ConnectorKind) -> &'static str {
  ENGINE_CHOICES
    .iter()
    .find(|choice| choice.kind == kind)
    .map_or("postgres", |choice| choice.id)
}

/// Only some connectors hold the credential for the connection's life, so a
/// token that expires mid-session is not replayed; None where the pool re-resolves.
fn credential_command_caveat(kind: ConnectorKind) -> Option<&'static str> {
  match kind {
    ConnectorKind::Redis => {
      Some("Read once at connect and never refreshed: an expired token needs a manual reconnect.")
    }
    ConnectorKind::Mongo => Some(
      "Read once at connect and never refreshed: once it expires new pooled connections fail, \
       and you have to reconnect.",
    ),
    _ => None,
  }
}

/// Follow the kind only when the port still sits on the previous kind's default;
/// a hand-set port survives an engine switch. SQLite has no port, so switching
/// away always lands on the next kind's default.
fn port_for_kind_change(port: &str, previous: ConnectorKind, next: ConnectorKind) -> String {
  if next == ConnectorKind::Sqlite {
    return port.to_string();
  }
  let follows = previous == ConnectorKind::Sqlite
    || port.trim().parse::<u16>().ok() == Some(kind_default_port(previous));
  if follows {
    kind_default_port(next).to_string()
  } else {
    port.to_string()
  }
}

/// Workspace badge for a live server: MariaDB and Valkey announce themselves
/// through the mysql / redis version strings.
pub fn server_badge(kind: ConnectorKind, version: &str) -> (String, String) {
  if kind == ConnectorKind::Mysql && version.contains("MariaDB") {
    return ("MariaDB".to_string(), first_segment(version, '-'));
  }
  if kind == ConnectorKind::Redis && version.contains("valkey") {
    return ("Valkey".to_string(), first_segment(version, '-'));
  }
  (kind_short(kind).to_string(), first_segment(version, ' '))
}

fn first_segment(value: &str, sep: char) -> String {
  value.split(sep).next().unwrap_or(value).to_string()
}

fn dsn(params: &ConnectorParams) -> String {
  match params {
    ConnectorParams::Sqlite { path } => format!("sqlite://{path}"),
    ConnectorParams::Redis(p) => {
      let scheme = if p.tls { "rediss" } else { "redis" };
      format!("{scheme}://{}:{}/{}", p.host, p.port, p.db)
    }
    ConnectorParams::Mongo(p) => {
      let auth = p
        .username
        .as_ref()
        .map(|user| format!("{user}@"))
        .unwrap_or_default();
      let db = p
        .database
        .as_ref()
        .map(|db| format!("/{db}"))
        .unwrap_or_default();
      format!("mongodb://{auth}{}:{}{db}", p.host, p.port)
    }
    ConnectorParams::Postgres(p) | ConnectorParams::Mysql(p) => {
      let scheme = match params.kind() {
        ConnectorKind::Mysql => "mysql",
        _ => "postgres",
      };
      format!("{scheme}://{}@{}:{}/{}", p.user, p.host, p.port, p.database)
    }
  }
}

/// libpq's `sslmode` mapped onto the app's coarser set.
fn url_ssl_mode(value: &str) -> Option<SslMode> {
  match value {
    "disable" => Some(SslMode::Disable),
    "allow" | "prefer" => Some(SslMode::Prefer),
    "require" => Some(SslMode::Require),
    "verify-ca" | "verify-full" => Some(SslMode::VerifyFull),
    _ => None,
  }
}

/// mysql's `ssl-mode` vocabulary (case-insensitive) mapped onto the app's set.
fn mysql_url_ssl_mode(value: &str) -> Option<SslMode> {
  match value.to_ascii_uppercase().as_str() {
    "DISABLED" => Some(SslMode::Disable),
    "PREFERRED" => Some(SslMode::Prefer),
    "REQUIRED" => Some(SslMode::Require),
    "VERIFY_CA" | "VERIFY_IDENTITY" => Some(SslMode::VerifyFull),
    _ => None,
  }
}

/// A form field a validation error anchors to, rendered under its input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FormField {
  Name,
  Host,
  Port,
  Database,
  User,
  Path,
  Command,
}

type FormErrors = Vec<(FormField, SharedString)>;

/// What a pasted connection URL prefills; fields the kind does not use stay at
/// their defaults and are ignored by `form_input`.
#[derive(Debug, Clone, PartialEq)]
struct ParsedUrl {
  kind: ConnectorKind,
  host: String,
  port: u16,
  database: String,
  user: String,
  password: String,
  db_index: u32,
  tls: bool,
  ssl_mode: Option<SslMode>,
  ssl_root_cert: Option<String>,
  auth_source: Option<String>,
}

fn decode(value: &str) -> String {
  percent_encoding::percent_decode_str(value)
    .decode_utf8_lossy()
    .into_owned()
}

/// Prefill from a postgres:// / mysql:// / redis(s):// / mongodb:// URL; None
/// when it does not parse or the scheme is not one we know.
fn parse_connection_url(raw: &str) -> Option<ParsedUrl> {
  let url = url::Url::parse(raw.trim()).ok()?;
  let scheme = url.scheme();
  let kind = KINDS
    .into_iter()
    .find(|kind| kind_protocols(*kind).contains(&scheme))?;
  let host = url
    .host_str()
    .filter(|host| !host.is_empty())
    .unwrap_or("localhost")
    .to_string();
  let port = url.port().unwrap_or_else(|| kind_default_port(kind));
  let path = url.path().trim_start_matches('/');
  let mut parsed = ParsedUrl {
    kind,
    host,
    port,
    database: decode(path),
    user: decode(url.username()),
    password: url.password().map(decode).unwrap_or_default(),
    db_index: 0,
    tls: false,
    ssl_mode: None,
    ssl_root_cert: None,
    auth_source: None,
  };
  if kind == ConnectorKind::Redis {
    // The path is a numeric db index, not a database name.
    parsed.database = String::new();
    parsed.db_index = path.parse().unwrap_or(0);
    parsed.tls = scheme == "rediss";
  }
  for (key, value) in url.query_pairs() {
    match key.as_ref() {
      "authSource" if kind == ConnectorKind::Mongo => parsed.auth_source = Some(value.into_owned()),
      "tls" | "ssl" if kind == ConnectorKind::Mongo => parsed.tls = value == "true",
      "sslmode" => parsed.ssl_mode = url_ssl_mode(&value).or(parsed.ssl_mode),
      "ssl-mode" => parsed.ssl_mode = mysql_url_ssl_mode(&value).or(parsed.ssl_mode),
      "sslrootcert" => parsed.ssl_root_cert = Some(value.into_owned()),
      _ => {}
    }
  }
  Some(parsed)
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
  // Option orders None before Some: ungrouped connections list first.
  sections.sort_by(|a, b| a.0.cmp(&b.0));
  sections
}

pub struct ConnectionsView {
  state: Arc<AppState>,
  profiles: Vec<ConnectionProfile>,
  connecting: Option<String>,
  status: SharedString,
  editing: Option<String>,
  /// Validation errors keyed by field, shown under the inputs in the dialog.
  form_errors: FormErrors,
  /// Test feedback shown inside the dialog, never on the page behind it.
  form_status: SharedString,
  form_name: Entity<InputState>,
  form_group: Entity<InputState>,
  form_host: Entity<InputState>,
  form_port: Entity<InputState>,
  form_database: Entity<InputState>,
  form_user: Entity<InputState>,
  form_password: Entity<InputState>,
  form_command: Entity<InputState>,
  form_path: Entity<InputState>,
  form_db_index: Entity<InputState>,
  form_auth_source: Entity<InputState>,
  form_ssl_root_cert: Entity<InputState>,
  form_url: Entity<InputState>,
  form_tls: bool,
  /// The engine select's source of truth for the previous kind, so a port on the
  /// old default follows the switch (see `port_for_kind_change`).
  form_kind: ConnectorKind,
  /// Probed once at load; false hides keychain from the credential picker.
  keychain_available: bool,
  form_engine: Entity<SelectState<Vec<String>>>,
  form_env: Entity<SelectState<Vec<String>>>,
  form_agent_access: Entity<SelectState<Vec<String>>>,
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
    let mut profiles = core::list_connections(&state);
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
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
    let form_path = text(cx, "/path/to/database.db");
    let form_db_index = text(cx, "0");
    let form_auth_source = text(cx, "admin (optional)");
    let form_ssl_root_cert = text(cx, "CA bundle path (optional)");
    let form_url = text(cx, "paste a connection URL to prefill");
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
    let form_engine = cx.new(|cx| {
      SelectState::new(
        ENGINE_CHOICES
          .iter()
          .map(|choice| choice.label.to_string())
          .collect::<Vec<_>>(),
        Some(IndexPath::default()),
        window,
        cx,
      )
    });
    // Switching engine follows the port and swaps the kind-specific fields.
    cx.subscribe_in(
      &form_engine,
      window,
      |this, _, event: &SelectEvent<Vec<String>>, window, cx| {
        if let SelectEvent::Confirm(Some(label)) = event {
          this.on_engine_change(label.clone(), window, cx);
        }
      },
    )
    .detach();
    // Enter on a pasted URL prefills and clears the field.
    cx.subscribe_in(
      &form_url,
      window,
      |this, _, event: &InputEvent, window, cx| {
        if matches!(event, InputEvent::PressEnter { .. }) {
          this.apply_url(window, cx);
        }
      },
    )
    .detach();
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
    let form_agent_access = cx.new(|cx| {
      SelectState::new(
        crate::mcp::AGENT_ACCESSES
          .iter()
          .map(|a| crate::mcp::agent_access_label(*a).to_string())
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
    // A keyring-less session drops keychain from the picker (probed once at load).
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
    let form_tunnel = cx.new(|cx| {
      SelectState::new(
        vec!["none".to_string()],
        Some(IndexPath::default()),
        window,
        cx,
      )
    });
    let tunnels_section = cx.new(|cx| TunnelsView::new(state.clone(), window, cx));
    // A tunnel saved or deleted while the connection form is open refreshes
    // the form's picker, keeping the current selection when it survives.
    cx.subscribe_in(
      &tunnels_section,
      window,
      |this, _, _: &TunnelsEvent, window, cx| {
        let current = this.selected_tunnel_id(cx);
        this.refresh_tunnel_picker(current.as_deref(), window, cx);
      },
    )
    .detach();

    Self {
      state,
      profiles,
      connecting: None,
      status: SharedString::default(),
      editing: None,
      form_errors: Vec::new(),
      form_status: SharedString::default(),
      form_name,
      form_group,
      form_host,
      form_port,
      form_database,
      form_user,
      form_password,
      form_command,
      form_path,
      form_db_index,
      form_auth_source,
      form_ssl_root_cert,
      form_url,
      form_tls: false,
      form_kind: ConnectorKind::Postgres,
      keychain_available,
      form_engine,
      form_env,
      form_agent_access,
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
    self.profiles.sort_by(|a, b| a.name.cmp(&b.name));
    cx.notify();
  }

  pub(crate) fn connect(&mut self, id: String, cx: &mut Context<Self>) {
    if self.connecting.is_some() {
      return;
    }
    self.connecting = Some(id.clone());
    self.status = SharedString::default();
    cx.notify();
    let task = core::connect_id(self.state.clone(), id.clone(), cx);
    self._task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        this.connecting = None;
        match result {
          Ok(db) => {
            if let Ok(profile) = this.state.profiles.lock().unwrap().get(&id) {
              cx.emit(ConnectionsEvent::Connected { db, profile });
            }
          }
          Err(Error::SecretRequired {
            subject,
            target_id,
            target_name,
            ..
          }) => {
            this.open_secret_prompt(subject, target_id, target_name, id.clone(), cx);
          }
          Err(Error::HostKeyUntrusted {
            host,
            port,
            fingerprint,
            key,
            previously_trusted,
            ..
          }) => {
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
                  view.status = crate::status::error(&error);
                  cx.notify();
                }
              },
            );
          }
          // The subject can be the connection's own command or its tunnel's;
          // either way the retry is the same connect.
          Err(Error::CommandApprovalRequired {
            subject,
            target_id,
            target_name,
            program,
            args,
            ..
          }) => {
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
                  view.status = crate::status::error(&error);
                  cx.notify();
                }
              },
            );
          }
          Err(error) => {
            this.status = crate::status::error(&error);
          }
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
    let this = cx.entity().downgrade();
    let input = self.prompt_password.clone();
    dialogs::defer_on_active_window(cx, move |window, cx| {
      input.update(cx, |input, cx| {
        input.set_value("", window, cx);
        input.focus(window, cx);
      });
      let input = input.clone();
      let this = this.clone();
      window.open_dialog(cx, move |dialog, window, cx| {
        let dialog = dialogs::styled(dialog, window, cx);
        let this = this.clone();
        let (subject, target_id, target_name, connect_id) = (
          subject,
          target_id.clone(),
          target_name.clone(),
          connect_id.clone(),
        );
        let Ok(remember) = this.read_with(cx, |view, _| view.prompt_remember) else {
          return dialog;
        };
        // Shared by the Connect button and Enter (the dialog's ConfirmDialog).
        let submit = {
          let this = this.clone();
          let input = input.clone();
          move |_: &mut Window, cx: &mut App| {
            let secret = input.read(cx).value().to_string();
            let (target_id, connect_id) = (target_id.clone(), connect_id.clone());
            this
              .update(cx, |this, cx| {
                core::unlock_secret(
                  &this.state,
                  subject,
                  target_id,
                  secret,
                  this.prompt_remember,
                );
                this.connect(connect_id, cx);
              })
              .ok();
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
                    this
                      .update(cx, |this, cx| {
                        this.prompt_remember = checked;
                        cx.notify();
                      })
                      .ok();
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
  }

  pub(crate) fn open_form(&mut self, editing: Option<ConnectionProfile>, cx: &mut Context<Self>) {
    self.editing = editing.as_ref().map(|p| p.id.clone());
    self.status = SharedString::default();
    self.form_errors.clear();
    self.form_status = SharedString::default();
    let this = cx.entity().downgrade();
    dialogs::defer_on_active_window(cx, move |window, cx| {
      this
        .update(cx, |view, cx| {
          view.prefill_form(editing.as_ref(), window, cx);
        })
        .ok();

      let this = this.clone();
      window.open_dialog(cx, move |dialog, window, cx| {
        let Some(strong) = this.upgrade() else {
          return dialog;
        };
        let view = strong.read(cx);
        let title = if view.editing.is_some() {
          "Edit connection"
        } else {
          "New connection"
        };
        let this_test = this.clone();
        let this_save = this.clone();
        dialogs::styled(dialog, window, cx)
          .title(title)
          .w(px(520.))
          .child(ConnectionForm {
            view: strong.clone(),
          })
          .footer(
            h_flex()
              .gap_2()
              .justify_end()
              .child(
                Button::new("form-test")
                  .ghost()
                  .label("Test")
                  .on_click(move |_, _, cx| {
                    this_test.update(cx, |this, cx| this.test_form(cx)).ok();
                  }),
              )
              .child(
                Button::new("form-cancel")
                  .label("Cancel")
                  .on_click(|_, window, cx| window.close_dialog(cx)),
              )
              .child(Button::new("form-save").primary().label("Save").on_click(
                move |_, window, cx| {
                  let saved = this_save
                    .update(cx, |this, cx| this.save_form(cx))
                    .unwrap_or(false);
                  if saved {
                    window.close_dialog(cx);
                  }
                },
              )),
          )
      });
    });
  }

  fn prefill_form(
    &mut self,
    editing: Option<&ConnectionProfile>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    // Defaults for a new connection; an edit overrides per kind below.
    let mut name = String::new();
    let mut group = String::new();
    let mut host = "localhost".to_string();
    let mut port = kind_default_port(ConnectorKind::Postgres).to_string();
    let mut database = String::new();
    let mut user = String::new();
    let mut path = String::new();
    let mut db_index = "0".to_string();
    let mut auth_source = String::new();
    let mut ssl_root_cert = String::new();
    let mut tls = false;
    let mut kind = ConnectorKind::Postgres;
    let mut env_ix = 0;
    let mut agent_ix = 0;
    let mut ssl_ix = 1;
    // A new profile opens on keychain, or prompt when the keyring is unavailable.
    let mut cred_ix = available_credential_modes(self.keychain_available)
      .iter()
      .position(|m| *m == default_credential_mode(self.keychain_available))
      .unwrap_or(0);
    let mut command = String::new();
    let mut tunnel_id: Option<String> = None;

    if let Some(profile) = editing {
      name = profile.name.clone();
      group = profile.group.clone().unwrap_or_default();
      env_ix = ENVS.iter().position(|e| *e == profile.env).unwrap_or(0);
      agent_ix = crate::mcp::AGENT_ACCESSES
        .iter()
        .position(|a| *a == profile.agent_access)
        .unwrap_or(0);
      let (mode, cmd) = match &profile.credential {
        CredentialSource::Keychain => (CredentialMode::Keychain, String::new()),
        CredentialSource::Prompt => (CredentialMode::Prompt, String::new()),
        CredentialSource::Command { command, .. } => (CredentialMode::Command, command.clone()),
      };
      command = cmd;
      // A stored keychain profile on a keyring-less machine falls back to the
      // first available mode rather than a missing entry.
      cred_ix = available_credential_modes(self.keychain_available)
        .iter()
        .position(|m| *m == mode)
        .unwrap_or(0);
      kind = profile.params.kind();
      match &profile.params {
        ConnectorParams::Postgres(p) | ConnectorParams::Mysql(p) => {
          host = p.host.clone();
          port = p.port.to_string();
          database = p.database.clone();
          user = p.user.clone();
          ssl_ix = SSL_MODES.iter().position(|m| *m == p.ssl_mode).unwrap_or(1);
          ssl_root_cert = p.ssl_root_cert.clone().unwrap_or_default();
          tunnel_id = p.tunnel_id.clone();
        }
        ConnectorParams::Sqlite { path: stored } => path = stored.clone(),
        ConnectorParams::Redis(p) => {
          host = p.host.clone();
          port = p.port.to_string();
          db_index = p.db.to_string();
          user = p.username.clone().unwrap_or_default();
          tls = p.tls;
          tunnel_id = p.tunnel_id.clone();
        }
        ConnectorParams::Mongo(p) => {
          host = p.host.clone();
          port = p.port.to_string();
          database = p.database.clone().unwrap_or_default();
          user = p.username.clone().unwrap_or_default();
          auth_source = p.auth_source.clone().unwrap_or_default();
          tls = p.tls;
          tunnel_id = p.tunnel_id.clone();
        }
      }
    }

    self.form_kind = kind;
    self.form_tls = tls;
    let engine_ix = ENGINE_CHOICES
      .iter()
      .position(|choice| choice.id == engine_choice_for_kind(kind))
      .unwrap_or(0);

    self.refresh_tunnel_picker(tunnel_id.as_deref(), window, cx);
    let mut text = |input: &Entity<InputState>, value: String| {
      input.update(cx, |i, cx| i.set_value(value, window, cx));
    };
    text(&self.form_name, name);
    text(&self.form_group, group);
    text(&self.form_host, host);
    text(&self.form_port, port);
    text(&self.form_database, database);
    text(&self.form_user, user);
    text(&self.form_password, String::new());
    text(&self.form_command, command);
    text(&self.form_path, path);
    text(&self.form_db_index, db_index);
    text(&self.form_auth_source, auth_source);
    text(&self.form_ssl_root_cert, ssl_root_cert);
    text(&self.form_url, String::new());
    let mut select = |state: &Entity<SelectState<Vec<String>>>, ix: usize| {
      state.update(cx, |s, cx| {
        s.set_selected_index(Some(IndexPath::new(ix)), window, cx)
      });
    };
    select(&self.form_engine, engine_ix);
    select(&self.form_env, env_ix);
    select(&self.form_agent_access, agent_ix);
    select(&self.form_ssl, ssl_ix);
    select(&self.form_credential, cred_ix);
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

  /// The kind the engine select points at; MariaDB reads as mysql here (the
  /// distinction is a runtime version, not a form shape).
  fn selected_kind(&self, cx: &App) -> ConnectorKind {
    let label = self.form_engine.read(cx).selected_value().cloned();
    label
      .and_then(|label| ENGINE_CHOICES.iter().find(|choice| choice.label == label))
      .map_or(ConnectorKind::Postgres, |choice| choice.kind)
  }

  fn on_engine_change(&mut self, label: String, window: &mut Window, cx: &mut Context<Self>) {
    let next = ENGINE_CHOICES
      .iter()
      .find(|choice| choice.label == label)
      .map_or(ConnectorKind::Postgres, |choice| choice.kind);
    let port = self.form_port.read(cx).value().to_string();
    let next_port = port_for_kind_change(&port, self.form_kind, next);
    self.form_kind = next;
    if next != ConnectorKind::Sqlite {
      self
        .form_port
        .update(cx, |i, cx| i.set_value(next_port, window, cx));
    }
    cx.notify();
  }

  /// Prefill from a pasted URL, then clear the field so it does not linger.
  fn apply_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let raw = self.form_url.read(cx).value().to_string();
    if raw.trim().is_empty() {
      return;
    }
    let Some(parsed) = parse_connection_url(&raw) else {
      self.form_status =
        "Not a connection URL. Use postgres://, mysql://, redis:// or mongodb://.".into();
      cx.notify();
      return;
    };
    self.form_status = SharedString::default();
    let engine_ix = ENGINE_CHOICES
      .iter()
      .position(|choice| choice.id == engine_choice_for_kind(parsed.kind))
      .unwrap_or(0);
    self.form_engine.update(cx, |s, cx| {
      s.set_selected_index(Some(IndexPath::new(engine_ix)), window, cx)
    });
    self.form_kind = parsed.kind;
    let mut set = |input: &Entity<InputState>, value: String| {
      input.update(cx, |i, cx| i.set_value(value, window, cx));
    };
    set(&self.form_host, parsed.host);
    set(&self.form_port, parsed.port.to_string());
    set(&self.form_database, parsed.database);
    set(&self.form_user, parsed.user);
    if !parsed.password.is_empty() {
      set(&self.form_password, parsed.password);
    }
    set(&self.form_db_index, parsed.db_index.to_string());
    set(
      &self.form_auth_source,
      parsed.auth_source.unwrap_or_default(),
    );
    if let Some(cert) = parsed.ssl_root_cert {
      set(&self.form_ssl_root_cert, cert);
    }
    self.form_tls = parsed.tls;
    if let Some(mode) = parsed.ssl_mode {
      let ssl_ix = SSL_MODES.iter().position(|m| *m == mode).unwrap_or(1);
      self.form_ssl.update(cx, |s, cx| {
        s.set_selected_index(Some(IndexPath::new(ssl_ix)), window, cx)
      });
    }
    self
      .form_url
      .update(cx, |i, cx| i.set_value("", window, cx));
    cx.notify();
  }

  /// Native picker for the sqlite file; typing the path still works, and under
  /// WSLg (no portal) the pick just no-ops.
  fn browse_sqlite_path(&mut self, cx: &mut Context<Self>) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
      files: true,
      directories: false,
      multiple: false,
      prompt: None,
    });
    self._task = cx.spawn(async move |this, cx| {
      let Ok(Ok(Some(paths))) = picked.await else {
        return;
      };
      let Some(path) = paths.into_iter().next() else {
        return;
      };
      let value = path.to_string_lossy().into_owned();
      let Some(handle) = cx.update(|cx| cx.active_window()) else {
        return;
      };
      let _ = cx.update_window(handle, move |_, window, cx| {
        let _ = this.update(cx, |this, cx| {
          this
            .form_path
            .update(cx, |i, cx| i.set_value(value, window, cx));
        });
      });
    });
  }

  fn selected_tunnel_id(&self, cx: &App) -> Option<String> {
    let ix = self
      .form_tunnel
      .read(cx)
      .selected_index(cx)
      .map_or(0, |ix| ix.row);
    self.form_tunnel_ids.get(ix).cloned().flatten()
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

  fn form_input(&self, cx: &Context<Self>) -> Result<ConnectionInput, FormErrors> {
    let mut errors: FormErrors = Vec::new();
    let name = self.form_name.read(cx).value().trim().to_string();
    if name.is_empty() {
      errors.push((FormField::Name, "Name is required".into()));
    }
    let kind = self.selected_kind(cx);
    let group = self.form_group.read(cx).value().trim().to_string();
    let env = self
      .form_env
      .read(cx)
      .selected_value()
      .and_then(|label| ENVS.iter().find(|e| env_label(**e) == label))
      .copied()
      .unwrap_or(Env::Dev);
    let agent_access = self
      .form_agent_access
      .read(cx)
      .selected_value()
      .and_then(|label| {
        crate::mcp::AGENT_ACCESSES
          .iter()
          .find(|a| crate::mcp::agent_access_label(**a) == label)
      })
      .copied()
      .unwrap_or(AgentAccess::None);

    let host = self.form_host.read(cx).value().trim().to_string();
    let port_text = self.form_port.read(cx).value().trim().to_string();
    let parse_port = |errors: &mut FormErrors| match port_text.parse::<u16>() {
      Ok(port) => port,
      Err(_) => {
        errors.push((FormField::Port, "the port is not a number".into()));
        0
      }
    };
    let optional = |input: &Entity<InputState>| {
      let value = input.read(cx).value().trim().to_string();
      (!value.is_empty()).then_some(value)
    };
    let tunnel_id = self.selected_tunnel_id(cx);

    let params = match kind {
      ConnectorKind::Sqlite => {
        let path = self.form_path.read(cx).value().trim().to_string();
        if path.is_empty() {
          errors.push((FormField::Path, "Database file is required".into()));
        }
        ConnectorParams::Sqlite { path }
      }
      ConnectorKind::Redis => {
        if host.is_empty() {
          errors.push((FormField::Host, "Host is required".into()));
        }
        let db = self
          .form_db_index
          .read(cx)
          .value()
          .trim()
          .parse::<u32>()
          .unwrap_or(0);
        ConnectorParams::Redis(RedisParams {
          host,
          port: parse_port(&mut errors),
          db,
          username: optional(&self.form_user),
          tls: self.form_tls,
          tunnel_id,
        })
      }
      ConnectorKind::Mongo => {
        if host.is_empty() {
          errors.push((FormField::Host, "Host is required".into()));
        }
        ConnectorParams::Mongo(MongoParams {
          host,
          port: parse_port(&mut errors),
          database: optional(&self.form_database),
          username: optional(&self.form_user),
          auth_source: optional(&self.form_auth_source),
          tls: self.form_tls,
          tunnel_id,
        })
      }
      ConnectorKind::Postgres | ConnectorKind::Mysql => {
        let database = self.form_database.read(cx).value().trim().to_string();
        let user = self.form_user.read(cx).value().trim().to_string();
        if host.is_empty() {
          errors.push((FormField::Host, "Host is required".into()));
        }
        if database.is_empty() {
          errors.push((FormField::Database, "Database is required".into()));
        }
        if user.is_empty() {
          errors.push((FormField::User, "User is required".into()));
        }
        let ssl_mode = self
          .form_ssl
          .read(cx)
          .selected_value()
          .and_then(|label| SSL_MODES.iter().find(|m| ssl_label(**m) == label))
          .copied()
          .unwrap_or(SslMode::Prefer);
        let params = SqlServerParams {
          host,
          port: parse_port(&mut errors),
          database,
          user,
          ssl_mode,
          // The CA only applies to verify-full; don't persist a stale path.
          ssl_root_cert: (ssl_mode == SslMode::VerifyFull)
            .then(|| optional(&self.form_ssl_root_cert))
            .flatten(),
          tunnel_id,
        };
        match kind {
          ConnectorKind::Mysql => ConnectorParams::Mysql(params),
          _ => ConnectorParams::Postgres(params),
        }
      }
    };

    // SQLite has no auth: it forces keychain (nothing is stored) and no password.
    let (credential, password) = if kind == ConnectorKind::Sqlite {
      (CredentialSource::Keychain, None)
    } else {
      let credential = match self.selected_mode(cx) {
        CredentialMode::Keychain => CredentialSource::Keychain,
        CredentialMode::Prompt => CredentialSource::Prompt,
        CredentialMode::Command => {
          let command = self.form_command.read(cx).value().trim().to_string();
          if command.is_empty() {
            errors.push((FormField::Command, "Command is required".into()));
          }
          CredentialSource::Command {
            command,
            refresh_after_secs: None,
          }
        }
      };
      let password = self.form_password.read(cx).value().to_string();
      (credential, (!password.is_empty()).then_some(password))
    };

    if !errors.is_empty() {
      return Err(errors);
    }

    Ok(ConnectionInput {
      name,
      env,
      group: (!group.is_empty()).then_some(group),
      agent_access,
      credential,
      params,
      password,
    })
  }

  fn test_form(&mut self, cx: &mut Context<Self>) {
    let input = match self.form_input(cx) {
      Ok(input) => {
        self.form_errors.clear();
        input
      }
      Err(errors) => {
        self.form_errors = errors;
        cx.notify();
        return;
      }
    };
    self.form_status = "testing...".into();
    cx.notify();
    let task = core::test_input(self.state.clone(), input, self.editing.clone(), cx);
    self._task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        match result {
          Ok(()) => this.form_status = "connection ok".into(),
          Err(Error::HostKeyUntrusted {
            host,
            port,
            fingerprint,
            key,
            previously_trusted,
            ..
          }) => {
            // The trust dialog owns this failure; retry re-reads the live form.
            this.form_status = SharedString::default();
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
                  view.form_status = crate::status::error(&error);
                  cx.notify();
                }
              },
            );
          }
          Err(error) => this.form_status = crate::status::error(&error),
        }
        cx.notify();
      });
    });
  }

  /// False when validation refused: the dialog stays open on the field errors.
  fn save_form(&mut self, cx: &mut Context<Self>) -> bool {
    let input = match self.form_input(cx) {
      Ok(input) => {
        self.form_errors.clear();
        input
      }
      Err(errors) => {
        self.form_errors = errors;
        cx.notify();
        return false;
      }
    };
    let task = core::save_connection(self.state.clone(), self.editing.clone(), input, cx);
    self._task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        match result {
          Ok(_) => this.refresh(cx),
          Err(error) => this.status = crate::status::error(&error),
        }
        cx.notify();
      });
    });
    true
  }

  fn revoke_command(
    &mut self,
    subject: SecretSubject,
    id: String,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let task = core::revoke_credential_command(self.state.clone(), subject, id, cx);
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

  pub(crate) fn open_export_dialog(&mut self, cx: &mut Context<Self>) {
    self.export_include_secrets = false;
    self.export_busy = false;
    self.export_error = None;
    let this = cx.entity().downgrade();
    dialogs::defer_on_active_window(cx, move |window, cx| {
      this
        .update(cx, |view, cx| {
          view
            .export_passphrase
            .update(cx, |i, cx| i.set_value("", window, cx));
          view
            .export_confirm
            .update(cx, |i, cx| i.set_value("", window, cx));
        })
        .ok();
      let this = this.clone();
      window.open_dialog(cx, move |dialog, window, cx| {
        let dialog = dialogs::styled(dialog, window, cx);
        let Some(strong) = this.upgrade() else {
          return dialog;
        };
        let view = strong.read(cx);
        let busy = view.export_busy;
        let this_run = this.clone();
        dialog
          .title("Export connections")
          .w(px(460.))
          .on_ok(|_, _, _| false)
          .child(ExportForm {
            view: strong.clone(),
          })
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
                    this_run.update(cx, |this, cx| this.run_export(cx)).ok();
                  }),
              ),
          )
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
            this.export_error = Some(crate::status::message(&error));
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
      let task = core::export_connections(state, path, include, include.then_some(passphrase), cx);
      let result = task.await;
      let mut done = None;
      let _ = this.update(cx, |this, cx| {
        this.export_busy = false;
        match result {
          Ok(summary) => done = Some(transfer::export_summary_message(&summary)),
          Err(error) => this.export_error = Some(crate::status::message(&error)),
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
            this.status =
              crate::status::error(&format!("{error}; drop the file on the window instead"));
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
    let this = cx.entity().downgrade();
    dialogs::defer_on_active_window(cx, move |window, cx| {
      this
        .update(cx, |view, cx| {
          view
            .import_passphrase
            .update(cx, |i, cx| i.set_value("", window, cx));
          view.load_import_preview(cx);
        })
        .ok();
      let this = this.clone();
      window.open_dialog(cx, move |dialog, window, cx| {
        import_dialog(dialogs::styled(dialog, window, cx), &this, cx)
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
    let task = core::preview_import(self.state.clone(), path, passphrase, cx);
    self._task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let _ = this.update(cx, |this, cx| {
        this.import_busy = false;
        match result {
          Ok(preview) => {
            this.import_locked = preview.needs_passphrase;
            this.import_preview = Some(preview);
          }
          // The lock stays as it was: a rejected passphrase keeps its field.
          Err(error) => {
            this.import_preview = None;
            this.import_error = Some(crate::status::message(&error));
          }
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
    let task = core::import_connections(
      self.state.clone(),
      path,
      passphrase,
      self.import_with_secrets,
      strategy,
      cx,
    );
    self._task = cx.spawn(async move |this, cx| {
      let result = task.await;
      let mut done = None;
      let _ = this.update(cx, |this, cx| {
        this.import_busy = false;
        match result {
          Ok(outcome) => {
            done = Some(transfer::import_outcome_message(&outcome));
            this.refresh(cx);
            this
              .tunnels_section
              .update(cx, |tunnels, cx| tunnels.refresh(cx));
          }
          Err(error) => this.import_error = Some(crate::status::message(&error)),
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
    let task = core::delete_connection(self.state.clone(), id, cx);
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
/// The form body; the dialog builder keeps only the chrome around it.
#[derive(IntoElement)]
struct ConnectionForm {
  view: Entity<ConnectionsView>,
}

impl RenderOnce for ConnectionForm {
  fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
    let this = self.view.downgrade();
    let view = self.view.read(cx);
    let form_status = view.form_status.clone();
    let mode = view.selected_mode(cx);
    let kind = view.selected_kind(cx);
    let is_server = kind != ConnectorKind::Sqlite;
    let is_sql = matches!(kind, ConnectorKind::Postgres | ConnectorKind::Mysql);
    let ssl_mode = view
      .form_ssl
      .read(cx)
      .selected_value()
      .and_then(|label| SSL_MODES.iter().find(|m| ssl_label(**m) == label))
      .copied()
      .unwrap_or(SslMode::Prefer);
    let tls = view.form_tls;
    let command = view.form_command.read(cx).value().trim().to_string();
    let this_tls = this.clone();
    let this_browse = this.clone();
    let errors = view.form_errors.clone();
    let error_for = move |target: FormField| -> Option<SharedString> {
      errors
        .iter()
        .find(|(field, _)| *field == target)
        .map(|(_, message)| message.clone())
    };
    // The description slot under the input: a validation error wins over the hint.
    let note = |f: Field, error: Option<SharedString>, hint: Option<SharedString>| -> Field {
      match (error, hint) {
        (Some(message), _) => f.description_fn(move |_, cx| {
          div()
            .text_color(cx.theme().danger)
            .child(message.clone())
            .into_any_element()
        }),
        (None, Some(text)) => f.description(text),
        (None, None) => f,
      }
    };
    v_form()
      .columns(2)
      .child(note(
        field().label("Name").child(Input::new(&view.form_name)),
        error_for(FormField::Name),
        None,
      ))
      .child(field().label("Group").child(Input::new(&view.form_group)))
      .child(
        field()
          .label("Engine")
          .col_span(2)
          .child(Select::new(&view.form_engine)),
      )
      .child(
        field()
          .label("From URL")
          .col_span(2)
          .child(Input::new(&view.form_url)),
      )
      .child(field().label("Env").child(Select::new(&view.form_env)))
      .child(
        field()
          .label("Agent access")
          .child(Select::new(&view.form_agent_access)),
      )
      .when(kind == ConnectorKind::Sqlite, |form| {
        let this_browse = this_browse.clone();
        form.child(note(
          field().label("Database file").col_span(2).child(
            h_flex()
              .w_full()
              .gap_2()
              .child(div().flex_1().child(Input::new(&view.form_path)))
              .child(
                Button::new("browse-sqlite")
                  .ghost()
                  .label("Browse")
                  .debug_selector(|| "browse-sqlite".into())
                  .on_click(move |_, _, cx| {
                    this_browse
                      .update(cx, |this, cx| this.browse_sqlite_path(cx))
                      .ok();
                  }),
              ),
          ),
          error_for(FormField::Path),
          None,
        ))
      })
      .when(is_server, |form| {
        form
          .child(note(
            field().label("Host").child(Input::new(&view.form_host)),
            error_for(FormField::Host),
            None,
          ))
          .child(note(
            field().label("Port").child(Input::new(&view.form_port)),
            error_for(FormField::Port),
            None,
          ))
      })
      .when(kind == ConnectorKind::Redis, |form| {
        form
          .child(
            field()
              .label("DB index")
              .child(Input::new(&view.form_db_index)),
          )
          .child(field().label("User").child(Input::new(&view.form_user)))
      })
      .when(kind == ConnectorKind::Mongo, |form| {
        form
          .child(
            field()
              .label("Database")
              .child(Input::new(&view.form_database)),
          )
          .child(
            field()
              .label("Auth source")
              .child(Input::new(&view.form_auth_source)),
          )
          .child(field().label("User").child(Input::new(&view.form_user)))
      })
      .when(is_sql, |form| {
        form
          .child(note(
            field()
              .label("Database")
              .child(Input::new(&view.form_database)),
            error_for(FormField::Database),
            None,
          ))
          .child(note(
            field().label("User").child(Input::new(&view.form_user)),
            error_for(FormField::User),
            None,
          ))
          .child(field().label("SSL").child(Select::new(&view.form_ssl)))
          .when(ssl_mode == SslMode::VerifyFull, |form| {
            form.child(
              field()
                .label("CA cert")
                .child(Input::new(&view.form_ssl_root_cert)),
            )
          })
      })
      .when(
        matches!(kind, ConnectorKind::Redis | ConnectorKind::Mongo),
        |form| {
          form.child(
            field()
              .label("TLS")
              .child(Switch::new("form-tls").checked(tls).on_click({
                let this_tls = this_tls.clone();
                move |checked, _, cx| {
                  let checked = *checked;
                  this_tls
                    .update(cx, |view, cx| {
                      view.form_tls = checked;
                      cx.notify();
                    })
                    .ok();
                }
              })),
          )
        },
      )
      .when(is_server, |form| {
        form.child(
          field()
            .label("SSH tunnel")
            .child(Select::new(&view.form_tunnel)),
        )
      })
      .when(is_server, |form| {
        form
          .child(
            field()
              .label("Password")
              .child(Select::new(&view.form_credential)),
          )
          // Amber, not destructive: one mode is gone, nothing is broken.
          .when_some(view.state.secrets_problem.clone(), |form, problem| {
            form.child(
              field()
                .col_span(2)
                .child(div().text_xs().text_color(cx.theme().yellow).child(problem)),
            )
          })
          .when(mode != CredentialMode::Command, |form| {
            form.child(note(
              field()
                .col_span(2)
                .when(mode == CredentialMode::Prompt, |f| {
                  f.label("Password (used by Test only)")
                })
                .child(Input::new(&view.form_password)),
              None,
              credential_mode_hint(mode).map(SharedString::from),
            ))
          })
          .when(mode == CredentialMode::Command, |form| {
            form
              .child(note(
                field()
                  .label("Command")
                  .col_span(2)
                  .child(Input::new(&view.form_command)),
                error_for(FormField::Command),
                credential_command_caveat(kind).map(SharedString::from),
              ))
              .child(field().col_span(2).child(command_preview(
                &command,
                CONNECTION_COMMAND_HINT,
                cx,
              )))
          })
      })
      .when(!form_status.is_empty(), |form| {
        form.child(
          field().col_span(2).child(
            div()
              .text_xs()
              .text_color(cx.theme().muted_foreground)
              .child(form_status),
          ),
        )
      })
  }
}

/// The export body; the dialog builder keeps only the chrome around it.
#[derive(IntoElement)]
struct ExportForm {
  view: Entity<ConnectionsView>,
}

impl RenderOnce for ExportForm {
  fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
    let this = self.view.downgrade();
    let view = self.view.read(cx);
    let include = view.export_include_secrets;
    let error = view.export_error.clone();
    let this_toggle = this.clone();
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
                  this_toggle
                    .update(cx, |view, cx| {
                      view.export_include_secrets = checked;
                      cx.notify();
                    })
                    .ok();
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
      })
  }
}

/// The import body; `import_dialog` keeps only the chrome around it.
#[derive(IntoElement)]
struct ImportForm {
  view: Entity<ConnectionsView>,
}

impl RenderOnce for ImportForm {
  fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
    let this = self.view.downgrade();
    let view = self.view.read(cx);
    let path_text = view
      .import_path
      .as_ref()
      .map(|p| p.to_string_lossy().to_string())
      .unwrap_or_default();
    let plan = view.import_preview.as_ref().map(transfer::import_plan);
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
                      this_unlock
                        .update(cx, |view, cx| view.load_import_preview(cx))
                        .ok();
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
                            this
                              .update(cx, |view, cx| {
                                view.import_with_secrets = checked;
                                cx.notify();
                              })
                              .ok();
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
                        this
                          .update(cx, |view, cx| {
                            view.import_strategy = ix;
                            cx.notify();
                          })
                          .ok();
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
      })
  }
}

fn import_dialog(
  dialog: gpui_component::dialog::Dialog,
  this: &WeakEntity<ConnectionsView>,
  cx: &App,
) -> gpui_component::dialog::Dialog {
  let Some(strong) = this.upgrade() else {
    return dialog;
  };
  let view = strong.read(cx);
  let plan = view.import_preview.as_ref().map(transfer::import_plan);
  let has_plan = plan.is_some();
  let blocked = plan.as_ref().is_some_and(|p| p.problems > 0);
  let locked = view.import_locked;
  let busy = view.import_busy;
  let this_run = this.clone();
  let this_ok = this.clone();
  dialog
    .title("Import connections")
    .w(px(520.))
    // Enter unlocks while locked; it never runs the import.
    .on_ok(move |_, _, cx| {
      this_ok
        .update(cx, |view, cx| {
          if view.import_locked && !view.import_busy {
            view.load_import_preview(cx);
          }
        })
        .ok();
      false
    })
    .child(ImportForm {
      view: strong.clone(),
    })
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
              this_run.update(cx, |this, cx| this.run_import(cx)).ok();
            }),
        ),
    )
}

impl Render for ConnectionsView {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    // Kept sorted by `refresh`; grouping borrows, nothing clones per frame.
    let groups = group_connections(&self.profiles);

    let connecting = self.connecting.clone();

    v_flex()
      .size_full()
      .bg(crate::theme::canvas(cx))
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
          .child(div().font_semibold().child("Connections"))
          .child(
            h_flex()
              .gap_2()
              .child(
                Button::new("open-mcp")
                  .ghost()
                  .small()
                  .label("Agents…")
                  .debug_selector(|| "open-mcp".into())
                  .on_click(cx.listener(|_, _, _, cx| cx.emit(ConnectionsEvent::OpenMcpPanel))),
              )
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
          .px_4()
          .py_2()
          .gap_2()
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
                  .px_1()
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
                  .bg(crate::theme::panel(cx))
                  .when(!cx.theme().mode.is_dark(), |s| s.shadow_sm())
                  .hover(|s| s.bg(cx.theme().list_hover))
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
                          .child(self.env_badge(profile.env, cx))
                          .when(profile.agent_access != AgentAccess::None, |row| {
                            row.child(
                              div()
                                .px_1p5()
                                .py_0p5()
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().muted)
                                .text_xs()
                                .font_family("IBM Plex Mono")
                                .text_color(cx.theme().muted_foreground)
                                .child("agent"),
                            )
                          }),
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

  fn has_error(errors: &FormErrors, field: FormField, message: &str) -> bool {
    errors
      .iter()
      .any(|(f, m)| *f == field && m.as_ref() == message)
  }

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
    let redis = ConnectorParams::Redis(RedisParams {
      host: "cache".into(),
      port: 6379,
      db: 2,
      username: None,
      tls: true,
      tunnel_id: None,
    });
    assert_eq!(dsn(&redis), "rediss://cache:6379/2");
    let mongo = ConnectorParams::Mongo(MongoParams {
      host: "m".into(),
      port: 27017,
      database: Some("app".into()),
      username: Some("u".into()),
      auth_source: None,
      tls: false,
      tunnel_id: None,
    });
    assert_eq!(dsn(&mongo), "mongodb://u@m:27017/app");
  }

  #[test]
  fn parses_each_connector_url() {
    let pg = parse_connection_url("postgres://alice:s%40cret@db.host:6000/shop?sslmode=require")
      .expect("pg url");
    assert_eq!(pg.kind, ConnectorKind::Postgres);
    assert_eq!(pg.host, "db.host");
    assert_eq!(pg.port, 6000);
    assert_eq!(pg.database, "shop");
    assert_eq!(pg.user, "alice");
    assert_eq!(pg.password, "s@cret", "userinfo is percent-decoded");
    assert_eq!(pg.ssl_mode, Some(SslMode::Require));

    let mysql =
      parse_connection_url("mysql://root@127.0.0.1/app?ssl-mode=VERIFY_IDENTITY").expect("mysql");
    assert_eq!(mysql.kind, ConnectorKind::Mysql);
    assert_eq!(mysql.port, 3306, "no port falls back to the kind default");
    assert_eq!(mysql.ssl_mode, Some(SslMode::VerifyFull));

    let redis = parse_connection_url("rediss://cache:6380/3").expect("redis");
    assert_eq!(redis.kind, ConnectorKind::Redis);
    assert_eq!(redis.db_index, 3);
    assert!(redis.tls, "rediss is tls");
    assert_eq!(redis.database, "");

    let mongo =
      parse_connection_url("mongodb://m:27017/app?authSource=admin&tls=true").expect("mongo");
    assert_eq!(mongo.auth_source.as_deref(), Some("admin"));
    assert!(mongo.tls);

    assert!(parse_connection_url("not a url").is_none());
    assert!(
      parse_connection_url("ftp://x/y").is_none(),
      "unknown scheme is refused"
    );
  }

  #[test]
  fn the_port_follows_the_kind_only_when_still_on_the_default() {
    // On the previous kind's default -> follow to the next default.
    assert_eq!(
      port_for_kind_change("5432", ConnectorKind::Postgres, ConnectorKind::Mysql),
      "3306"
    );
    // Hand-set port survives the switch.
    assert_eq!(
      port_for_kind_change("9999", ConnectorKind::Postgres, ConnectorKind::Mysql),
      "9999"
    );
    // sqlite has no port: switching away lands on the next default.
    assert_eq!(
      port_for_kind_change("", ConnectorKind::Sqlite, ConnectorKind::Redis),
      "6379"
    );
    // Switching to sqlite keeps whatever was there (the field is hidden anyway).
    assert_eq!(
      port_for_kind_change("5432", ConnectorKind::Postgres, ConnectorKind::Sqlite),
      "5432"
    );
  }

  #[test]
  fn the_badge_names_mariadb_and_valkey_by_their_version() {
    assert_eq!(
      server_badge(ConnectorKind::Mysql, "11.4.7-MariaDB-log"),
      ("MariaDB".to_string(), "11.4.7".to_string())
    );
    assert_eq!(
      server_badge(ConnectorKind::Redis, "8.0.1-valkey"),
      ("Valkey".to_string(), "8.0.1".to_string())
    );
    assert_eq!(
      server_badge(ConnectorKind::Postgres, "16.2 (Debian)"),
      ("PG".to_string(), "16.2".to_string())
    );
    assert_eq!(
      server_badge(ConnectorKind::Mysql, "8.0.36"),
      ("MySQL".to_string(), "8.0.36".to_string())
    );
  }

  #[test]
  fn the_engine_entry_for_a_kind_never_lands_on_mariadb() {
    assert_eq!(engine_choice_for_kind(ConnectorKind::Mysql), "mysql");
    assert_eq!(engine_choice_for_kind(ConnectorKind::Mongo), "mongo");
    // MariaDB is a distinct entry but the same kind as mysql.
    let mariadb = ENGINE_CHOICES.iter().find(|c| c.id == "mariadb").unwrap();
    assert_eq!(mariadb.kind, ConnectorKind::Mysql);
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
        assert!(has_error(
          &view.form_input(cx).unwrap_err(),
          FormField::Port,
          "the port is not a number"
        ));

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

  #[gpui::test]
  fn the_form_round_trips_agent_access(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let (_dir, state) = test_state();
    let view = cx.update(|window, cx| cx.new(|cx| ConnectionsView::new(state, window, cx)));
    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        // An empty form refuses on the name first.
        assert!(has_error(
          &view.form_input(cx).unwrap_err(),
          FormField::Name,
          "Name is required"
        ));
        let mut profile = profile("warehouse", None);
        profile.agent_access = AgentAccess::WriteWithApproval;
        view.prefill_form(Some(&profile), window, cx);
        assert_eq!(
          view.form_input(cx).unwrap().agent_access,
          AgentAccess::WriteWithApproval
        );
      });
    });
  }

  #[gpui::test]
  fn the_form_validates_each_kind(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let (_dir, state) = test_state();
    let view = cx.update(|window, cx| cx.new(|cx| ConnectionsView::new(state, window, cx)));

    let set_engine =
      |view: &ConnectionsView, kind: ConnectorKind, window: &mut Window, cx: &mut App| {
        let ix = ENGINE_CHOICES
          .iter()
          .position(|choice| choice.id == engine_choice_for_kind(kind))
          .unwrap();
        view.form_engine.update(cx, |s, cx| {
          s.set_selected_index(Some(IndexPath::new(ix)), window, cx)
        });
      };

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        // Start from the defaults, then a name so it is the kind fields that fail.
        view.prefill_form(None, window, cx);
        view
          .form_name
          .update(cx, |i, cx| i.set_value("conn", window, cx));

        // sqlite: its file is required; host/db/user are not asked.
        set_engine(view, ConnectorKind::Sqlite, window, cx);
        assert!(has_error(
          &view.form_input(cx).unwrap_err(),
          FormField::Path,
          "Database file is required"
        ));

        // redis: only host+port, and host is required.
        set_engine(view, ConnectorKind::Redis, window, cx);
        view
          .form_host
          .update(cx, |i, cx| i.set_value("", window, cx));
        assert!(has_error(
          &view.form_input(cx).unwrap_err(),
          FormField::Host,
          "Host is required"
        ));

        // A server kind refuses a non-numeric port.
        set_engine(view, ConnectorKind::Mysql, window, cx);
        view
          .form_host
          .update(cx, |i, cx| i.set_value("db", window, cx));
        view
          .form_database
          .update(cx, |i, cx| i.set_value("app", window, cx));
        view
          .form_user
          .update(cx, |i, cx| i.set_value("u", window, cx));
        view
          .form_port
          .update(cx, |i, cx| i.set_value("nope", window, cx));
        assert!(has_error(
          &view.form_input(cx).unwrap_err(),
          FormField::Port,
          "the port is not a number"
        ));

        // mysql/postgres need host, database and user, each named on its field.
        view
          .form_port
          .update(cx, |i, cx| i.set_value("3306", window, cx));
        view
          .form_database
          .update(cx, |i, cx| i.set_value("", window, cx));
        assert!(has_error(
          &view.form_input(cx).unwrap_err(),
          FormField::Database,
          "Database is required"
        ));

        // sqlite has no auth: it forces keychain and drops any typed password.
        set_engine(view, ConnectorKind::Sqlite, window, cx);
        view
          .form_path
          .update(cx, |i, cx| i.set_value("/x.db", window, cx));
        view
          .form_password
          .update(cx, |i, cx| i.set_value("ignored", window, cx));
        let input = view.form_input(cx).unwrap();
        assert!(matches!(input.credential, CredentialSource::Keychain));
        assert_eq!(input.password, None);
      });
    });
  }

  #[gpui::test]
  fn no_keyring_hides_keychain_and_defaults_to_prompt(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let dir = tempfile::tempdir().unwrap();
    let mut app_state = soquel_core::AppState::for_tests(
      dir.path(),
      Box::new(soquel_core::secrets::InMemoryStore::default()),
    );
    // The keyring probe failed at load.
    app_state.secrets_problem = Some("keyring unavailable".to_string());
    let state = std::sync::Arc::new(app_state);
    let view = cx.update(|window, cx| cx.new(|cx| ConnectionsView::new(state, window, cx)));

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        assert!(!view.keychain_available);
        view.prefill_form(None, window, cx);
        // A new profile must not open on a mode that cannot store anything.
        assert_eq!(view.selected_mode(cx), CredentialMode::Prompt);
      });
    });
  }

  fn profile_with(params: ConnectorParams) -> ConnectionProfile {
    ConnectionProfile {
      id: "c".to_string(),
      name: "conn".to_string(),
      env: Env::Dev,
      group: None,
      agent_access: AgentAccess::None,
      credential: CredentialSource::Keychain,
      params,
    }
  }

  #[gpui::test]
  fn the_form_round_trips_every_kind(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let (_dir, state) = test_state();
    let view = cx.update(|window, cx| cx.new(|cx| ConnectionsView::new(state, window, cx)));

    let sqlite = ConnectorParams::Sqlite {
      path: "/data/app.db".to_string(),
    };
    let mysql = ConnectorParams::Mysql(SqlServerParams {
      host: "db".to_string(),
      port: 3307,
      database: "shop".to_string(),
      user: "root".to_string(),
      ssl_mode: SslMode::VerifyFull,
      ssl_root_cert: Some("/ca.pem".to_string()),
      tunnel_id: None,
    });
    let redis = ConnectorParams::Redis(RedisParams {
      host: "cache".to_string(),
      port: 6390,
      db: 3,
      username: Some("acl".to_string()),
      tls: true,
      tunnel_id: None,
    });
    let mongo = ConnectorParams::Mongo(MongoParams {
      host: "m".to_string(),
      port: 27018,
      database: Some("app".to_string()),
      username: Some("u".to_string()),
      auth_source: Some("admin".to_string()),
      tls: true,
      tunnel_id: None,
    });

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        for params in [sqlite, mysql, redis, mongo] {
          view.prefill_form(Some(&profile_with(params.clone())), window, cx);
          let got = view.form_input(cx).expect("form is valid").params;
          match (&params, &got) {
            (ConnectorParams::Sqlite { path: want }, ConnectorParams::Sqlite { path }) => {
              assert_eq!(path, want)
            }
            (ConnectorParams::Mysql(want), ConnectorParams::Mysql(p)) => {
              assert_eq!(
                (&p.host, p.port, &p.database, &p.user),
                (&want.host, want.port, &want.database, &want.user)
              );
              assert_eq!(p.ssl_mode, SslMode::VerifyFull);
              assert_eq!(p.ssl_root_cert.as_deref(), Some("/ca.pem"));
            }
            (ConnectorParams::Redis(want), ConnectorParams::Redis(p)) => {
              assert_eq!(
                (&p.host, p.port, p.db, p.tls),
                (&want.host, want.port, want.db, want.tls)
              );
              assert_eq!(p.username.as_deref(), Some("acl"));
            }
            (ConnectorParams::Mongo(want), ConnectorParams::Mongo(p)) => {
              assert_eq!((&p.host, p.port, p.tls), (&want.host, want.port, want.tls));
              assert_eq!(p.database.as_deref(), Some("app"));
              assert_eq!(p.auth_source.as_deref(), Some("admin"));
              assert_eq!(p.username.as_deref(), Some("u"));
            }
            other => panic!("kind mismatch: {other:?}"),
          }
        }
      });
    });
  }

  #[gpui::test]
  fn a_pasted_url_prefills_and_builds(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let cx = cx.add_empty_window();
    let (_dir, state) = test_state();
    let view = cx.update(|window, cx| cx.new(|cx| ConnectionsView::new(state, window, cx)));

    cx.update(|window, cx| {
      view.update(cx, |view, cx| {
        view
          .form_name
          .update(cx, |i, cx| i.set_value("prod", window, cx));
        view.form_url.update(cx, |i, cx| {
          i.set_value(
            "mongodb://u:p%40ss@m.host:27019/app?authSource=admin&tls=true",
            window,
            cx,
          )
        });
        view.apply_url(window, cx);

        assert_eq!(view.selected_kind(cx), ConnectorKind::Mongo);
        let input = view.form_input(cx).expect("valid");
        assert_eq!(input.password.as_deref(), Some("p@ss"), "userinfo decoded");
        let ConnectorParams::Mongo(p) = input.params else {
          panic!("mongo");
        };
        assert_eq!(p.host, "m.host");
        assert_eq!(p.port, 27019);
        assert_eq!(p.database.as_deref(), Some("app"));
        assert_eq!(p.auth_source.as_deref(), Some("admin"));
        assert!(p.tls);
        // The URL field clears itself after applying.
        assert_eq!(view.form_url.read(cx).value(), "");

        // A URL we do not understand says so instead of silently doing nothing.
        view
          .form_url
          .update(cx, |i, cx| i.set_value("ftp://nope", window, cx));
        view.apply_url(window, cx);
        assert!(
          view.form_status.contains("Not a connection URL"),
          "got: {}",
          view.form_status
        );
      });
    });
  }

  #[gpui::test]
  fn browse_fills_the_sqlite_path(cx: &mut TestAppContext) {
    let (_dir, state) = test_state();
    let (view, cx) = crate::test_support::shell_window(cx, {
      let state = state.clone();
      move |window, cx| ConnectionsView::new(state, window, cx)
    });

    cx.update(|_, cx| view.update(cx, |view, cx| view.browse_sqlite_path(cx)));
    cx.run_until_parked();
    assert!(cx.did_prompt_for_paths());
    let file = std::path::PathBuf::from("/data/app.db");
    cx.simulate_path_prompt_response({
      let file = file.clone();
      move |options| {
        assert!(options.files && !options.directories && !options.multiple);
        Some(vec![file.clone()])
      }
    });
    crate::test_support::wait_until(cx, "the picked path", |cx| {
      cx.update(|_, cx| view.read(cx).form_path.read(cx).value() == "/data/app.db")
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
    // The retry dials a real (dead) socket: tokio IO wakes cross-thread.
    cx.executor().allow_parking();
    use gpui_component::WindowExt;

    let (_dir, state) = test_state();
    let line = "echo swordfish";
    let profile = soquel_core::ops::create_connection(&state, &command_input(line)).unwrap();
    // Imported shape: the command sits in the store with no approval.
    soquel_core::ops::revoke_credential_command(
      &state,
      SecretSubject::Connection,
      profile.id.clone(),
    )
    .unwrap();

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
    soquel_core::ops::revoke_credential_command(
      &state,
      SecretSubject::Connection,
      profile.id.clone(),
    )
    .unwrap();

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
    // The retry dials a real (dead) socket: tokio IO wakes cross-thread.
    cx.executor().allow_parking();
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
        let command_ix = available_credential_modes(true)
          .iter()
          .position(|m| *m == CredentialMode::Command)
          .unwrap();
        view.form_credential.update(cx, |s, cx| {
          s.set_selected_index(Some(IndexPath::new(command_ix)), window, cx)
        });

        assert!(has_error(
          &view.form_input(cx).unwrap_err(),
          FormField::Command,
          "Command is required"
        ));

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

  // Iterations shuffle the task order per seed: preview then import must
  // survive any interleaving of the two bridge tasks.
  #[gpui::test(iterations = 10)]
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
    // The retry dials a real (dead) socket: tokio IO wakes cross-thread.
    cx.executor().allow_parking();
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
    soquel_core::ops::revoke_credential_command(&state, SecretSubject::Tunnel, tunnel.id.clone())
      .unwrap();
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
