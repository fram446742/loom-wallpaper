//! `loom-wallpaper`: per-workspace wallpaper scenes for niri.
//!
//! Scenes map workspace names (or `default`) to image paths. Two backends can
//! render them:
//!
//! - **native** (`native`): one `zwlr_layer_shell_v1` background surface per output, filled through
//!   `wl_shm` — no external wallpaper program needed.
//! - **command** (`command`): the original behaviour, an external command template (`swaybg`/`ssww`
//!   style) run per output, with `{image}`, `{workspace}` and `{output}` placeholders.
//!
//! `auto` (the default) uses the native backend when the compositor supports
//! it and degrades to the command template otherwise. `watch` follows niri's
//! event stream and reapplies on workspace focus changes; `apply` renders once.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use niri_ipc::{Event, Request, Response, Workspace};
use serde::{Deserialize, Serialize};

pub mod backend;
pub mod geom;
pub mod hdr;
pub mod images;
pub mod native;
pub mod schedule;
pub mod video;

use schedule::{Playlist, TimeRule, TransitionSpec};

pub const DEFAULT_COMMAND: &str = "swaybg -i {image}";

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Scene {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub wallpapers: BTreeMap<String, String>,
    /// Cross-fade used by the native backend (command backends have their own
    /// flags; see the README).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<TransitionSpec>,
    /// Ordered/shuffled rotation used when the workspace has no exact image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist: Option<Playlist>,
    /// Day/night ranges; the first matching range wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub time_rules: Vec<TimeRule>,
    /// Pick a random scene image (stable for a five-minute slot).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub random: bool,
    /// Free-form metadata kept across import/export.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Store {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default)]
    pub scenes: BTreeMap<String, Scene>,
    /// Per-session scene selection, e.g. `work` -> `office`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub session_scenes: BTreeMap<String, String>,
}

pub fn state_path() -> PathBuf {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".local/state")
        });
    dir.join("loom-wallpaper/scenes.json")
}

impl Store {
    pub fn load(path: &Path) -> std::io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(tmp, path)
    }

    pub fn active_scene(&self) -> Option<&Scene> {
        self.active
            .as_deref()
            .and_then(|name| self.scenes.get(name))
    }

    /// Scene selection for a babysitting session (`None` = the normal active
    /// scene). Unknown session names fall back to the active scene.
    pub fn scene_for_session(&self, session: Option<&str>) -> Option<&Scene> {
        session
            .and_then(|session| self.session_scenes.get(session))
            .and_then(|name| self.scenes.get(name))
            .or_else(|| self.active_scene())
    }

    /// Name of the scene selected for a session.
    pub fn scene_name_for_session(&self, session: Option<&str>) -> Option<&str> {
        session
            .and_then(|session| self.session_scenes.get(session))
            .map(String::as_str)
            .filter(|name| self.scenes.contains_key(*name))
            .or(self.active.as_deref())
    }

    /// Binds a session name to a scene.
    pub fn use_scene_for_session(&mut self, session: &str, scene: &str) -> Result<(), String> {
        if !self.scenes.contains_key(scene) {
            return Err(format!("unknown scene `{scene}`"));
        }
        self.session_scenes
            .insert(session.to_owned(), scene.to_owned());
        Ok(())
    }

    /// Exports one scene as a JSON document with its name.
    pub fn export_scene(&self, name: &str) -> Result<String, String> {
        let scene = self
            .scenes
            .get(name)
            .ok_or_else(|| format!("unknown scene `{name}`"))?;
        serde_json::to_string_pretty(&serde_json::json!({
            "name": name,
            "scene": scene,
        }))
        .map_err(|err| format!("cannot serialize scene: {err}"))
    }

    /// Imports a scene exported by [`Self::export_scene`] (or a bare scene
    /// object) under `name`, defaulting to the exported name.
    pub fn import_scene(&mut self, text: &str, name: Option<&str>) -> Result<String, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|err| format!("invalid scene JSON: {err}"))?;
        let (exported_name, scene_value) = match value.get("scene") {
            Some(scene) => (
                value.get("name").and_then(serde_json::Value::as_str),
                scene.clone(),
            ),
            None => (None, value),
        };
        let name = name
            .map(str::to_owned)
            .or_else(|| exported_name.map(str::to_owned))
            .ok_or_else(|| "imported scene has no name; pass one explicitly".to_owned())?;
        let scene: Scene =
            serde_json::from_value(scene_value).map_err(|err| format!("invalid scene: {err}"))?;
        scene_validate(&scene)?;
        self.scenes.insert(name.clone(), scene);
        Ok(name)
    }
}

