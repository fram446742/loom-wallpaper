//! Native background renderer: one `zwlr_layer_shell_v1` background surface per
//! output, drawn into `wl_shm` buffers.
//!
//! Each surface is anchored to all four edges with an exclusive zone of `0`, so
//! the compositor makes it cover exactly the output (including scale and
//! rotation, which the compositor applies to the surface). Images are scaled to
//! cover the surface and centered; the image for an output is the one resolved
//! for that output's active workspace, falling back to the scene `default`.
//!
//! Images and SDR video use 8-bit `Xrgb8888`. When niri reports the output as
//! HDR-active and the compositor advertises `wp_color_manager_v1` with a
//! parametric PQ/BT.2020 description, HDR video is decoded to 10-bit
//! `Xrgb2101010` and that description is attached to the surface; everything
//! else keeps the SDR path, which the compositor tone-maps itself.
//!
//! niri events arrive on a dedicated thread that reconnects on its own and
//! feeds a calloop channel, so the Wayland event loop never blocks on IPC. A
//! one-second timer catches image file changes, output HDR toggles and retries
//! draws that could not use a buffer because the compositor still had it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::{Event as ChannelEvent, Sender, channel};
use calloop::timer::{TimeoutAction, Timer};
use calloop_wayland_source::WaylandSource;
use niri_ipc::{Event, Request, Response};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_output::Transform;
use wayland_client::protocol::{wl_output, wl_registry, wl_shm, wl_surface};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::color_management::v1::client::wp_color_management_surface_v1::WpColorManagementSurfaceV1;
use wayland_protocols::wp::color_management::v1::client::wp_color_manager_v1::{
    self, Feature, Primaries, RenderIntent, TransferFunction, WpColorManagerV1,
};
use wayland_protocols::wp::color_management::v1::client::wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1;
use wayland_protocols::wp::color_management::v1::client::wp_image_description_v1::{
    self, WpImageDescriptionV1,
};

use crate::geom::{Rotation, Size};
use crate::images::{self, FileStamp, ImageCache, needs_reload};
use crate::schedule::{FRAME_MS, TransitionState};
use crate::video::{self, DecodeFormat, Video};
use crate::{Scene, active_workspaces, hdr, resolve_at};

/// Layer-shell namespace, visible in compositor debugging tools.
pub const NAMESPACE: &str = "loom-wallpaper";
/// Colour shown while an output has no image (or its image cannot be decoded).
const FALLBACK_COLOR: [u8; 3] = [0x10, 0x10, 0x14];
/// How often image files and deferred redraws are checked.
const TICK: Duration = Duration::from_secs(1);
/// Delay before reconnecting to niri IPC or the compositor.
const RETRY: Duration = Duration::from_secs(1);
/// Buffers kept per surface: a redraw must not write into the buffer the
/// compositor may still be displaying, so a second one absorbs the redraw.
const MAX_BUFFERS: usize = 2;

/// Verbose diagnostics for a wallpaper that does not show up, enabled with
/// `LOOM_WALLPAPER_DEBUG=1`.
fn debug_log(args: std::fmt::Arguments<'_>) {
    if std::env::var_os("LOOM_WALLPAPER_DEBUG").is_some() {
        eprintln!("loom-wallpaper: {args}");
    }
}

/// Whether the session should follow niri's focus changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Render every output once and keep the surfaces mapped (like `swaybg`).
    Apply,
    /// Keep following niri events.
    Watch,
}

#[derive(Debug)]
pub enum NativeError {
    /// The compositor cannot host a native background; another backend is needed.
    Unsupported(String),
    /// The session started but could not continue (usually a lost connection).
    Failed(String),
}

impl fmt::Display for NativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(reason) => write!(f, "the native backend is unavailable: {reason}"),
            Self::Failed(reason) => write!(f, "native wallpaper session failed: {reason}"),
        }
    }
}

impl std::error::Error for NativeError {}

/// Checks whether this compositor can host the native backend without
/// rendering anything: a Wayland connection plus `wl_compositor`, `wl_shm` and
/// `zwlr_layer_shell_v1` must all exist.
pub fn probe() -> Result<(), String> {
    let conn = Connection::connect_to_env()
        .map_err(|err| format!("cannot connect to a Wayland compositor: {err}"))?;
    let (globals, mut queue) = registry_queue_init::<Probe>(&conn)
        .map_err(|err| format!("cannot list Wayland globals: {err}"))?;
    queue
        .roundtrip(&mut Probe)
        .map_err(|err| format!("Wayland roundtrip failed: {err}"))?;
    let mut missing = Vec::new();
    globals.contents().with_list(|list| {
        for interface in ["wl_compositor", "wl_shm", "zwlr_layer_shell_v1"] {
            if !list.iter().any(|global| global.interface == interface) {
                missing.push(interface);
            }
        }
    });
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("missing Wayland globals: {}", missing.join(", ")))
    }
}

