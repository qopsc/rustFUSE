//! The TUI's state and what keys do to it. Anything slow (running
//! `rustfuse`, asking a daemon, starting a service) runs on a thread of
//! its own and hands back an [`Update`] that the main loop applies, so the
//! screen never stalls.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rustfuse_sync::provider::{Item, Kind, ROOT_ID};
use rustfuse_sync::remote::RemoteEntry;
use serde::Deserialize;

use crate::client::{Client, VolumeUsage};
use crate::form::{Field, Form, Outcome};
use crate::library::{self, Connection, Library, Mode, MountBackend, S3Access, VolumeConfig};
use crate::places::Places;
use crate::secrets::{self, Secrets};
use crate::service::Services;

/// A change to the app, made on the main thread when a job finishes.
pub type Update = Box<dyn FnOnce(&mut App) + Send>;

/// How often the volumes' daemons are asked how they are.
const POLL: Duration = Duration::from_secs(2);
/// How long a message stays in the footer.
const MESSAGE_FOR: Duration = Duration::from_secs(8);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Volume(String),
    Connection(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Pane,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Files,
    Bucket,
}

#[derive(Clone, Debug)]
pub enum Loaded<T> {
    Loading,
    Ready(T),
    Failed(String),
}

impl<T> Loaded<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Loaded::Ready(t) => Some(t),
            _ => None,
        }
    }
}

/// `rustfuse conflicts --json`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Conflicts {
    pub files: Vec<String>,
    pub branches: Vec<Branch>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Branch {
    pub repo: String,
    pub branch: String,
}

#[derive(Deserialize)]
struct Listing {
    entries: Vec<RemoteEntry>,
}

/// The Files tab: the volume's tree as its daemon sees it.
pub struct Files {
    pub key: String,
    /// The folders opened from the top: (id, name).
    pub path: Vec<(String, String)>,
    pub items: Loaded<Vec<Item>>,
    pub selected: usize,
}

impl Files {
    pub fn dir_id(&self) -> String {
        self.path
            .last()
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| ROOT_ID.to_string())
    }

    pub fn current(&self) -> Option<&Item> {
        self.items.ready()?.get(self.selected)
    }
}

/// The In the bucket tab: the volume as the bucket holds it.
pub struct Bucket {
    pub key: String,
    /// Folder names from the top.
    pub path: Vec<String>,
    pub entries: Loaded<Vec<RemoteEntry>>,
    pub selected: usize,
}

impl Bucket {
    pub fn dir(&self) -> String {
        self.path.join("/")
    }

    pub fn current(&self) -> Option<&RemoteEntry> {
        self.entries.ready()?.get(self.selected)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormFor {
    Connection(Option<String>),
    AddVolume { connection: String },
    CreateVolume { connection: String },
    CacheSize(String),
    Download { key: String, path: String },
    Settings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    RemoveVolume(String),
    RemoveConnection(String),
}

pub enum Modal {
    Form(FormFor, Form),
    Confirm {
        text: String,
        action: Action,
    },
    Log {
        key: String,
        lines: Vec<String>,
        scroll: usize,
    },
    Help,
}

pub struct Message {
    pub text: String,
    pub error: bool,
    at: Instant,
}

/// How a volume is doing on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Its service runs and the daemon answers.
    Syncing,
    /// Its service is installed but the daemon does not answer (yet).
    Starting,
    /// The daemon answers but no service of the TUI runs it: the Mac app.
    Elsewhere,
    Stopped,
}

pub struct App {
    pub places: Places,
    pub lib: Library,
    pub secrets: Secrets,
    pub services: Services,
    /// This `rustfuse`, run for every command.
    pub exe: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub focus: Focus,
    pub tab: Tab,
    pub alive: HashMap<String, bool>,
    pub installed: HashMap<String, bool>,
    pub usage: HashMap<String, VolumeUsage>,
    pub conflicts: HashMap<String, Loaded<Conflicts>>,
    pub files: Option<Files>,
    pub bucket: Option<Bucket>,
    /// Volume names each connection's token opens.
    pub remote: HashMap<String, Loaded<Vec<String>>>,
    pub remote_selected: usize,
    pub modal: Option<Modal>,
    pub message: Option<Message>,
    /// What runs in the background, shown in the footer.
    pub busy: Vec<String>,
    tx: Sender<Update>,
    rx: Receiver<Update>,
    last_poll: Option<Instant>,
    pub quit: bool,
}

impl App {
    pub fn new(places: Places, secrets: Secrets, services: Services, exe: PathBuf) -> Result<App> {
        let lib = Library::load(&places.library)?;
        let (tx, rx) = channel();
        let mut app = App {
            places,
            lib,
            secrets,
            services,
            exe,
            entries: Vec::new(),
            selected: 0,
            focus: Focus::Sidebar,
            tab: Tab::Overview,
            alive: HashMap::new(),
            installed: HashMap::new(),
            usage: HashMap::new(),
            conflicts: HashMap::new(),
            files: None,
            bucket: None,
            remote: HashMap::new(),
            remote_selected: 0,
            modal: None,
            message: None,
            busy: Vec::new(),
            tx,
            rx,
            last_poll: None,
            quit: false,
        };
        app.rebuild_entries();
        app.selection_changed();
        Ok(app)
    }

    // MARK: Running things