/// Resolves the image for a workspace: exact name, then scene default.
pub fn resolve<'a>(scene: &'a Scene, workspace: Option<&str>) -> Option<&'a String> {
    workspace
        .and_then(|workspace| scene.wallpapers.get(workspace))
        .or(scene.default.as_ref())
}

/// Validates scene-specific extras (time ranges and playlist intervals).
pub fn scene_validate(scene: &Scene) -> Result<(), String> {
    for rule in &scene.time_rules {
        rule.validate()?;
    }
    if scene
        .wallpapers
        .values()
        .any(|image| image.trim().is_empty())
    {
        return Err("scene wallpaper image must not be empty".to_owned());
    }
    if let Some(default) = scene.default.as_deref()
        && default.trim().is_empty()
    {
        return Err("scene default image must not be empty".to_owned());
    }
    Ok(())
}

/// Local minute of day (0..1440) for a Unix timestamp.
pub fn local_minute_of_day(unix: i64) -> u32 {
    #[cfg(unix)]
    {
        let time = unix as libc::time_t;
        // SAFETY: `localtime_r` writes into a `tm` we own and does not retain
        // the pointer; only the hour/minute fields are read immediately.
        unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&time, &mut tm).is_null() {
                return (unix.rem_euclid(86_400) / 60) as u32;
            }
            ((tm.tm_hour as u32) * 60 + tm.tm_min as u32) % 1440
        }
    }
    #[cfg(not(unix))]
    {
        (unix.rem_euclid(86_400) / 60) as u32
    }
}

/// Current Unix time and local minute of day.
pub fn local_now() -> (i64, u32) {
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    (unix, local_minute_of_day(unix))
}

/// Resolves the image including time rules, playlists and random selection.
///
/// Priority: first matching time rule, workspace image, playlist slot, random
/// scene image, scene default. `now` is Unix seconds; the seed for a shuffled
/// or random choice comes from the same value, so redraws within one interval
/// are stable.
pub fn resolve_at<'a>(
    scene: &'a Scene,
    workspace: Option<&str>,
    minute_of_day: u32,
    now: u64,
) -> Option<&'a String> {
    if let Some(rule) = scene
        .time_rules
        .iter()
        .find(|rule| rule.matches(minute_of_day))
    {
        return Some(&rule.image);
    }
    if let Some(image) = workspace.and_then(|workspace| scene.wallpapers.get(workspace)) {
        return Some(image);
    }
    if let Some(image) = scene
        .playlist
        .as_ref()
        .and_then(|playlist| playlist.image_at(now, now))
    {
        return Some(image);
    }
    if scene.random {
        let candidates = scene_candidates(scene);
        let slot = now / 300;
        if let Some(index) = schedule::random_index(candidates.len(), slot)
            && let Some(image) = candidates.get(index)
            && let Some(image) = scene_image(scene, image)
        {
            return Some(image);
        }
    }
    scene.default.as_ref()
}

/// Finds the borrowed original of an image chosen from [`scene_candidates`].
fn scene_image<'a>(scene: &'a Scene, image: &str) -> Option<&'a String> {
    if let Some(image) = scene
        .playlist
        .as_ref()
        .and_then(|playlist| playlist.images.iter().find(|candidate| *candidate == image))
    {
        return Some(image);
    }
    if let Some(image) = scene
        .wallpapers
        .values()
        .find(|candidate| *candidate == image)
    {
        return Some(image);
    }
    scene
        .default
        .as_ref()
        .filter(|default| default.as_str() == image)
}

/// Every image a random scene can choose from (playlist first, then named
/// wallpapers and the default), deduplicated deterministically.
pub fn scene_candidates(scene: &Scene) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(playlist) = &scene.playlist {
        for image in &playlist.images {
            if !candidates.contains(image) {
                candidates.push(image.clone());
            }
        }
    }
    for image in scene.wallpapers.values() {
        if !candidates.contains(image) {
            candidates.push(image.clone());
        }
    }
    if let Some(default) = &scene.default
        && !candidates.contains(default)
    {
        candidates.push(default.clone());
    }
    candidates
}