struct Probe;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Probe {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: <wl_registry::WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// Runs the native backend, reconnecting after a compositor restart.
///
/// `Unsupported` is returned before anything is rendered, so callers can fall
/// back to the command backend; once a session has started, lost connections
/// are retried here instead of failing the daemon.
pub fn run(scene: &Scene, socket: Option<&Path>, mode: Mode) -> Result<(), NativeError> {
    let mut attempt = 0u32;
    loop {
        match session(scene, socket, mode) {
            Ok(()) => return Ok(()),
            Err(NativeError::Unsupported(reason)) => {
                return Err(NativeError::Unsupported(reason));
            }
            Err(NativeError::Failed(reason)) => {
                attempt += 1;
                eprintln!(
                    "loom-wallpaper: {reason}; reconnecting (attempt {attempt}) in {}s",
                    RETRY.as_secs()
                );
                std::thread::sleep(RETRY);
            }
        }
    }
}

struct Wallpaper {
    registry_state: RegistryState,
    compositor: CompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    output_state: OutputState,
    scene: Scene,
    /// niri IPC socket, used to poll per-output HDR state.
    socket: Option<PathBuf>,
    /// Workspace name that is active on each output name.
    active: BTreeMap<String, String>,
    /// Whether niri reports the output as currently signalling HDR.
    hdr: BTreeMap<String, bool>,
    /// `wp_color_manager_v1` state, `None` when HDR content is not usable.
    color: Option<Color>,
    cache: ImageCache,
    surfaces: Vec<Surface>,
    frame_sender: Sender<()>,
    frame_pending: bool,
    exit: bool,
}

/// `wp_color_manager_v1` and the shared parametric PQ/BT.2020 description.
struct Color {
    manager: WpColorManagerV1,
    /// Whether the compositor supports the exact description we need.
    perceptual: bool,
    parametric: bool,
    luminances: bool,
    pq: bool,
    bt2020: bool,
    done: bool,
    description: Option<WpImageDescriptionV1>,
    ready: bool,
}

impl Color {
    fn new(manager: WpColorManagerV1) -> Self {
        Self {
            manager,
            perceptual: false,
            parametric: false,
            luminances: false,
            pq: false,
            bt2020: false,
            done: false,
            description: None,
            ready: false,
        }
    }