    /// Runs `work` on a thread; `done` applies its result. `label` shows in
    /// the footer meanwhile.
    pub fn spawn<T: Send + 'static>(
        &mut self,
        label: &str,
        work: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut App, T) + Send + 'static,
    ) {
        let label = label.to_string();
        if !label.is_empty() {
            self.busy.push(label.clone());
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let out = work();
            let _ = tx.send(Box::new(move |app: &mut App| {
                if let Some(i) = app.busy.iter().position(|b| *b == label) {
                    app.busy.remove(i);
                }
                done(app, out);
            }));
        });
    }

    /// Applies finished jobs and polls the daemons when it is time.
    pub fn tick(&mut self) {
        while let Ok(update) = self.rx.try_recv() {
            update(self);
        }
        if self.last_poll.is_none_or(|t| t.elapsed() >= POLL) {
            self.last_poll = Some(Instant::now());
            self.poll();
        }
        if self
            .message
            .as_ref()
            .is_some_and(|m| m.at.elapsed() > MESSAGE_FOR)
        {
            self.message = None;
        }
    }

    /// Waits for the jobs running now (tests).
    pub fn settle(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.busy.is_empty() && Instant::now() < deadline {
            if let Ok(update) = self.rx.recv_timeout(Duration::from_millis(50)) {
                update(self);
            }
        }
        while let Ok(update) = self.rx.try_recv() {
            update(self);
        }
    }

    pub fn say(&mut self, text: impl Into<String>) {
        self.message = Some(Message {
            text: text.into(),
            error: false,
            at: Instant::now(),
        });
    }

    pub fn fail(&mut self, text: impl Into<String>) {
        self.message = Some(Message {
            text: text.into(),
            error: true,
            at: Instant::now(),
        });
    }

    /// The environment carrying `c`'s secrets.
    fn env_for(&self, c: &Connection) -> BTreeMap<String, String> {
        library::tool_env(
            c,
            self.secrets.get(secrets::Kind::HubToken, &c.id),
            self.secrets.get(secrets::Kind::S3Secret, &c.id),
        )
    }

    /// A job running `rustfuse args` with `c`'s secrets.
    fn tool_job(
        &self,
        args: Vec<String>,
        c: Option<&Connection>,
    ) -> impl FnOnce() -> Result<String> + Send + 'static {
        let exe = self.exe.clone();
        let env = c.map(|c| self.env_for(c)).unwrap_or_default();
        move || tool(&exe, &args, &env)
    }

    fn poll(&mut self) {
        let keys: Vec<(String, PathBuf)> = self
            .lib
            .volumes
            .iter()
            .map(|v| (v.key.clone(), self.places.socket(&v.key)))
            .collect();
        let services = self.services.clone();
        let selected = self.selected_volume().map(|v| v.key.clone());
        self.spawn(
            "",
            move || {
                keys.into_iter()
                    .map(|(key, socket)| {
                        let client = Client::new(&socket);
                        let alive = client.alive();
                        let usage = if alive && Some(&key) == selected.as_ref() {
                            client.usage().ok()
                        } else {
                            None
                        };
                        let installed = services.installed(&key);
                        (key, alive, installed, usage)
                    })
                    .collect::<Vec<_>>()
            },
            |app, states| {
                for (key, alive, installed, usage) in states {
                    app.alive.insert(key.clone(), alive);
                    app.installed.insert(key.clone(), installed);
                    if let Some(u) = usage {
                        app.usage.insert(key, u);
                    }
                }
            },
        );
    }

    pub fn state(&self, key: &str) -> State {
        let alive = self.alive.get(key).copied().unwrap_or(false);
        let installed = self.installed.get(key).copied().unwrap_or(false);
        match (installed, alive) {
            (true, true) => State::Syncing,
            (true, false) => State::Starting,
            (false, true) => State::Elsewhere,
            (false, false) => State::Stopped,
        }
    }

    // MARK: Selection

    fn rebuild_entries(&mut self) {
        let old = self.entries.get(self.selected).cloned();
        self.entries = self
            .lib
            .volumes
            .iter()
            .map(|v| Entry::Volume(v.key.clone()))
            .chain(
                self.lib
                    .connections
                    .iter()
                    .map(|c| Entry::Connection(c.id.clone())),
            )
            .collect();
        if let Some(old) = old
            && let Some(i) = self.entries.iter().position(|e| *e == old)
        {
            self.selected = i;
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    pub fn entry(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    pub fn selected_volume(&self) -> Option<&VolumeConfig> {
        match self.entry()? {
            Entry::Volume(key) => self.lib.volume(key),
            Entry::Connection(_) => None,
        }
    }

    pub fn selected_connection(&self) -> Option<&Connection> {
        match self.entry()? {
            Entry::Connection(id) => self.lib.connection(id),
            Entry::Volume(_) => None,
        }
    }

    pub fn select(&mut self, entry: &Entry) {
        if let Some(i) = self.entries.iter().position(|e| e == entry) {
            self.selected = i;
            self.selection_changed();
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let n = self.entries.len() as isize;
        let next = (self.selected as isize + delta).clamp(0, n - 1) as usize;
        if next != self.selected {
            self.selected = next;
            self.selection_changed();
        }
    }

    fn selection_changed(&mut self) {
        self.tab = Tab::Overview;
        self.files = None;
        self.bucket = None;
        self.remote_selected = 0;
        self.last_poll = None;
        match self.entry().cloned() {
            Some(Entry::Volume(key)) => self.load_conflicts(&key),
            Some(Entry::Connection(id)) if !self.remote.contains_key(&id) => self.load_remote_volumes(&id),
            _ => {}
        }
    }

    // MARK: Loading

    fn load_conflicts(&mut self, key: &str) {
        let Some(v) = self.lib.volume(key) else { return };
        let dir = library::synced_dir(v, &self.places.data);
        if !dir.join(".rustfuse").exists() {
            self.conflicts
                .insert(key.to_string(), Loaded::Ready(Conflicts::default()));
            return;
        }
        let key = key.to_string();
        let job = self.tool_job(
            vec!["conflicts".into(), dir.to_string_lossy().into(), "--json".into()],
            None,
        );
        self.spawn(
            "",
            move || job().and_then(|out| Ok(serde_json::from_str::<Conflicts>(&out)?)),
            move |app, r| {
                app.conflicts.insert(
                    key,
                    match r {
                        Ok(c) => Loaded::Ready(c),
                        Err(e) => Loaded::Failed(format!("{e:#}")),
                    },
                );
            },
        );
    }

    fn load_remote_volumes(&mut self, id: &str) {
        let Some(c) = self.lib.connection(id).cloned() else {
            return;
        };
        self.remote.insert(id.to_string(), Loaded::Loading);
        let job = self.tool_job(library::volumes_args(&c), Some(&c));
        let id = id.to_string();
        self.spawn(
            &format!("Asking {} for its volumes", c.name),
            move || job().and_then(|out| Ok(serde_json::from_str::<Vec<String>>(&out)?)),
            move |app, r| {
                app.remote.insert(
                    id,
                    match r {
                        Ok(mut names) => {
                            names.sort();
                            Loaded::Ready(names)
                        }
                        Err(e) => Loaded::Failed(format!("{e:#}")),
                    },
                );
            },
        );
    }

    fn open_files(&mut self, key: &str) {
        let files = self.files.get_or_insert_with(|| Files {
            key: key.to_string(),
            path: Vec::new(),
            items: Loaded::Loading,
            selected: 0,
        });
        files.items = Loaded::Loading;
        let dir = files.dir_id();
        let socket = self.places.socket(key);
        let key = key.to_string();
        let asked = dir.clone();
        self.spawn(
            "",
            move || Client::new(&socket).list(&asked).map_err(|e| format!("{e:#}")),
            move |app, r| {
                if let Some(f) = app.files.as_mut().filter(|f| f.key == key && f.dir_id() == dir) {
                    f.items = match r {
                        Ok(items) => Loaded::Ready(sorted_items(items)),
                        Err(e) => Loaded::Failed(e),
                    };
                    let n = f.items.ready().map_or(0, |i| i.len());
                    f.selected = f.selected.min(n.saturating_sub(1));
                }
            },
        );
    }

    fn open_bucket(&mut self, key: &str) {
        let Some(v) = self.lib.volume(key).cloned() else {
            return;
        };
        let Some(c) = self.lib.connection(&v.connection).cloned() else {
            return;
        };
        let bucket = self.bucket.get_or_insert_with(|| Bucket {
            key: key.to_string(),
            path: Vec::new(),
            entries: Loaded::Loading,
            selected: 0,
        });
        bucket.entries = Loaded::Loading;
        let dir = bucket.dir();
        let job = self.tool_job(library::ls_args(&v.name, &dir, &c), Some(&c));
        let key = key.to_string();
        self.spawn(
            "Reading the bucket",
            move || job().and_then(|out| Ok(serde_json::from_str::<Listing>(&out)?.entries)),
            move |app, r| {
                if let Some(b) = app.bucket.as_mut().filter(|b| b.key == key && b.dir() == dir) {
                    b.entries = match r {
                        Ok(mut entries) => {
                            entries.sort_by(|a, b| {
                                (a.kind != Kind::Dir, a.name.to_lowercase())
                                    .cmp(&(b.kind != Kind::Dir, b.name.to_lowercase()))
                            });
                            Loaded::Ready(entries)
                        }
                        Err(e) => Loaded::Failed(format!("{e:#}")),
                    };
                    let n = b.entries.ready().map_or(0, |e| e.len());
                    b.selected = b.selected.min(n.saturating_sub(1));
                }
            },
        );
    }

    fn show_tab(&mut self, tab: Tab) {
        let Some(key) = self.selected_volume().map(|v| v.key.clone()) else {
            return;
        };
        self.tab = tab;
        match tab {
            Tab::Overview => {
                self.focus = Focus::Sidebar;
                self.load_conflicts(&key);
            }
            Tab::Files => {
                self.focus = Focus::Pane;
                if self.files.is_none() {
                    self.open_files(&key);
                }
            }
            Tab::Bucket => {
                self.focus = Focus::Pane;
                if self.bucket.is_none() {
                    self.open_bucket(&key);
                }
            }
        }
    }

    fn refresh(&mut self) {
        self.last_poll = None;
        match self.entry().cloned() {
            Some(Entry::Volume(key)) => match self.tab {
                Tab::Overview => self.load_conflicts(&key),
                Tab::Files => self.open_files(&key),
                Tab::Bucket => self.open_bucket(&key),
            },
            Some(Entry::Connection(id)) => self.load_remote_volumes(&id),
            None => {}
        }
    }

    // MARK: Keys

    pub fn key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.modal.is_some() {
            self.modal_key(key);
            return;
        }
        let in_pane = self.focus == Focus::Pane;
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Char('a') => self.connection_form(None),
            KeyCode::Char('S') => self.settings_form(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Tab => {
                self.focus = if in_pane { Focus::Sidebar } else { Focus::Pane };
            }
            KeyCode::Up | KeyCode::Char('k') if !in_pane => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') if !in_pane => self.move_selection(1),
            KeyCode::Right | KeyCode::Enter if !in_pane => {
                if self.selected_volume().is_some() && self.tab == Tab::Overview {
                    self.show_tab(Tab::Files);
                } else {
                    self.focus = Focus::Pane;
                }
            }
            _ => match self.entry().cloned() {
                Some(Entry::Volume(k)) => self.volume_key(&k, key),
                Some(Entry::Connection(id)) => self.connection_key(&id, key),
                None => {}
            },
        }
    }

    fn volume_key(&mut self, key: &str, k: KeyEvent) {
        let in_pane = self.focus == Focus::Pane;
        match k.code {
            KeyCode::Char('1') => self.show_tab(Tab::Overview),
            KeyCode::Char('2') => self.show_tab(Tab::Files),
            KeyCode::Char('3') => self.show_tab(Tab::Bucket),
            KeyCode::Char('s') => self.start_or_stop(key),
            KeyCode::Char('c') => self.cache_form(key),
            KeyCode::Char('x') => {
                self.modal = Some(Modal::Confirm {
                    text: format!(
                        "Stop syncing {key} and forget it? Its files stay where they are \
                         ({}).",
                        self.lib
                            .volume(key)
                            .map(|v| library::synced_dir(v, &self.places.data).display().to_string())
                            .unwrap_or_default()
                    ),
                    action: Action::RemoveVolume(key.to_string()),
                })
            }
            KeyCode::Char('l') => self.show_log(key),
            KeyCode::Esc | KeyCode::Left if in_pane && self.at_top() => self.focus = Focus::Sidebar,
            _ if in_pane => match self.tab {
                Tab::Files => self.files_key(key, k),
                Tab::Bucket => self.bucket_key(key, k),
                Tab::Overview => {}
            },
            _ => {}
        }
    }

    /// Whether the open browser shows the volume's top folder.
    fn at_top(&self) -> bool {
        match self.tab {
            Tab::Files => self.files.as_ref().is_none_or(|f| f.path.is_empty()),
            Tab::Bucket => self.bucket.as_ref().is_none_or(|b| b.path.is_empty()),
            Tab::Overview => true,
        }
    }

    fn files_key(&mut self, key: &str, k: KeyEvent) {
        let Some(f) = self.files.as_mut() else { return };
        let n = f.items.ready().map_or(0, |i| i.len());
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => f.selected = f.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => f.selected = (f.selected + 1).min(n.saturating_sub(1)),
            KeyCode::PageUp => f.selected = f.selected.saturating_sub(15),
            KeyCode::PageDown => f.selected = (f.selected + 15).min(n.saturating_sub(1)),
            KeyCode::Enter | KeyCode::Right => {
                if let Some(item) = f.current().filter(|i| i.kind == Kind::Dir).cloned() {
                    f.path.push((item.id, item.name));
                    f.selected = 0;
                    self.open_files(key);
                }
            }
            KeyCode::Backspace | KeyCode::Left => {
                if f.path.pop().is_some() {
                    f.selected = 0;
                    self.open_files(key);
                }
            }
            KeyCode::Char('p') => {
                if let Some(item) = f.current().cloned() {
                    let keep = !item.pinned;
                    let what = if keep { "Keeping" } else { "Stopped keeping" };
                    self.item_job(
                        key,
                        &item,
                        &format!("{what} {} downloaded", item.name),
                        move |c, id| c.keep(id, keep),
                    );
                }
            }
            KeyCode::Char('f') => {
                if let Some(item) = f.current().cloned() {
                    self.item_job(key, &item, &format!("Freed {}", item.name), |c, id| c.free(id));
                }
            }
            KeyCode::Char('d') => {
                if let Some(item) = f.current().cloned() {
                    if item.kind == Kind::Dir {
                        self.fail("Press p to keep a whole folder downloaded.");
                    } else {
                        self.item_job(key, &item, &format!("Downloaded {}", item.name), |c, id| {
                            c.fetch(id)
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// Runs `call` on `item` against the volume's daemon, then reloads the
    /// folder.
    fn item_job(
        &mut self,
        key: &str,
        item: &Item,
        done: &str,
        call: impl FnOnce(&Client, &str) -> Result<()> + Send + 'static,
    ) {
        let socket = self.places.socket(key);
        let id = item.id.clone();
        let done = done.to_string();
        let key = key.to_string();
        self.spawn(
            &format!("{}…", item.name),
            move || call(&Client::patient(&socket), &id),
            move |app, r| {
                match r {
                    Ok(()) => app.say(done),
                    Err(e) => app.fail(format!("{e:#}")),
                }
                if app.files.as_ref().is_some_and(|f| f.key == key) {
                    app.open_files(&key);
                }
                app.last_poll = None;
            },
        );
    }

    fn bucket_key(&mut self, key: &str, k: KeyEvent) {
        let Some(b) = self.bucket.as_mut() else { return };
        let n = b.entries.ready().map_or(0, |e| e.len());
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => b.selected = b.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => b.selected = (b.selected + 1).min(n.saturating_sub(1)),
            KeyCode::PageUp => b.selected = b.selected.saturating_sub(15),
            KeyCode::PageDown => b.selected = (b.selected + 15).min(n.saturating_sub(1)),
            KeyCode::Enter | KeyCode::Right => {
                if let Some(e) = b.current().filter(|e| e.kind == Kind::Dir).cloned() {
                    b.path.push(e.name);
                    b.selected = 0;
                    self.open_bucket(key);
                }
            }
            KeyCode::Backspace | KeyCode::Left => {
                if b.path.pop().is_some() {
                    b.selected = 0;
                    self.open_bucket(key);
                }
            }
            KeyCode::Char('g') => {
                if let Some(e) = b.current().cloned() {
                    let path = if b.path.is_empty() {
                        e.name.clone()
                    } else {
                        format!("{}/{}", b.dir(), e.name)
                    };
                    let dest = default_download(&e.name);
                    let form = Form::new(
                        &format!("Download {}", e.name),
                        vec![Field::text("Save to", &dest, "")],
                    )
                    .with_note(
                        "Folders download with everything in them. The destination must not exist yet.",
                    );
                    self.modal = Some(Modal::Form(
                        FormFor::Download {
                            key: key.to_string(),
                            path,
                        },
                        form,
                    ));
                }
            }
            _ => {}
        }
    }

    fn connection_key(&mut self, id: &str, k: KeyEvent) {
        let names = self
            .remote
            .get(id)
            .and_then(|l| l.ready())
            .cloned()
            .unwrap_or_default();
        match k.code {
            KeyCode::Char('e') => self.connection_form(Some(id.to_string())),
            KeyCode::Char('n') => self.create_form(id),
            KeyCode::Char('x') => {
                if self
                    .lib
                    .volumes
                    .iter()
                    .any(|v| v.connection.eq_ignore_ascii_case(id))
                {
                    self.fail("Remove the volumes that use this connection first.");
                } else {
                    let name = self
                        .lib
                        .connection(id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    self.modal = Some(Modal::Confirm {
                        text: format!("Forget the connection {name} and its saved token?"),
                        action: Action::RemoveConnection(id.to_string()),
                    });
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.remote_selected = self.remote_selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.remote_selected = (self.remote_selected + 1).min(names.len().saturating_sub(1))
            }
            KeyCode::Esc | KeyCode::Left => self.focus = Focus::Sidebar,
            KeyCode::Enter | KeyCode::Char('v') if self.focus == Focus::Pane => {
                if let Some(name) = names.get(self.remote_selected) {
                    self.add_form(id, name);
                }
            }
            _ => {}
        }
    }

    fn modal_key(&mut self, k: KeyEvent) {
        let Some(modal) = self.modal.take() else { return };
        match modal {
            Modal::Help => {}
            Modal::Confirm { text, action } => match k.code {
                KeyCode::Char('y') | KeyCode::Enter => self.confirmed(action),
                KeyCode::Char('n') | KeyCode::Esc => {}
                _ => self.modal = Some(Modal::Confirm { text, action }),
            },
            Modal::Log {
                key,
                lines,
                mut scroll,
            } => {
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('l') => return,
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => scroll += 1,
                    KeyCode::PageUp => scroll = scroll.saturating_sub(20),
                    KeyCode::PageDown => scroll += 20,
                    KeyCode::Home | KeyCode::Char('g') => scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => scroll = usize::MAX,
                    KeyCode::Char('r') => {
                        self.show_log(&key);
                        return;
                    }
                    _ => {}
                }
                scroll = scroll.min(lines.len().saturating_sub(1));
                self.modal = Some(Modal::Log { key, lines, scroll });
            }
            Modal::Form(what, mut form) => match form.key(k) {
                Outcome::Cancel => {}
                Outcome::Continue => {
                    self.form_changed(&what, &mut form);
                    self.modal = Some(Modal::Form(what, form));
                }
                Outcome::Submit => {
                    if let Err(e) = self.submit(&what, &form) {
                        form.error = Some(format!("{e:#}"));
                        self.modal = Some(Modal::Form(what, form));
                    }
                }
            },
        }
    }

    // MARK: Forms

    fn connection_form(&mut self, id: Option<String>) {
        let c = id.as_deref().and_then(|id| self.lib.connection(id)).cloned();
        let s3 = c.as_ref().and_then(|c| c.s3.clone()).unwrap_or_default();
        let saved = |kind| {
            id.as_deref()
                .and_then(|id| self.secrets.get(kind, id))
                .map_or("", |_| "saved; type to replace")
        };
        let token_hint = saved(secrets::Kind::HubToken);
        let secret_hint = saved(secrets::Kind::S3Secret);
        let form = Form::new(
            if c.is_some() {
                "Edit connection"
            } else {
                "Add a connection"
            },
            vec![
                Field::text("Name", c.as_ref().map_or("", |c| &c.name), "e.g. Home NAS"),
                Field::text(
                    "Hub",
                    c.as_ref().map_or("", |c| &c.hub),
                    "host:7420 or tls://host:7420",
                ),
                Field::text(
                    "Hub CA file",
                    "",
                    c.as_ref()
                        .and_then(|c| c.ca_file.as_deref())
                        .unwrap_or("only for a private CA"),
                ),
                Field::secret(
                    "Hub token",
                    if token_hint.is_empty() {
                        "from rustfuse key create"
                    } else {
                        token_hint
                    },
                ),
                Field::text("Bucket", &s3.bucket, "optional: read the bucket directly"),
                Field::text(
                    "Endpoint",
                    s3.endpoint.as_deref().unwrap_or(""),
                    "e.g. http://nas.local:9000",
                ),
                Field::text("Region", s3.region.as_deref().unwrap_or(""), "us-east-1"),
                Field::text("Prefix", s3.prefix.as_deref().unwrap_or(""), ""),
                Field::text("Access key", &s3.access_key, ""),
                Field::secret("Secret key", secret_hint),
            ],
        )
        .with_note("The token and secret key are kept in the keychain (Mac) or a file only you can read.");
        self.modal = Some(Modal::Form(FormFor::Connection(id), form));
    }

    fn add_form(&mut self, connection: &str, name: &str) {
        let mode = Mode::Mount;
        let mut form = Form::new(
            &format!("Add {name}"),
            vec![
                Field::text("Volume", name, ""),
                Field::choice("Show as", &["mount", "folder"], lazy_label(mode)),
                Field::text("Path", &default_path(name, mode), ""),
                Field::text(
                    "Cache size",
                    &format_size(self.lib.settings.default_cache_size),
                    "e.g. 20 GB",
                ),
            ],
        )
        .with_note(MOUNT_NOTE);
        form.focus = 1;
        self.modal = Some(Modal::Form(
            FormFor::AddVolume {
                connection: connection.to_string(),
            },
            form,
        ));
    }

    fn create_form(&mut self, connection: &str) {
        let form = Form::new(
            "New volume from a folder",
            vec![
                Field::text("Name", "", "letters, digits, . _ -"),
                Field::text("Folder", "", "e.g. ~/src/proj"),
            ],
        )
        .with_note("The folder is uploaded as a new volume and keeps syncing in place.");
        self.modal = Some(Modal::Form(
            FormFor::CreateVolume {
                connection: connection.to_string(),
            },
            form,
        ));
    }

    fn cache_form(&mut self, key: &str) {
        let Some(v) = self.lib.volume(key) else { return };
        if !v.mode.is_lazy() {
            self.fail("A folder volume keeps everything; it has no cache.");
            return;
        }
        let size = v.cache_size.unwrap_or(self.lib.settings.default_cache_size);
        let form = Form::new(
            &format!("Cache size of {key}"),
            vec![Field::text("Cache size", &format_size(size), "e.g. 20 GB")],
        )
        .with_note("Downloaded files beyond this are freed again, least recently used first.");
        self.modal = Some(Modal::Form(FormFor::CacheSize(key.to_string()), form));
    }

    fn settings_form(&mut self) {
        let s = &self.lib.settings;
        let mut fields = vec![Field::text(
            "Default cache size",
            &format_size(s.default_cache_size),
            "",
        )];
        if cfg!(target_os = "macos") {
            let backend = match s.mount_backend {
                MountBackend::Fskit => "fskit",
                MountBackend::Kernel => "kernel",
            };
            fields.push(Field::choice("Mount with", &["fskit", "kernel"], backend));
        }
        let form = Form::new("Settings", fields).with_note(&format!(
            "Library: {}\nVolumes run as a {}.",
            self.places.library.display(),
            self.services.describe()
        ));
        self.modal = Some(Modal::Form(FormFor::Settings, form));
    }

    /// Follows edits that change other fields.
    fn form_changed(&self, what: &FormFor, form: &mut Form) {
        // Only a change of mode moves the path along.
        if let FormFor::AddVolume { .. } = what
            && form.fields[form.focus].label == "Show as"
        {
            let name = form.get("Volume");
            let mode = mode_from(&form.get("Show as"));
            let path = form.get("Path");
            let other = if mode == Mode::Mount {
                Mode::Folder
            } else {
                Mode::Mount
            };
            if path.is_empty() || path == default_path(&name, other) {
                form.set("Path", &default_path(&name, mode));
            }
            form.note = if mode == Mode::Mount {
                MOUNT_NOTE
            } else {
                FOLDER_NOTE
            }
            .into();
        }
    }

    fn submit(&mut self, what: &FormFor, form: &Form) -> Result<()> {
        match what {
            FormFor::Connection(id) => self.save_connection(id.as_deref(), form),
            FormFor::AddVolume { connection } => self.add_volume(connection, form),
            FormFor::CreateVolume { connection } => self.create_volume(connection, form),
            FormFor::CacheSize(key) => {
                let size = parse_size(&form.get("Cache size"))?;
                let Some(v) = self.lib.volumes.iter_mut().find(|v| v.key == *key) else {
                    return Ok(());
                };
                v.cache_size = Some(size);
                self.save()?;
                self.restart_if_ours(key);
                self.say(format!("{key} keeps up to {} downloaded", format_size(size)));
                Ok(())
            }
            FormFor::Download { key, path } => {
                let dest = expand(&form.get("Save to"));
                if dest.as_os_str().is_empty() {
                    bail!("Choose where to save it.");
                }
                if dest.exists() {
                    bail!("{} already exists.", dest.display());
                }
                let v = self.lib.volume(key).context("no such volume")?;
                let c = self
                    .lib
                    .connection(&v.connection)
                    .context("no connection")?
                    .clone();
                let job = self.tool_job(library::get_args(&v.name, path, &dest, &c), Some(&c));
                self.spawn(&format!("Downloading {path}"), job, move |app, r| match r {
                    Ok(_) => app.say(format!("Saved {}", dest.display())),
                    Err(e) => app.fail(format!("{e:#}")),
                });
                Ok(())
            }
            FormFor::Settings => {
                self.lib.settings.default_cache_size = parse_size(&form.get("Default cache size"))?;
                if form.field("Mount with").is_some() {
                    self.lib.settings.mount_backend = if form.get("Mount with") == "kernel" {
                        MountBackend::Kernel
                    } else {
                        MountBackend::Fskit
                    };
                }
                self.save()?;
                self.say("Settings saved");
                Ok(())
            }
        }
    }

    fn save(&self) -> Result<()> {
        self.lib.save(&self.places.library)
    }

    fn save_connection(&mut self, id: Option<&str>, form: &Form) -> Result<()> {
        let name = form.get("Name");
        let hub = form.get("Hub");
        if name.is_empty() || hub.is_empty() {
            bail!("A connection needs a name and a hub.");
        }
        let mut c = match id.and_then(|id| self.lib.connection(id)) {
            Some(c) => c.clone(),
            None => Connection::new(name.clone(), hub.clone()),
        };
        let before = c.clone();
        c.name = name;
        c.hub = hub;
        let ca = form.get("Hub CA file");
        if !ca.is_empty() {
            let src = expand(&ca);
            let dst = self.places.ca_file(&c.id);
            if src != dst {
                std::fs::create_dir_all(dst.parent().unwrap())?;
                std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
            }
            c.ca_file = Some(dst.to_string_lossy().into());
        }
        let bucket = form.get("Bucket");
        let opt = |s: String| if s.is_empty() { None } else { Some(s) };
        c.s3 = if bucket.is_empty() {
            None
        } else {
            Some(S3Access {
                bucket,
                endpoint: opt(form.get("Endpoint")),
                region: opt(form.get("Region")),
                prefix: opt(form.get("Prefix")),
                access_key: form.get("Access key"),
            })
        };
        let token = form.get("Hub token");
        let secret = form.get("Secret key");
        if !token.is_empty() {
            self.secrets.set(secrets::Kind::HubToken, &c.id, &token)?;
        }
        if c.s3.is_none() {
            self.secrets.set(secrets::Kind::S3Secret, &c.id, "")?;
        } else if !secret.is_empty() {
            self.secrets.set(secrets::Kind::S3Secret, &c.id, &secret)?;
        }
        let changed = c != before || !token.is_empty() || !secret.is_empty();
        match self.lib.connections.iter_mut().find(|x| x.id == c.id) {
            Some(x) => *x = c.clone(),
            None => self.lib.connections.push(c.clone()),
        }
        self.save()?;
        self.rebuild_entries();
        // Volumes pick up a changed hub or bucket on restart.
        if id.is_some() && changed {
            let keys: Vec<String> = self
                .lib
                .volumes
                .iter()
                .filter(|v| v.connection.eq_ignore_ascii_case(&c.id))
                .map(|v| v.key.clone())
                .collect();
            for key in keys {
                self.restart_if_ours(&key);
            }
        }
        self.select(&Entry::Connection(c.id.clone()));
        self.load_remote_volumes(&c.id);
        self.focus = Focus::Pane;
        self.say(format!("Saved {}", c.name));
        Ok(())
    }

    fn add_volume(&mut self, connection: &str, form: &Form) -> Result<()> {
        let c = self
            .lib
            .connection(connection)
            .context("no such connection")?
            .clone();
        let name = form.get("Volume");
        let mode = mode_from(&form.get("Show as"));
        let path = expand(&form.get("Path"));
        if path.as_os_str().is_empty() {
            bail!("Choose where to put it.");
        }
        if mode == Mode::Folder && path.exists() {
            bail!("{} already exists; choose a new folder.", path.display());
        }
        if mode == Mode::Mount && path.read_dir().is_ok_and(|mut d| d.next().is_some()) {
            bail!(
                "{} is not empty; choose an empty folder to mount on.",
                path.display()
            );
        }
        let cache_size = if mode.is_lazy() {
            Some(parse_size(&form.get("Cache size"))?)
        } else {
            None
        };
        let v = VolumeConfig {
            key: self.lib.new_key(&name),
            name: name.clone(),
            connection: c.id.clone(),
            mode,
            path: Some(path.to_string_lossy().into()),
            cache_size,
        };
        let dir = library::synced_dir(&v, &self.places.data);
        self.places.create()?;
        let job = self.tool_job(library::attach_args(&v, &c, &dir), Some(&c));
        let what = if mode == Mode::Folder {
            "Downloading"
        } else {
            "Attaching"
        };
        self.spawn(&format!("{what} {name}"), job, move |app, r| match r {
            Ok(_) => app.added(v),
            Err(e) => app.fail(format!("Adding {name}: {e:#}")),
        });
        Ok(())
    }

    fn create_volume(&mut self, connection: &str, form: &Form) -> Result<()> {
        let c = self
            .lib
            .connection(connection)
            .context("no such connection")?
            .clone();
        let name = form.get("Name");
        if !library::valid_name(&name) {
            bail!("Use up to 40 letters, digits, '.', '_' or '-' for the name, not starting with '.'.");
        }
        let dir = expand(&form.get("Folder"));
        if !dir.is_dir() {
            bail!("{} is not a folder.", dir.display());
        }
        let dir = dir.canonicalize()?;
        let v = VolumeConfig {
            key: self.lib.new_key(&name),
            name: name.clone(),
            connection: c.id.clone(),
            mode: Mode::Folder,
            path: Some(dir.to_string_lossy().into()),
            cache_size: None,
        };
        let job = self.tool_job(library::init_args(&name, &c, &dir), Some(&c));
        self.spawn(&format!("Uploading {name}"), job, move |app, r| match r {
            Ok(_) => app.added(v),
            Err(e) => app.fail(format!("Creating {name}: {e:#}")),
        });
        Ok(())
    }

    /// A volume attached or created: saved, started, selected.
    fn added(&mut self, v: VolumeConfig) {
        let key = v.key.clone();
        let conn = v.connection.clone();
        self.lib.volumes.push(v);
        if let Err(e) = self.save() {
            self.fail(format!("{e:#}"));
            return;
        }
        self.rebuild_entries();
        self.select(&Entry::Volume(key.clone()));
        self.start(&key);
        // The connection's list marks it as added.
        if let Some(Loaded::Ready(_)) = self.remote.get(&conn) {
            self.load_remote_volumes(&conn);
        }
    }

    fn confirmed(&mut self, action: Action) {
        match action {
            Action::RemoveVolume(key) => {
                let services = self.services.clone();
                let k = key.clone();
                self.spawn(
                    &format!("Stopping {key}"),
                    move || services.stop(&k),
                    move |app, r| {
                        if let Err(e) = r {
                            app.fail(format!("Stopping {key}: {e:#}"));
                            return;
                        }
                        app.lib.volumes.retain(|v| v.key != key);
                        app.usage.remove(&key);
                        app.conflicts.remove(&key);
                        match app.save() {
                            Ok(()) => app.say(format!("Removed {key}")),
                            Err(e) => app.fail(format!("{e:#}")),
                        }
                        app.rebuild_entries();
                        app.selection_changed();
                    },
                );
            }
            Action::RemoveConnection(id) => {
                self.lib.connections.retain(|c| !c.id.eq_ignore_ascii_case(&id));
                let _ = self.secrets.forget(&id);
                let _ = std::fs::remove_file(self.places.ca_file(&id));
                self.remote.remove(&id);
                match self.save() {
                    Ok(()) => self.say("Connection removed"),
                    Err(e) => self.fail(format!("{e:#}")),
                }
                self.rebuild_entries();
                self.selection_changed();
            }
        }
    }

    // MARK: Volumes

    fn start_or_stop(&mut self, key: &str) {
        match self.state(key) {
            State::Syncing | State::Starting => self.stop(key),
            State::Elsewhere => self.fail(format!("{key} is run by the Mac app; stop it there.")),
            State::Stopped => self.start(key),
        }
    }

    pub fn start(&mut self, key: &str) {
        let Some(v) = self.lib.volume(key) else { return };
        if v.mode == Mode::Finder {
            self.fail(format!("{key} shows in Finder, which only the Mac app can run."));
            return;
        }
        let services = self.services.clone();
        let exe = self.exe.clone();
        let log = self.places.log(key);
        let k = key.to_string();
        self.installed.insert(key.to_string(), true);
        self.spawn(
            &format!("Starting {key}"),
            move || services.start(&k, &exe, &log),
            {
                let key = key.to_string();
                move |app, r| {
                    match r {
                        Ok(()) => app.say(format!("{key} syncs in the background, also after you quit")),
                        Err(e) => app.fail(format!("Starting {key}: {e:#}")),
                    }
                    app.last_poll = None;
                }
            },
        );
    }

    fn stop(&mut self, key: &str) {
        let services = self.services.clone();
        let k = key.to_string();
        self.spawn(&format!("Stopping {key}"), move || services.stop(&k), {
            let key = key.to_string();
            move |app, r| {
                match r {
                    Ok(()) => app.say(format!("Stopped {key}")),
                    Err(e) => app.fail(format!("Stopping {key}: {e:#}")),
                }
                app.usage.remove(&key);
                app.last_poll = None;
            }
        });
    }

    /// Restarts `key` if a service of the TUI runs it, so it picks up a
    /// changed setting.
    fn restart_if_ours(&mut self, key: &str) {
        if self.services.installed(key) {
            self.start(key);
        }
    }

    fn show_log(&mut self, key: &str) {
        let file = self.places.log(key);
        let lines: Vec<String> = match std::fs::read(&file) {
            Ok(data) => {
                let text = String::from_utf8_lossy(&data);
                let all: Vec<&str> = text.lines().collect();
                all[all.len().saturating_sub(2000)..]
                    .iter()
                    .map(|l| strip_ansi(l))
                    .collect()
            }
            Err(_) => vec![format!("No log yet ({}).", file.display())],
        };
        let scroll = lines.len().saturating_sub(1);
        self.modal = Some(Modal::Log {
            key: key.to_string(),
            lines,
            scroll,
        });
    }
}

const MOUNT_NOTE: &str = "Mounted: files download when first opened and the cache size \
                          bounds what stays downloaded. Needs FUSE (macFUSE on a Mac).";
const FOLDER_NOTE: &str = "A plain folder holding a full copy: everything downloads now.";

fn lazy_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Folder => "folder",
        _ => "mount",
    }
}

fn mode_from(label: &str) -> Mode {
    if label == "folder" {
        Mode::Folder
    } else {
        Mode::Mount
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Where a new volume goes by default: `~/rustFUSE/<name>` as a folder,
/// `~/mnt/<name>` (Linux) or `~/rustFUSE/<name>` (Mac) mounted.
fn default_path(name: &str, mode: Mode) -> String {
    let base = if mode == Mode::Mount && !cfg!(target_os = "macos") {
        "~/mnt"
    } else {
        "~/rustFUSE"
    };
    format!("{base}/{name}")
}

fn default_download(name: &str) -> String {
    if home().join("Downloads").is_dir() {
        format!("~/Downloads/{name}")
    } else {
        format!("./{name}")
    }
}

/// `~/x` to the home directory's `x`.
pub fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if path == "~" => home(),
        None => PathBuf::from(path),
    }
}

/// Folders first, then by name.
fn sorted_items(mut items: Vec<Item>) -> Vec<Item> {
    items.sort_by(|a, b| {
        (a.kind != Kind::Dir, a.name.to_lowercase()).cmp(&(b.kind != Kind::Dir, b.name.to_lowercase()))
    });
    items
}

/// Runs `rustfuse args`; its output, or its error output as the error.
pub fn tool(exe: &Path, args: &[String], env: &BTreeMap<String, String>) -> Result<String> {
    let out = Command::new(exe)
        .args(args)
        .envs(env)
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("running {}", exe.display()))?;
    if !out.status.success() {
        let err = strip_ansi(String::from_utf8_lossy(&out.stderr).trim());
        let last: Vec<&str> = err.lines().rev().take(3).collect();
        let msg: Vec<&str> = last.into_iter().rev().collect();
        bail!(
            "{}",
            if msg.is_empty() {
                format!("rustfuse {} failed", args[0])
            } else {
                msg.join(" ")
            }
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Bytes in decimal units, as Finder shows sizes: "1.16 GB".
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} {}", if bytes == 1 { "byte" } else { "bytes" });
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit < UNITS.len() - 1 {
        v /= 1000.0;
        unit += 1;
    }
    let text = if v >= 100.0 {
        format!("{v:.0}")
    } else if v >= 10.0 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    };
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text
    };
    format!("{text} {}", UNITS[unit])
}

/// "20", "20 GB", "500MB", "1.5T": decimal units, GB when none is given.
pub fn parse_size(text: &str) -> Result<u64> {
    let t = text.trim().to_ascii_uppercase().replace(' ', "");
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("Give a size like 20 GB."))?;
    let mult = match unit.trim_end_matches('B') {
        "" if unit == "B" => 1.0,
        "" | "G" => 1e9,
        "K" => 1e3,
        "M" => 1e6,
        "T" => 1e12,
        _ => bail!("Give a size like 20 GB."),
    };
    if n <= 0.0 {
        bail!("The size must be more than 0.");
    }
    Ok((n * mult).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1_160_000_000), "1.16 GB");
        assert_eq!(format_size(20_000_000_000), "20 GB");
        assert_eq!(format_size(412_000_000_000), "412 GB");
        assert_eq!(format_size(12_408), "12.4 KB");
        assert_eq!(parse_size("20").unwrap(), 20_000_000_000);
        assert_eq!(parse_size("20 GB").unwrap(), 20_000_000_000);
        assert_eq!(parse_size("500mb").unwrap(), 500_000_000);
        assert_eq!(parse_size("1.5T").unwrap(), 1_500_000_000_000);
        assert_eq!(parse_size("4096B").unwrap(), 4096);
        assert!(parse_size("lots").is_err());
        assert!(parse_size("0").is_err());
        assert_eq!(parse_size(&format_size(1_160_000_000)).unwrap(), 1_160_000_000);
    }

    #[test]
    fn ansi() {
        assert_eq!(
            strip_ansi("\u{1b}[2m2026\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m x"),
            "2026  INFO x"
        );
    }
}