/// Session name from loom-session's hook environment or a session file.
///
/// `LOOM_SESSION_CONTEXT` is set by loom-session's restore hooks;
/// `LOOM_WALLPAPER_SESSION` overrides it, and
/// `LOOM_WALLPAPER_SESSION_FILE` can name a file whose first line is the
/// session (useful when pairing with an external session manager).
pub fn session_name() -> Option<String> {
    for name in ["LOOM_WALLPAPER_SESSION", "LOOM_SESSION_CONTEXT"] {
        if let Some(value) = std::env::var_os(name)
            && let Some(value) = value.to_str().map(str::trim).filter(|v| !v.is_empty())
        {
            return Some(value.to_owned());
        }
    }
    if let Some(path) = std::env::var_os("LOOM_WALLPAPER_SESSION_FILE") {
        return std::fs::read_to_string(path).ok().and_then(|text| {
            text.lines()
                .next()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
        });
    }
    None
}

pub fn command_template() -> String {
    std::env::var("LOOM_WALLPAPER_COMMAND").unwrap_or_else(|_| DEFAULT_COMMAND.to_owned())
}

pub fn render_command(template: &str, image: &str, workspace: &str, output: &str) -> String {
    template
        .replace("{image}", image)
        .replace("{workspace}", workspace)
        .replace("{output}", output)
}

fn spawn(command: &str) -> Result<Child, String> {
    Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("cannot run wallpaper command: {err}"))
}

/// Stops a wallpaper process: gives short-lived commands a brief grace period
/// so a just-spawned command is not killed before it runs, then kills it.
fn stop_child(mut child: Child) {
    for _ in 0..10 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: u64,
    pub name: Option<String>,
    pub output: Option<String>,
    pub is_focused: bool,
}

pub fn workspace_infos(workspaces: &[Workspace]) -> Vec<WorkspaceInfo> {
    workspaces
        .iter()
        .map(|ws| WorkspaceInfo {
            id: ws.id,
            name: ws.name.clone(),
            output: ws.output.clone(),
            is_focused: ws.is_focused,
        })
        .collect()
}

/// Maps each output name to the name of the workspace currently active on it.
///
/// The native backend renders per output, so it needs the active workspace of
/// every output, not just the globally focused one. Unnamed workspaces and
/// outputs without a name are skipped; those outputs fall back to the scene
/// `default`.
pub fn active_workspaces(workspaces: &[Workspace]) -> BTreeMap<String, String> {
    workspaces
        .iter()
        .filter(|ws| ws.is_active)
        .filter_map(|ws| Some((ws.output.clone()?, ws.name.clone()?)))
        .collect()
}

/// Resolves the image to show on every output given the active workspace map.
pub fn plan_for_outputs(
    scene: &Scene,
    active: &BTreeMap<String, String>,
) -> BTreeMap<String, Option<String>> {
    let (now, minute) = local_now();
    plan_for_outputs_at(scene, active, minute, now as u64)
}

/// Time-aware variant of [`plan_for_outputs`].
pub fn plan_for_outputs_at(
    scene: &Scene,
    active: &BTreeMap<String, String>,
    minute_of_day: u32,
    now: u64,
) -> BTreeMap<String, Option<String>> {
    active
        .iter()
        .map(|(output, workspace)| {
            (
                output.clone(),
                resolve_at(scene, Some(workspace), minute_of_day, now).cloned(),
            )
        })
        .collect()
}

/// Queries niri once and resolves the image for every output. Used by
/// `--dry-run` (no compositor surface is created) and by the native backend's
/// startup message.
pub fn workspace_plan(
    socket: &Path,
    store: &Store,
) -> Result<BTreeMap<String, Option<String>>, String> {
    let scene = store
        .active_scene()
        .ok_or_else(|| "no active scene; use `scene use <name>` first".to_owned())?;
    let workspaces = match send(socket, Request::Workspaces)? {
        Response::Workspaces(workspaces) => workspaces,
        other => return Err(format!("unexpected niri response: {other:?}")),
    };
    Ok(plan_for_outputs(scene, &active_workspaces(&workspaces)))
}

#[derive(Debug, Default)]
pub struct Watcher {
    template: String,
    dry_run: bool,
    applied: HashMap<String, String>,
    children: HashMap<String, Child>,
}

impl Watcher {
    pub fn new(template: impl Into<String>, dry_run: bool) -> Self {
        Self {
            template: template.into(),
            dry_run,
            applied: HashMap::new(),
            children: HashMap::new(),
        }
    }