    /// The compositor advertised everything the PQ/BT.2020 description needs.
    fn supported(&self) -> bool {
        self.done && self.perceptual && self.parametric && self.pq && self.bt2020
    }
}

struct Surface {
    output: wl_output::WlOutput,
    layer: LayerSurface,
    pool: SlotPool,
    buffers: Vec<Buffer>,
    /// Format of the buffers in `buffers`; a change drops them all.
    buffer_format: Option<wl_shm::Format>,
    /// Logical (surface-local) size from the last configure.
    logical: Size,
    configured: bool,
    /// Image currently committed for this surface, if any.
    current: Option<PathBuf>,
    current_buffer: Size,
    current_scale: u32,
    current_stamp: Option<FileStamp>,
    /// Pixels currently on screen (the target of the last completed draw).
    last_pixels: Option<Vec<u8>>,
    /// An active cross-fade towards `current`.
    transition: Option<TransitionState>,
    /// A redraw was requested but could not commit yet.
    pending: bool,
    /// Decoder for a video wallpaper on this output.
    video: Option<Video>,
    /// Decode format of `video`, so a format change restarts it.
    video_format: DecodeFormat,
    /// The 10-bit buffer could not be created; keep this output on SDR.
    hdr_failed: bool,
    /// The colour management object for this surface, when available.
    color_surface: Option<WpColorManagementSurfaceV1>,
    /// Whether the PQ description is currently attached (pending or committed).
    color_set: bool,
    /// Last frame committed, so a 30 Hz timer does not recopy a slower video.
    last_frame: Option<Arc<Vec<u8>>>,
}

impl Surface {
    /// Destroys the per-surface colour management object.
    fn destroy_color(&mut self) {
        if let Some(color_surface) = self.color_surface.take() {
            color_surface.destroy();
        }
        self.color_set = false;
    }
}

enum NiriEvent {
    Workspaces(BTreeMap<String, String>),
    Failed(String),
    Disconnected,
}

fn session(scene: &Scene, socket: Option<&Path>, mode: Mode) -> Result<(), NativeError> {
    let conn = Connection::connect_to_env().map_err(|err| {
        NativeError::Failed(format!("cannot connect to the Wayland compositor: {err}"))
    })?;
    let (globals, mut queue) = registry_queue_init::<Wallpaper>(&conn)
        .map_err(|err| NativeError::Failed(format!("cannot list Wayland globals: {err}")))?;
    let qh = queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|err| NativeError::Unsupported(format!("wl_compositor: {err}")))?;
    let shm = Shm::bind(&globals, &qh)
        .map_err(|err| NativeError::Unsupported(format!("wl_shm: {err}")))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|err| NativeError::Unsupported(format!("zwlr_layer_shell_v1: {err}")))?;
    let output_state = OutputState::new(&globals, &qh);
    // Version 1 keeps the original `ready` event; every request used here
    // exists since then. Absent (or refused) means HDR video stays SDR.
    let color = globals
        .bind::<WpColorManagerV1, _, _>(&qh, 1..=1, ())
        .ok()
        .map(Color::new);

    let active = match socket {
        Some(socket) => match query_active(socket) {
            Ok(active) => active,
            Err(err) => {
                eprintln!("loom-wallpaper: {err}; using the scene default for now");
                BTreeMap::new()
            }
        },
        None => BTreeMap::new(),
    };
    let hdr = match socket {
        Some(socket) => match query_hdr(socket) {
            Ok(hdr) => hdr,
            Err(err) => {
                eprintln!("loom-wallpaper: {err}; HDR video will stay on the SDR path");
                BTreeMap::new()
            }
        },
        None => BTreeMap::new(),
    };

    let (frame_sender, frame_receiver) = channel::<()>();
    let mut state = Wallpaper {
        registry_state: RegistryState::new(&globals),
        compositor,
        shm,
        layer_shell,
        output_state,
        scene: scene.clone(),
        socket: socket.map(Path::to_path_buf),
        active,
        hdr,
        color,
        cache: ImageCache::new(),
        surfaces: Vec::new(),
        frame_sender,
        frame_pending: false,
        exit: false,
    };

    queue
        .roundtrip(&mut state)
        .map_err(|err| NativeError::Failed(format!("initial roundtrip: {err}")))?;
    state.setup_color(&qh);
    // The image description becomes usable only after the compositor answers
    // with `ready`; block here, before anything is drawn, so every later commit
    // can trust `color_ready()`.
    queue
        .roundtrip(&mut state)
        .map_err(|err| NativeError::Failed(format!("colour roundtrip: {err}")))?;
    if !state.color_ready() {
        if state.color.is_some() {
            debug_log(format_args!(
                "no parametric PQ/BT.2020 image description; HDR video will be tone-mapped"
            ));
        }
        state.drop_color();
    }
    let outputs: Vec<_> = state.output_state.outputs().collect();
    for output in outputs {
        state.ensure_surface(&qh, output);
    }
    // A second roundtrip picks up output names and scales advertised through
    // xdg-output, which the first one may not have delivered yet.
    queue
        .roundtrip(&mut state)
        .map_err(|err| NativeError::Failed(format!("output roundtrip: {err}")))?;
    state.draw_all();
    debug_log(format_args!(
        "native backend running on {} output(s)",
        state.surfaces.len()
    ));

    let mut event_loop: EventLoop<Wallpaper> = EventLoop::try_new()
        .map_err(|err| NativeError::Failed(format!("cannot create event loop: {err}")))?;
    let handle = event_loop.handle();
    let signal = event_loop.get_signal();

    // Cross-fade frames are driven by a self-rearming one-shot timer: while no
    // animation runs, the loop does not wake at the refresh rate.
    let frame_loop = handle.clone();
    let frame_sender = state.frame_sender.clone();
    handle
        .insert_source(frame_receiver, move |event, _, state: &mut Wallpaper| {
            if matches!(event, ChannelEvent::Closed) {
                state.exit = true;
                return;
            }
            state.step_transitions();
            state.step_videos();
            if state.has_transitions() || state.has_videos() {
                let sender = frame_sender.clone();
                if let Err(err) = frame_loop.insert_source(
                    Timer::from_duration(Duration::from_millis(FRAME_MS)),
                    move |_, _, _| {
                        let _ = sender.send(());
                        TimeoutAction::Drop
                    },
                ) {
                    eprintln!("loom-wallpaper: cannot schedule a transition frame: {err}");
                }
            }
        })
        .map_err(|err| NativeError::Failed(format!("cannot register frame source: {err}")))?;

    handle
        .insert_source(
            Timer::from_duration(TICK),
            move |_, _, state: &mut Wallpaper| {
                state.tick();
                TimeoutAction::ToDuration(TICK)
            },
        )
        .map_err(|err| NativeError::Failed(format!("cannot register timer: {err}")))?;

    if mode == Mode::Watch {
        match socket {
            Some(socket) => {
                let (sender, receiver) = channel();
                spawn_niri_thread(socket.to_path_buf(), sender);
                handle
                    .insert_source(receiver, |event, _, state: &mut Wallpaper| {
                        state.on_niri(event);
                    })
                    .map_err(|err| {
                        NativeError::Failed(format!("cannot watch niri events: {err}"))
                    })?;
            }
            None => eprintln!(
                "loom-wallpaper: NIRI_SOCKET is not set; \
                 wallpapers will not follow workspace focus"
            ),
        }
    }

    WaylandSource::new(conn.clone(), queue)
        .insert(handle.clone())
        .map_err(|err| NativeError::Failed(format!("cannot register Wayland source: {err}")))?;

    event_loop
        .run(None, &mut state, move |state| {
            if state.exit {
                signal.stop();
            }
        })
        .map_err(|err| NativeError::Failed(format!("Wayland connection lost: {err}")))?;

    if state.exit {
        Ok(())
    } else {
        Err(NativeError::Failed("Wayland connection closed".to_owned()))
    }
}