    pub fn apply(
        &mut self,
        scene: &Scene,
        workspace: Option<&str>,
        output: &str,
    ) -> Result<Option<String>, String> {
        let (now, minute) = local_now();
        let Some(image) = resolve_at(scene, workspace, minute, now as u64) else {
            return Ok(None);
        };
        if self.applied.get(output) == Some(image) {
            return Ok(Some(image.clone()));
        }
        let workspace = workspace.unwrap_or("default");
        let command = render_command(&self.template, image, workspace, output);
        if !self.dry_run {
            if let Some(old) = self.children.remove(output) {
                stop_child(old);
            }
            let child = spawn(&command)?;
            self.children.insert(output.to_owned(), child);
        }
        self.applied.insert(output.to_owned(), image.clone());
        Ok(Some(image.clone()))
    }

    pub fn finish(&mut self) {
        for (_, child) in self.children.drain() {
            stop_child(child);
        }
    }
}

pub(crate) fn send(socket: &Path, request: Request) -> Result<Response, String> {
    use niri_ipc::socket::Socket;
    let mut socket =
        Socket::connect_to(socket).map_err(|err| format!("cannot connect to niri IPC: {err}"))?;
    let reply = socket
        .send(request)
        .map_err(|err| format!("niri IPC error: {err}"))?;
    reply.map_err(|err| format!("niri returned an error: {err}"))
}

/// Applies the active scene once to the currently focused workspace.
pub fn apply_once(socket: &Path, store: &Store, dry_run: bool) -> Result<Option<String>, String> {
    let scene = store
        .active_scene()
        .ok_or_else(|| "no active scene; use `scene use <name>` first".to_owned())?;
    let workspaces = match send(socket, Request::Workspaces)? {
        Response::Workspaces(workspaces) => workspaces,
        other => return Err(format!("unexpected niri response: {other:?}")),
    };
    let focused = workspaces
        .iter()
        .find(|ws| ws.is_focused)
        .ok_or_else(|| "no focused workspace".to_owned())?;
    let mut watcher = Watcher::new(command_template(), dry_run);
    let result = watcher.apply(
        scene,
        focused.name.as_deref(),
        focused.output.as_deref().unwrap_or(""),
    );
    watcher.finish();
    result
}