/// Thread that follows niri's event stream, reconnecting after niri restarts.
fn spawn_niri_thread(socket: PathBuf, sender: Sender<NiriEvent>) {
    std::thread::spawn(move || {
        loop {
            match event_stream(&socket) {
                Ok(stream) => {
                    if let Ok(active) = query_active(&socket)
                        && sender.send(NiriEvent::Workspaces(active)).is_err()
                    {
                        return;
                    }
                    let mut read_event = stream.read_events();
                    loop {
                        match read_event() {
                            Ok(Event::WorkspacesChanged { workspaces }) => {
                                let active = active_workspaces(&workspaces);
                                if sender.send(NiriEvent::Workspaces(active)).is_err() {
                                    return;
                                }
                            }
                            Ok(Event::WorkspaceActivated { focused: true, .. }) => {
                                if let Ok(active) = query_active(&socket)
                                    && sender.send(NiriEvent::Workspaces(active)).is_err()
                                {
                                    return;
                                }
                            }
                            Ok(_) => {}
                            Err(err)
                                if err.kind() == std::io::ErrorKind::UnexpectedEof
                                    || err.kind() == std::io::ErrorKind::ConnectionReset =>
                            {
                                let _ = sender.send(NiriEvent::Disconnected);
                                break;
                            }
                            Err(err) => {
                                let _ = sender.send(NiriEvent::Failed(format!(
                                    "niri event stream error: {err}"
                                )));
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    if sender.send(NiriEvent::Failed(err)).is_err() {
                        return;
                    }
                }
            }
            std::thread::sleep(RETRY);
        }
    });
}

fn event_stream(socket: &Path) -> Result<niri_ipc::socket::Socket, String> {
    let mut stream = niri_ipc::socket::Socket::connect_to(socket)
        .map_err(|err| format!("cannot connect to niri IPC: {err}"))?;
    match stream
        .send(Request::EventStream)
        .map_err(|err| format!("niri IPC error: {err}"))?
    {
        Ok(Response::Handled) => Ok(stream),
        Ok(other) => Err(format!("unexpected niri response: {other:?}")),
        Err(message) => Err(format!("niri returned an error: {message}")),
    }
}

fn query_active(socket: &Path) -> Result<BTreeMap<String, String>, String> {
    match crate::send(socket, Request::Workspaces)? {
        Response::Workspaces(workspaces) => Ok(active_workspaces(&workspaces)),
        other => Err(format!("unexpected niri response: {other:?}")),
    }
}

/// Whether niri is signalling HDR on each output, keyed by output name.
fn query_hdr(socket: &Path) -> Result<BTreeMap<String, bool>, String> {
    match crate::send(socket, Request::Outputs)? {
        Response::Outputs(outputs) => Ok(outputs
            .into_iter()
            .map(|(name, output)| (name, output.hdr_active))
            .collect()),
        other => Err(format!("unexpected niri response: {other:?}")),
    }
}

impl Wallpaper {
    fn scale_of(&self, output: &wl_output::WlOutput) -> u32 {
        self.output_state
            .info(output)
            .map(|info| info.scale_factor.max(1) as u32)
            .unwrap_or(1)
    }

    fn rotation_of(&self, output: &wl_output::WlOutput) -> Rotation {
        match self.output_state.info(output).map(|info| info.transform) {
            Some(Transform::_90) | Some(Transform::Flipped90) => Rotation::Cw90,
            Some(Transform::_180) => Rotation::Cw180,
            Some(Transform::_270) | Some(Transform::Flipped270) => Rotation::Cw270,
            _ => Rotation::Normal,
        }
    }

    fn index_of(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|entry| entry.layer.wl_surface() == surface)
    }

    fn index_of_output(&self, output: &wl_output::WlOutput) -> Option<usize> {
        self.surfaces
            .iter()
            .position(|entry| entry.output == *output)
    }

    /// Whether niri currently signals HDR on this output.
    fn hdr_active(&self, output: &wl_output::WlOutput) -> bool {
        self.output_state
            .info(output)
            .and_then(|info| info.name)
            .is_some_and(|name| self.hdr.get(&name).copied().unwrap_or(false))
    }

    /// Whether a ready PQ/BT.2020 image description can be attached.
    fn color_ready(&self) -> bool {
        self.color
            .as_ref()
            .is_some_and(|color| color.supported() && color.ready && color.description.is_some())
    }

    /// Creates the shared parametric description once the manager's advertised
    /// capabilities are known. The `ready` event arrives on a later roundtrip.
    fn setup_color(&mut self, qh: &QueueHandle<Self>) {
        let Some(color) = self.color.as_mut() else {
            return;
        };
        if !color.supported() || color.description.is_some() {
            return;
        }
        let creator = color.manager.create_parametric_creator(qh, ());
        creator.set_tf_named(TransferFunction::St2084Pq);
        creator.set_primaries_named(Primaries::Bt2020);
        if color.luminances {
            creator.set_luminances(hdr::MIN_LUMINANCE, hdr::MAX_LUMINANCE, hdr::REFERENCE_WHITE);
        }
        color.description = Some(creator.create(qh, ()));
    }

    /// Drops every colour management object and falls back to SDR.
    fn drop_color(&mut self) {
        let Some(mut color) = self.color.take() else {
            return;
        };
        if let Some(description) = color.description.take() {
            description.destroy();
        }
        for surface in &mut self.surfaces {
            surface.destroy_color();
        }
        color.manager.destroy();
    }

    /// Re-reads per-output HDR state; cheap enough to run with a video active.
    fn refresh_hdr(&mut self) {
        let Some(socket) = self.socket.as_deref() else {
            return;
        };
        let Ok(hdr) = query_hdr(socket) else {
            return;
        };
        if hdr == self.hdr {
            return;
        }
        self.hdr = hdr;
        for surface in &mut self.surfaces {
            surface.pending = true;
        }
    }

    fn ensure_surface(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self.index_of_output(&output).is_some() {
            return;
        }
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Background,
            Some(NAMESPACE),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // Size 0 with all edges anchored lets the compositor hand us the output
        // size; it is reported back in the first configure.
        layer.set_size(0, 0);
        layer.commit();

        let color_surface = self
            .color
            .as_ref()
            .map(|color| color.manager.get_surface(layer.wl_surface(), qh, ()));
        let pool = match SlotPool::new(1 << 20, &self.shm) {
            Ok(pool) => pool,
            Err(err) => {
                eprintln!("loom-wallpaper: cannot create a shm pool for an output: {err}");
                return;
            }
        };
        self.surfaces.push(Surface {
            output,
            layer,
            pool,
            buffers: Vec::new(),
            buffer_format: None,
            logical: Size::default(),
            configured: false,
            current: None,
            current_buffer: Size::default(),
            current_scale: 0,
            current_stamp: None,
            last_pixels: None,
            transition: None,
            pending: false,
            video: None,
            video_format: DecodeFormat::Sdr,
            hdr_failed: false,
            color_surface,
            color_set: false,
            last_frame: None,
        });
    }

    /// Everything a draw needs, resolved from immutable state.
    fn plan(&self, index: usize) -> Option<(Size, u32, Option<PathBuf>)> {
        let surface = self.surfaces.get(index)?;
        if !surface.configured || surface.logical.is_empty() {
            return None;
        }
        let output_name = self
            .output_state
            .info(&surface.output)
            .and_then(|info| info.name);
        let workspace = output_name
            .as_deref()
            .and_then(|name| self.active.get(name))
            .map(String::as_str);
        let (now, minute) = crate::local_now();
        let path = resolve_at(&self.scene, workspace, minute, now as u64).map(PathBuf::from);
        Some((surface.logical, self.scale_of(&surface.output), path))
    }

    fn draw_all(&mut self) {
        for index in 0..self.surfaces.len() {
            let Some((logical, scale, path)) = self.plan(index) else {
                continue;
            };
            let buffer = logical.scaled(scale);
            let surface = &self.surfaces[index];
            if !surface.pending
                && surface.current == path
                && surface.current_buffer == buffer
                && surface.current_scale == scale
            {
                continue;
            }
            self.draw(index);
        }
    }

    fn draw(&mut self, index: usize) {
        let Some((logical, scale, path)) = self.plan(index) else {
            return;
        };
        let buffer = logical.scaled(scale);
        if buffer.is_empty() {
            return;
        }
        if path.as_deref().is_some_and(video::is_video) {
            let video_path = path.clone().expect("video path");
            self.ensure_video(index, &video_path, buffer);
            let format = self.surfaces[index].video_format;
            let frame = self.surfaces[index].video.as_ref().and_then(Video::frame);
            let fallback;
            let pixels: &[u8] = match frame.as_ref() {
                Some(frame) => frame,
                None => {
                    fallback = match format {
                        DecodeFormat::Hdr => hdr::render_solid(buffer, FALLBACK_COLOR),
                        DecodeFormat::Sdr => images::render_solid(buffer, FALLBACK_COLOR),
                    };
                    &fallback
                }
            };
            self.commit(index, scale, buffer, pixels, path, format);
            if let Some(surface) = self.surfaces.get_mut(index) {
                surface.transition = None;
                surface.last_pixels = None;
                surface.last_frame = frame;
                surface.current_buffer = buffer;
                surface.current_scale = scale;
                surface.pending = false;
            }
            self.schedule_frames();
            return;
        }
        // Leaving a video workspace stops its decoder.
        if let Some(surface) = self.surfaces.get_mut(index) {
            surface.video = None;
            surface.last_frame = None;
        }
        let pixels = match path.as_deref() {
            Some(path) => match self.cache.get(path) {
                Ok(image) => match images::render_cover(image, buffer) {
                    Ok(pixels) => pixels,
                    Err(err) => {
                        eprintln!("loom-wallpaper: {err}");
                        images::render_solid(buffer, FALLBACK_COLOR)
                    }
                },
                Err(err) => {
                    eprintln!("loom-wallpaper: {err}");
                    images::render_solid(buffer, FALLBACK_COLOR)
                }
            },
            None => images::render_solid(buffer, FALLBACK_COLOR),
        };
        let switching = self.surfaces[index].current != path;
        let transition = if switching {
            self.scene.transition
        } else {
            None
        };
        let previous_pixels = self.surfaces[index].last_pixels.clone();
        if let Some(spec) = transition
            && spec.duration_ms > 0
            && previous_pixels
                .as_ref()
                .is_some_and(|previous| previous.len() == pixels.len())
        {
            let from = previous_pixels.unwrap_or_default();
            if from.len() == pixels.len() {
                // Keep the old image on screen and animate towards the new one.
                let surface = &mut self.surfaces[index];
                surface.transition = Some(TransitionState::new(spec, from, pixels));
                surface.current = path;
                surface.current_buffer = buffer;
                surface.current_scale = scale;
                surface.pending = false;
                self.schedule_frames();
                return;
            }
        }
        self.commit(index, scale, buffer, &pixels, path, DecodeFormat::Sdr);
        if let Some(surface) = self.surfaces.get_mut(index) {
            surface.last_pixels = Some(pixels);
        }
    }

    fn ensure_video(&mut self, index: usize, path: &Path, size: Size) {
        let color_ready = self.color_ready();
        let ten_bit = self.shm.formats().contains(&wl_shm::Format::Xrgb2101010);
        let output = self
            .surfaces
            .get(index)
            .map(|surface| surface.output.clone());
        let hdr_active = output
            .as_ref()
            .is_some_and(|output| self.hdr_active(output));
        let Some(surface) = self.surfaces.get_mut(index) else {
            return;
        };
        let format = if surface.hdr_failed || surface.color_surface.is_none() {
            DecodeFormat::Sdr
        } else {
            video::decode_format(video::source(path), hdr_active, color_ready, ten_bit)
        };
        let matches = surface.video.as_ref().is_some_and(|video| {
            video.path() == path && video.size() == size && video.format() == format
        });
        if !matches {
            surface.video = Video::start(path, size, format);
            surface.video_format = format;
            surface.last_frame = None;
            debug_log(format_args!(
                "video wallpaper: {} at {}x{} ({format:?})",
                path.display(),
                size.width,
                size.height
            ));
        }
    }

    fn has_videos(&self) -> bool {
        self.surfaces.iter().any(|surface| surface.video.is_some())
    }

    fn step_videos(&mut self) {
        if !self.has_videos() {
            return;
        }
        for index in 0..self.surfaces.len() {
            if self.surfaces[index].transition.is_some() {
                continue;
            }
            let (buffer, scale, path, format) = {
                let surface = &self.surfaces[index];
                (
                    surface.current_buffer,
                    surface.current_scale,
                    surface.current.clone(),
                    surface.video_format,
                )
            };
            let Some(frame) = self.surfaces[index].video.as_ref().and_then(Video::frame) else {
                continue;
            };
            self.commit(index, scale, buffer, &frame, path, format);
        }
        self.schedule_frames();
    }

    fn has_transitions(&self) -> bool {
        self.surfaces
            .iter()
            .any(|surface| surface.transition.is_some())
    }

    /// Asks the event loop for a frame if one is not already scheduled.
    fn schedule_frames(&mut self) {
        if self.frame_pending {
            return;
        }
        self.frame_pending = true;
        let _ = self.frame_sender.send(());
    }

    /// Advances every active cross-fade by one frame and commits the blend.
    fn step_transitions(&mut self) {
        self.frame_pending = false;
        for index in 0..self.surfaces.len() {
            if self.surfaces[index].transition.is_none() {
                continue;
            }
            let buffer = self.surfaces[index].current_buffer;
            let scale = self.surfaces[index].current_scale;
            let path = self.surfaces[index].current.clone();
            let frame = self.surfaces[index]
                .transition
                .as_mut()
                .map(|transition| transition.advance(FRAME_MS));
            let Some(frame) = frame else {
                continue;
            };
            self.commit(index, scale, buffer, &frame, path, DecodeFormat::Sdr);
            let Some(surface) = self.surfaces.get_mut(index) else {
                continue;
            };
            if surface
                .transition
                .as_ref()
                .is_some_and(|transition| transition.done())
            {
                surface.last_pixels = surface.transition.take().map(|transition| transition.to);
            }
        }
    }

    fn commit(
        &mut self,
        index: usize,
        scale: u32,
        buffer_size: Size,
        pixels: &[u8],
        path: Option<PathBuf>,
        format: DecodeFormat,
    ) {
        let hdr = format == DecodeFormat::Hdr;
        let shm_format = if hdr {
            wl_shm::Format::Xrgb2101010
        } else {
            wl_shm::Format::Xrgb8888
        };
        let color = if hdr { self.color.as_ref() } else { None };
        let Some(surface) = self.surfaces.get_mut(index) else {
            return;
        };
        let width = buffer_size.width as i32;
        let height = buffer_size.height as i32;
        let stride = width.saturating_mul(4);

        if surface.buffer_format != Some(shm_format) {
            // Buffers of the other format cannot be attached to this one.
            surface.buffers.clear();
            surface.buffer_format = Some(shm_format);
        }

        // Reuse a buffer the compositor has released, so a redraw never
        // overwrites pixels that may still be on screen.
        let reusable = (0..surface.buffers.len()).find(|&slot| {
            let candidate = &surface.buffers[slot];
            candidate.height() == height
                && candidate.stride() == stride
                && candidate.canvas(&mut surface.pool).is_some()
        });
        let slot = match reusable {
            Some(slot) => {
                if let Some(canvas) = surface.buffers[slot].canvas(&mut surface.pool) {
                    let len = pixels.len().min(canvas.len());
                    canvas[..len].copy_from_slice(&pixels[..len]);
                }
                slot
            }
            None => {
                if surface.buffers.len() >= MAX_BUFFERS {
                    // Dropping an in-use buffer is safe: the compositor keeps
                    // the storage alive until it has released it.
                    surface.buffers.remove(0);
                }
                let created = surface
                    .pool
                    .create_buffer(width, height, stride, shm_format);
                match created {
                    Ok((buffer, canvas)) => {
                        let len = pixels.len().min(canvas.len());
                        canvas[..len].copy_from_slice(&pixels[..len]);
                        surface.buffers.push(buffer);
                        surface.buffers.len() - 1
                    }
                    Err(err) => {
                        eprintln!("loom-wallpaper: cannot create a shm buffer: {err}");
                        if hdr {
                            // Keep the last frame on screen and re-decode this
                            // output through the SDR path on the next draw.
                            surface.hdr_failed = true;
                        }
                        surface.pending = true;
                        return;
                    }
                }
            }
        };

        // The description travels with the surface commit, so switch it in the
        // same commit as the pixel format.
        if hdr {
            if !surface.color_set
                && let (Some(color), Some(color_surface)) = (color, surface.color_surface.as_ref())
                && let Some(description) = color.description.as_ref()
            {
                color_surface.set_image_description(description, RenderIntent::Perceptual);
                surface.color_set = true;
            }
        } else if surface.color_set {
            if let Some(color_surface) = surface.color_surface.as_ref() {
                color_surface.unset_image_description();
            }
            surface.color_set = false;
        }

        let wl_surface = surface.layer.wl_surface().clone();
        wl_surface.set_buffer_scale(scale as i32);
        wl_surface.damage_buffer(0, 0, width, height);
        if let Err(err) = surface.buffers[slot].attach_to(&wl_surface) {
            eprintln!("loom-wallpaper: cannot attach a shm buffer: {err}");
            surface.pending = true;
            return;
        }
        surface.layer.commit();
        surface.current_stamp = path.as_deref().and_then(FileStamp::from_path);
        debug_log(format_args!(
            "committed {}x{} buffer at scale {} ({})",
            width,
            height,
            scale,
            path.as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "scene fallback colour".to_owned())
        ));
        surface.current = path;
        surface.current_buffer = buffer_size;
        surface.current_scale = scale;
        surface.pending = false;
    }

    /// Periodic maintenance: redraw changed image files and retry deferred draws.
    fn tick(&mut self) {
        // HDR toggles are not part of niri's event stream; poll them while a
        // video is playing so a mode switch is picked up within a second.
        if self.has_videos() {
            self.refresh_hdr();
        }
        for index in 0..self.surfaces.len() {
            if self.surfaces[index]
                .video
                .as_mut()
                .is_some_and(|video| !video.alive())
            {
                self.surfaces[index].video = None;
                self.surfaces[index].pending = true;
            }
            let Some(path) = self.surfaces[index].current.clone() else {
                continue;
            };
            let current = FileStamp::from_path(&path);
            if needs_reload(
                self.surfaces[index].current_stamp.as_ref(),
                current.as_ref(),
            ) {
                self.surfaces[index].pending = true;
            }
        }
        self.draw_all();
        // Drop decoded images that no output displays any more.
        let paths: Vec<PathBuf> = self
            .surfaces
            .iter()
            .filter_map(|surface| surface.current.clone())
            .collect();
        self.cache.retain(paths);
    }

    fn on_niri(&mut self, event: ChannelEvent<NiriEvent>) {
        match event {
            ChannelEvent::Msg(NiriEvent::Workspaces(active)) => {
                if self.active != active {
                    self.active = active;
                    self.draw_all();
                }
            }
            ChannelEvent::Msg(NiriEvent::Failed(err)) => {
                eprintln!("loom-wallpaper: {err}; retrying in {}s", RETRY.as_secs());
            }
            ChannelEvent::Msg(NiriEvent::Disconnected) => {
                eprintln!(
                    "loom-wallpaper: niri event stream closed; reconnecting in {}s",
                    RETRY.as_secs()
                );
            }
            // The watcher thread only stops once this end is gone.
            ChannelEvent::Closed => self.exit = true,
        }
    }

    /// Logical size to use when a configure reports a zero size.
    fn fallback_size(&self, output: &wl_output::WlOutput) -> Size {
        let Some(info) = self.output_state.info(output) else {
            return Size::default();
        };
        if let Some((width, height)) = info.logical_size {
            return Size::new(width.max(0) as u32, height.max(0) as u32);
        }
        let scale = info.scale_factor.max(1) as u32;
        let mode = info
            .modes
            .iter()
            .find(|mode| mode.current)
            .or_else(|| info.modes.first());
        match mode {
            Some(mode) => Size::new(mode.dimensions.0 as u32, mode.dimensions.1 as u32)
                .rotated(self.rotation_of(output))
                .divided(scale),
            None => Size::default(),
        }
    }
}

impl ShmHandler for Wallpaper {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Wallpaper {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

impl OutputHandler for Wallpaper {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.ensure_surface(qh, output);
    }

    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // A scale change needs a new buffer size and buffer scale; size changes
        // from a mode switch arrive through `configure`.
        let Some(index) = self.index_of_output(&output) else {
            return;
        };
        let scale = self.scale_of(&output);
        if self.surfaces[index].current_scale != scale {
            self.surfaces[index].buffers.clear();
            self.surfaces[index].pending = true;
            self.draw(index);
        }
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(index) = self.index_of_output(&output) {
            let mut surface = self.surfaces.remove(index);
            surface.destroy_color();
        }
    }
}

impl CompositorHandler for Wallpaper {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: i32,
    ) {
        let Some(index) = self.index_of(surface) else {
            return;
        };
        let scale = self.scale_of(&self.surfaces[index].output);
        if self.surfaces[index].current_scale != scale {
            self.surfaces[index].buffers.clear();
            self.surfaces[index].pending = true;
            self.draw(index);
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _: Transform,
    ) {
        if let Some(index) = self.index_of(surface) {
            self.surfaces[index].pending = true;
            self.draw(index);
        }
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        // No frame callbacks are requested: redraws are driven by niri events,
        // the timer and configure events, never by the refresh rate.
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for Wallpaper {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // The compositor dismissed the background surface (for example because
        // its output went away): drop it. A later `new_output` recreates it.
        if let Some(index) = self.index_of(layer.wl_surface()) {
            let mut surface = self.surfaces.remove(index);
            surface.destroy_color();
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let Some(index) = self.index_of(layer.wl_surface()) else {
            return;
        };
        let output = self.surfaces[index].output.clone();
        let logical = match (configure.new_size.0, configure.new_size.1) {
            (width, height) if width > 0 && height > 0 => Size::new(width, height),
            _ => self.fallback_size(&output),
        };
        let scale = self.scale_of(&output);
        let surface = &mut self.surfaces[index];
        let changed = surface.logical != logical || !surface.configured;
        surface.logical = logical;
        surface.configured = !logical.is_empty();
        debug_log(format_args!(
            "configure {}x{} scale {scale} (changed: {changed})",
            logical.width, logical.height
        ));
        if changed {
            // Buffers sized for the old geometry cannot be reused, and a
            // repeated configure with the same size must not re-scale the image.
            surface.buffers.clear();
            surface.pending = true;
            self.draw(index);
        }
    }
}

impl Dispatch<WpColorManagerV1, ()> for Wallpaper {
    fn event(
        state: &mut Self,
        _: &WpColorManagerV1,
        event: <WpColorManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(color) = state.color.as_mut() else {
            return;
        };
        match event {
            wp_color_manager_v1::Event::SupportedIntent { render_intent } => {
                color.perceptual |= matches!(render_intent, WEnum::Value(RenderIntent::Perceptual));
            }
            wp_color_manager_v1::Event::SupportedFeature { feature } => match feature {
                WEnum::Value(Feature::Parametric) => color.parametric = true,
                WEnum::Value(Feature::SetLuminances) => color.luminances = true,
                _ => {}
            },
            wp_color_manager_v1::Event::SupportedTfNamed { tf } => {
                color.pq |= matches!(tf, WEnum::Value(TransferFunction::St2084Pq));
            }
            wp_color_manager_v1::Event::SupportedPrimariesNamed { primaries } => {
                color.bt2020 |= matches!(primaries, WEnum::Value(Primaries::Bt2020));
            }
            wp_color_manager_v1::Event::Done => color.done = true,
            _ => {}
        }
    }
}

impl Dispatch<WpImageDescriptionCreatorParamsV1, ()> for Wallpaper {
    fn event(
        _: &mut Self,
        _: &WpImageDescriptionCreatorParamsV1,
        _: <WpImageDescriptionCreatorParamsV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpImageDescriptionV1, ()> for Wallpaper {
    fn event(
        state: &mut Self,
        _: &WpImageDescriptionV1,
        event: <WpImageDescriptionV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(color) = state.color.as_mut() else {
            return;
        };
        match event {
            wp_image_description_v1::Event::Ready { .. } => color.ready = true,
            wp_image_description_v1::Event::Failed { cause, msg } => {
                eprintln!("loom-wallpaper: HDR image description rejected ({cause:?}): {msg}");
                color.ready = false;
            }
            _ => {}
        }
    }
}

impl Dispatch<WpColorManagementSurfaceV1, ()> for Wallpaper {
    fn event(
        _: &mut Self,
        _: &WpColorManagementSurfaceV1,
        _: <WpColorManagementSurfaceV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_registry!(Wallpaper);
smithay_client_toolkit::delegate_dispatch2!(Wallpaper);