/// Watches the niri event stream until it closes, reapplying on focus changes.
pub fn watch(
    socket_path: &Path,
    store: &Store,
    template: &str,
    dry_run: bool,
) -> Result<usize, String> {
    use niri_ipc::socket::Socket;

    let scene = store
        .active_scene()
        .ok_or_else(|| "no active scene; use `scene use <name>` first".to_owned())?;
    let mut stream = Socket::connect_to(socket_path)
        .map_err(|err| format!("cannot connect to niri IPC: {err}"))?;
    match stream
        .send(Request::EventStream)
        .map_err(|err| format!("niri IPC error: {err}"))?
    {
        Ok(Response::Handled) => (),
        Ok(other) => return Err(format!("unexpected niri response: {other:?}")),
        Err(message) => return Err(message),
    }

    let mut watcher = Watcher::new(template, dry_run);
    let mut applied = 0;
    let mut read_event = stream.read_events();
    loop {
        let event = match read_event() {
            Ok(event) => event,
            Err(err)
                if err.kind() == std::io::ErrorKind::UnexpectedEof
                    || err.kind() == std::io::ErrorKind::ConnectionReset =>
            {
                break;
            }
            Err(err) => return Err(format!("event stream: {err}")),
        };
        match event {
            Event::WorkspacesChanged { workspaces } => {
                if let Some(ws) = workspaces.iter().find(|ws| ws.is_focused)
                    && watcher
                        .apply(
                            scene,
                            ws.name.as_deref(),
                            ws.output.as_deref().unwrap_or(""),
                        )
                        .map_err(|err| format!("event stream: {err}"))?
                        .is_some()
                {
                    applied += 1;
                }
            }
            Event::WorkspaceActivated { id, focused } => {
                if !focused {
                    continue;
                }
                let workspaces = match send(socket_path, Request::Workspaces) {
                    Ok(Response::Workspaces(workspaces)) => workspaces,
                    _ => continue,
                };
                if let Some(ws) = workspaces.iter().find(|ws| ws.id == id)
                    && watcher
                        .apply(
                            scene,
                            ws.name.as_deref(),
                            ws.output.as_deref().unwrap_or(""),
                        )
                        .map_err(|err| format!("event stream: {err}"))?
                        .is_some()
                {
                    applied += 1;
                }
            }
            _ => {}
        }
    }
    watcher.finish();
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::*;

    /// Waits until the file content matches `predicate` (commands run in
    /// detached processes, so completion is not synchronous).
    fn wait_for(path: &Path, predicate: impl Fn(&str) -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && predicate(&text)
            {
                return;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting for {what}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("loom-wallpaper-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn focused_workspace() -> serde_json::Value {
        json!({
            "id": 7,
            "idx": 1,
            "name": "main",
            "output": "eDP-1",
            "is_urgent": false,
            "is_active": true,
            "is_focused": true,
            "active_window_id": null,
            "is_hidden": false
        })
    }

    fn event_server(path: &PathBuf) -> std::thread::JoinHandle<()> {
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Request = serde_json::from_str(line.trim()).unwrap();
            assert!(matches!(request, Request::EventStream));
            let mut stream = stream;
            stream.write_all(b"{\"Ok\":\"Handled\"}\n").unwrap();
            let event = json!({ "WorkspacesChanged": { "workspaces": [focused_workspace()] } });
            stream
                .write_all(serde_json::to_string(&event).unwrap().as_bytes())
                .unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
        })
    }

    #[test]
    fn resolves_named_and_default_images() {
        let scene = Scene {
            default: Some("/default.png".to_owned()),
            wallpapers: BTreeMap::from([("main".to_owned(), "/main.png".to_owned())]),
            ..Scene::default()
        };
        assert_eq!(resolve(&scene, Some("main")).unwrap(), "/main.png");
        assert_eq!(resolve(&scene, Some("other")).unwrap(), "/default.png");
        assert_eq!(resolve(&scene, None).unwrap(), "/default.png");
        let empty = Scene::default();
        assert!(resolve(&empty, Some("main")).is_none());
    }

    #[test]
    fn renders_all_placeholders() {
        assert_eq!(
            render_command(
                "swaybg -i {image} --output {output} --workspace {workspace}",
                "/img.png",
                "main",
                "eDP-1"
            ),
            "swaybg -i /img.png --output eDP-1 --workspace main"
        );
    }

    #[test]
    fn store_round_trips() {
        let path = temp_dir("store").join("scenes.json");
        let mut store = Store::default();
        let mut scene = Scene::default();
        scene
            .wallpapers
            .insert("main".to_owned(), "/main.png".to_owned());
        store.scenes.insert("work".to_owned(), scene);
        store.active = Some("work".to_owned());
        store.save(&path).unwrap();

        let reloaded = Store::load(&path).unwrap();
        assert_eq!(reloaded.active.as_deref(), Some("work"));
        assert_eq!(
            reloaded.active_scene().unwrap().wallpapers["main"],
            "/main.png"
        );
    }

    #[test]
    fn watcher_applies_images_and_dry_run_spawns_nothing() {
        let dir = temp_dir("watcher");
        let log = dir.join("log");
        let template = format!("printf '%s\\n' {{image}} >> {}", log.display());
        let scene = Scene {
            default: None,
            wallpapers: BTreeMap::from([
                ("main".to_owned(), "/a.png".to_owned()),
                ("web".to_owned(), "/b.png".to_owned()),
            ]),
            ..Scene::default()
        };

        let mut dry = Watcher::new(&template, true);
        assert_eq!(
            dry.apply(&scene, Some("main"), "eDP-1").unwrap(),
            Some("/a.png".to_owned())
        );
        dry.finish();
        assert!(!log.exists());

        // Different outputs keep independent processes; the same output would
        // replace the previous wallpaper (covered by `replaces_same_output`).
        let mut watcher = Watcher::new(&template, false);
        watcher.apply(&scene, Some("main"), "eDP-1").unwrap();
        watcher.apply(&scene, Some("web"), "HDMI-1").unwrap();
        wait_for(&log, |text| text.lines().count() == 2, "both images");
        watcher.finish();
        // Both detached commands run concurrently, so their appends can land in
        // either order; each image must appear exactly once.
        let mut lines: Vec<_> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        lines.sort();
        assert_eq!(lines, ["/a.png", "/b.png"]);
    }

    #[test]
    fn watcher_replaces_previous_process_on_same_output() {
        let dir = temp_dir("replace");
        let log = dir.join("log");
        let pids = dir.join("pids");
        let template = format!(
            "printf '%s\\n' {{image}} >> {}; printf '%s\\n' $$ >> {}; sleep 0.2",
            log.display(),
            pids.display()
        );
        let scene = Scene {
            default: None,
            wallpapers: BTreeMap::from([
                ("main".to_owned(), "/a.png".to_owned()),
                ("web".to_owned(), "/b.png".to_owned()),
            ]),
            ..Scene::default()
        };
        let mut watcher = Watcher::new(&template, false);
        watcher.apply(&scene, Some("main"), "eDP-1").unwrap();
        watcher.apply(&scene, Some("web"), "eDP-1").unwrap();
        // The second apply must have killed the first process: only the second
        // image is current for the output.
        assert_eq!(watcher.applied["eDP-1"], "/b.png");
        assert_eq!(watcher.children.len(), 1);
        wait_for(&log, |text| text.lines().count() == 2, "replaced images");
        watcher.finish();
        let lines = std::fs::read_to_string(&log).unwrap();
        assert_eq!(lines.lines().count(), 2);
        assert!(lines.ends_with("/b.png\n"));
    }

    #[test]
    fn watch_stream_applies_focused_workspace() {
        let dir = temp_dir("watch");
        let log = dir.join("log");
        let socket = dir.join("niri.sock");
        let server = event_server(&socket);
        let template = format!("printf '%s' {{image}} >> {}", log.display());
        let store = Store {
            active: Some("work".to_owned()),
            scenes: BTreeMap::from([(
                "work".to_owned(),
                Scene {
                    default: None,
                    wallpapers: BTreeMap::from([("main".to_owned(), "/main.png".to_owned())]),
                    ..Scene::default()
                },
            )]),
            ..Store::default()
        };

        let applied = watch(&socket, &store, &template, false).unwrap();
        assert_eq!(applied, 1);
        server.join().unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "/main.png");
    }

    #[test]
    fn apply_once_queries_focused_workspace() {
        let dir = temp_dir("once");
        let socket = dir.join("niri.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Request = serde_json::from_str(line.trim()).unwrap();
            assert!(matches!(request, Request::Workspaces));
            let response = json!({ "Ok": { "Workspaces": [focused_workspace()] } });
            let mut stream = stream;
            stream
                .write_all(serde_json::to_string(&response).unwrap().as_bytes())
                .unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
        });
        let store = Store {
            active: Some("work".to_owned()),
            scenes: BTreeMap::from([(
                "work".to_owned(),
                Scene {
                    default: Some("/default.png".to_owned()),
                    wallpapers: BTreeMap::new(),
                    ..Scene::default()
                },
            )]),
            ..Store::default()
        };
        // Dry run: verifies the IPC query + resolution without spawning a
        // wallpaper process (spawning is covered by the watcher test).
        let image = apply_once(&socket, &store, true).unwrap();
        server.join().unwrap();
        assert_eq!(image.as_deref(), Some("/default.png"));
    }

    #[test]
    fn command_executable_bit_is_preserved() {
        // Sanity check for the fake-command helpers: a script must be executable.
        let dir = temp_dir("exec");
        let script = dir.join("cmd");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let meta = std::fs::metadata(&script).unwrap();
        assert_eq!(meta.permissions().mode() & 0o111, 0o111);
    }

    fn workspace(id: u64, name: Option<&str>, output: Option<&str>, active: bool) -> Workspace {
        Workspace {
            id,
            idx: id as u8,
            name: name.map(str::to_owned),
            output: output.map(str::to_owned),
            is_urgent: false,
            is_active: active,
            is_focused: false,
            active_window_id: None,
            is_hidden: false,
        }
    }

    #[test]
    fn active_workspaces_maps_every_output() {
        let workspaces = vec![
            workspace(1, Some("main"), Some("eDP-1"), true),
            workspace(2, Some("web"), Some("eDP-1"), false),
            workspace(3, Some("scratch"), Some("HDMI-1"), true),
            workspace(4, None, Some("DP-1"), true),
            workspace(5, Some("ghost"), None, true),
        ];
        let active = active_workspaces(&workspaces);
        assert_eq!(active.len(), 2);
        assert_eq!(active["eDP-1"], "main");
        assert_eq!(active["HDMI-1"], "scratch");
        assert!(!active.contains_key("DP-1"));
        assert!(!active.contains_key(""));
    }

    #[test]
    fn plan_for_outputs_resolves_each_active_workspace() {
        let scene = Scene {
            default: Some("/default.png".to_owned()),
            wallpapers: BTreeMap::from([
                ("main".to_owned(), "/main.png".to_owned()),
                ("web".to_owned(), "/web.png".to_owned()),
            ]),
            ..Scene::default()
        };
        let active = BTreeMap::from([
            ("eDP-1".to_owned(), "main".to_owned()),
            ("HDMI-1".to_owned(), "games".to_owned()),
        ]);
        let plan = plan_for_outputs(&scene, &active);
        assert_eq!(plan["eDP-1"].as_deref(), Some("/main.png"));
        assert_eq!(plan["HDMI-1"].as_deref(), Some("/default.png"));

        let empty = Scene::default();
        let plan = plan_for_outputs(&empty, &active);
        assert!(plan["eDP-1"].is_none());
    }

    #[test]
    fn scene_parses_partial_json_and_rejects_bad_values() {
        let scene: Scene = serde_json::from_str(r#"{"wallpapers":{"main":"/m.png"}}"#).unwrap();
        assert_eq!(scene.wallpapers["main"], "/m.png");
        assert!(scene.default.is_none());
        assert!(
            serde_json::from_str::<Scene>("{}")
                .unwrap()
                .wallpapers
                .is_empty()
        );
        assert!(serde_json::from_str::<Scene>(r#"{"wallpapers":{"main":7}}"#).is_err());
    }

    #[test]
    fn store_load_reports_malformed_json() {
        let dir = temp_dir("badjson");
        let path = dir.join("scenes.json");
        std::fs::write(&path, b"{not json").unwrap();
        let err = Store::load(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        // A missing file is an empty store, not an error.
        let missing = Store::load(&dir.join("absent.json")).unwrap();
        assert!(missing.scenes.is_empty());
    }

    #[test]
    fn workspace_plan_resolves_every_output_from_niri() {
        let dir = temp_dir("plan");
        let socket = dir.join("niri.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Request = serde_json::from_str(line.trim()).unwrap();
            assert!(matches!(request, Request::Workspaces));
            let response = json!({ "Ok": { "Workspaces": [
                focused_workspace(),
                {
                    "id": 8,
                    "idx": 1,
                    "name": "web",
                    "output": "HDMI-1",
                    "is_urgent": false,
                    "is_active": true,
                    "is_focused": false,
                    "active_window_id": null,
                    "is_hidden": false
                }
            ] } });
            let mut stream = stream;
            stream
                .write_all(serde_json::to_string(&response).unwrap().as_bytes())
                .unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
        });
        let store = Store {
            active: Some("work".to_owned()),
            scenes: BTreeMap::from([(
                "work".to_owned(),
                Scene {
                    default: Some("/default.png".to_owned()),
                    wallpapers: BTreeMap::from([("main".to_owned(), "/main.png".to_owned())]),
                    ..Scene::default()
                },
            )]),
            ..Store::default()
        };
        let plan = workspace_plan(&socket, &store).unwrap();
        server.join().unwrap();
        assert_eq!(plan["eDP-1"].as_deref(), Some("/main.png"));
        assert_eq!(plan["HDMI-1"].as_deref(), Some("/default.png"));
    }

    #[test]
    fn time_rules_take_precedence_over_workspace_images() {
        let scene = Scene {
            default: Some("/default.png".to_owned()),
            wallpapers: BTreeMap::from([("main".to_owned(), "/main.png".to_owned())]),
            time_rules: vec![
                TimeRule {
                    start: "07:00".to_owned(),
                    end: "19:00".to_owned(),
                    image: "/day.png".to_owned(),
                },
                TimeRule {
                    start: "19:00".to_owned(),
                    end: "07:00".to_owned(),
                    image: "/night.png".to_owned(),
                },
            ],
            ..Scene::default()
        };
        assert_eq!(
            resolve_at(&scene, Some("main"), 12 * 60, 100).map(String::as_str),
            Some("/day.png")
        );
        assert_eq!(
            resolve_at(&scene, Some("main"), 23 * 60, 100).map(String::as_str),
            Some("/night.png")
        );
        // Outside a matching rule the workspace wins.
        let without_rules = Scene {
            time_rules: Vec::new(),
            ..scene
        };
        assert_eq!(
            resolve_at(&without_rules, Some("main"), 12 * 60, 100).map(String::as_str),
            Some("/main.png")
        );
    }

    #[test]
    fn playlist_rotates_and_random_selection_is_stable() {
        let mut scene = Scene {
            default: Some("/default.png".to_owned()),
            playlist: Some(Playlist {
                images: vec!["/a.png".to_owned(), "/b.png".to_owned()],
                interval_secs: 10,
                shuffle: false,
            }),
            ..Scene::default()
        };
        assert_eq!(
            resolve_at(&scene, None, 0, 0).map(String::as_str),
            Some("/a.png")
        );
        assert_eq!(
            resolve_at(&scene, None, 0, 10).map(String::as_str),
            Some("/b.png")
        );

        scene.playlist = None;
        scene.random = true;
        scene
            .wallpapers
            .insert("other".to_owned(), "/c.png".to_owned());
        let first = resolve_at(&scene, None, 0, 100).unwrap().clone();
        let same_slot = resolve_at(&scene, None, 0, 199).unwrap().clone();
        assert_eq!(first, same_slot);
        let mut seen = std::collections::BTreeSet::new();
        for slot in 0..12 {
            seen.insert(resolve_at(&scene, None, 0, slot * 300).unwrap().clone());
        }
        assert!(seen.len() > 1);
        assert!(scene_candidates(&scene).contains(&"/c.png".to_owned()));
    }

    #[test]
    fn session_scenes_select_a_named_scene() {
        let mut store = Store {
            active: Some("default".to_owned()),
            scenes: BTreeMap::from([
                ("default".to_owned(), Scene::default()),
                (
                    "office".to_owned(),
                    Scene {
                        default: Some("/office.png".to_owned()),
                        ..Scene::default()
                    },
                ),
            ]),
            ..Store::default()
        };
        assert_eq!(store.scene_name_for_session(None), Some("default"));
        store.use_scene_for_session("work", "office").unwrap();
        assert_eq!(store.scene_name_for_session(Some("work")), Some("office"));
        assert_eq!(
            store
                .scene_for_session(Some("work"))
                .and_then(|scene| scene.default.as_deref()),
            Some("/office.png")
        );
        // Unknown sessions fall back to the active scene.
        assert_eq!(
            store.scene_name_for_session(Some("missing")),
            Some("default")
        );
        assert!(store.use_scene_for_session("work", "missing").is_err());
    }

    #[test]
    fn scenes_export_and_import_with_metadata() {
        let mut store = Store::default();
        let scene = Scene {
            default: Some("/a.png".to_owned()),
            metadata: BTreeMap::from([("author".to_owned(), "me".to_owned())]),
            transition: Some(TransitionSpec {
                duration_ms: 250,
                easing: schedule::Easing::Out,
            }),
            ..Scene::default()
        };
        store.scenes.insert("work".to_owned(), scene.clone());
        let exported = store.export_scene("work").unwrap();
        assert!(exported.contains("\"name\": \"work\""), "{exported}");
        assert!(exported.contains("author"), "{exported}");

        let mut other = Store::default();
        let name = other.import_scene(&exported, None).unwrap();
        assert_eq!(name, "work");
        assert_eq!(other.scenes["work"], scene);
        let renamed = other.import_scene(&exported, Some("copy")).unwrap();
        assert_eq!(renamed, "copy");
        assert_eq!(other.scenes["copy"].metadata["author"], "me");
        assert!(other.import_scene("{}", None).is_err());
        assert!(other.import_scene("not json", None).is_err());
        assert!(store.export_scene("missing").is_err());
    }

    #[test]
    fn scene_validation_reports_bad_time_rules_and_empty_images() {
        let scene = Scene {
            default: Some(String::new()),
            time_rules: vec![TimeRule {
                start: "99:99".to_owned(),
                end: "07:00".to_owned(),
                image: "/x".to_owned(),
            }],
            ..Scene::default()
        };
        let error = scene_validate(&scene).unwrap_err();
        assert!(error.contains("HH:MM"), "{error}");
        let empty = Scene {
            time_rules: Vec::new(),
            ..scene
        };
        assert!(scene_validate(&empty).is_err());
    }

    #[test]
    fn local_minute_of_day_is_in_range() {
        let minute = local_minute_of_day(1_709_251_200);
        assert!(minute < 1440);
        let (_now, minute) = local_now();
        assert!(minute < 1440);
    }
}
