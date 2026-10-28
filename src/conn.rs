//! Shared Wayland connection + registry binding.
//!
//! Every subcommand opens a fresh connection (one-shot process, no daemon -
//! except the `left-mouse-down` holder, see `input::pointer`), does an initial
//! `wl_registry` roundtrip to discover the globals the compositor advertises,
//! and binds the ones it needs. wlroots compositors (Sway / Hyprland / Niri)
//! advertise `zwlr_virtual_pointer_manager_v1`, `zwp_virtual_keyboard_manager_v1`,
//! `zwlr_screencopy_manager_v1`, a foreign-toplevel manager, `zxdg_output_manager_v1`,
//! plus the core `wl_output` / `wl_seat` / `wl_shm`.
//!
//! Pure-Rust: `wayland-client` uses its `client_rust` backend (no libwayland),
//! so this links and runs as a fully static musl binary.

use anyhow::{Context, Result, anyhow};
use wayland_client::protocol::{wl_output, wl_registry, wl_seat, wl_shm};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1;
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1;

/// A discovered `wl_output` and the metadata we collect for it.
#[derive(Debug, Clone)]
pub struct OutputEntry {
    pub wl_output: wl_output::WlOutput,
    pub registry_name: u32,
    /// Connector name from wl_output.name (e.g. `DP-1`), or xdg-output name.
    pub name: Option<String>,
    /// Integer wl_output scale.
    pub scale: i32,
    /// Logical position from xdg-output (global compositor space).
    pub logical_x: Option<i32>,
    pub logical_y: Option<i32>,
    /// Logical size from xdg-output.
    pub logical_width: Option<i32>,
    pub logical_height: Option<i32>,
    /// Physical mode size from wl_output.mode (current).
    pub mode_width: Option<i32>,
    pub mode_height: Option<i32>,
    /// Refresh rate in mHz from wl_output.mode.
    pub refresh_millihz: Option<u32>,
    /// Output transform from wl_output.geometry. The mode (and the screencopy
    /// buffer) is in untransformed buffer orientation; the logical size is not.
    pub transform: crate::transform::Transform,
}

/// A bound global: its `wl_registry` name and the version we bound at.
#[derive(Debug, Clone, Copy)]
pub struct BoundGlobal {
    pub name: u32,
    pub version: u32,
}

/// The registry sweep result: which globals the compositor advertises and at
/// what version. The bindings themselves are created lazily by each subcommand
/// from the registry (we keep the registry + names so we can bind on demand).
#[derive(Debug, Default)]
pub struct Globals {
    pub virtual_pointer: Option<BoundGlobal>,
    pub virtual_keyboard: Option<BoundGlobal>,
    pub screencopy: Option<BoundGlobal>,
    pub foreign_toplevel_wlr: Option<BoundGlobal>,
    pub foreign_toplevel_ext: Option<BoundGlobal>,
    pub xdg_output_manager: Option<BoundGlobal>,
    pub seat: Option<BoundGlobal>,
    /// How many `wl_seat` globals the compositor advertises.
    pub seat_count: usize,
    pub shm: Option<BoundGlobal>,
    /// Every advertised wl_output (name, version).
    pub outputs: Vec<BoundGlobal>,
}

/// How long [`Conn::settle_new_device`] leaves clients to bind a seat
/// capability that the new virtual device just added.
const SETTLE_MS: u64 = 50;

/// Shared connection + a live registry + the discovered globals.
pub struct Conn {
    pub conn: Connection,
    pub registry: wl_registry::WlRegistry,
    pub globals: Globals,
}

/// State for the initial registry roundtrip: just collects advertised globals.
#[derive(Default)]
pub struct RegistryState {
    pub globals: Globals,
}

impl Conn {
    /// Connect via `$WAYLAND_DISPLAY` and sweep the registry once.
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env()
            .context("failed to connect to the Wayland compositor (is WAYLAND_DISPLAY set?)")?;

        let mut queue = conn.new_event_queue::<RegistryState>();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());

        let mut state = RegistryState::default();
        // Two roundtrips: the first delivers the global advertisements, the
        // second lets any bindings we create settle. One is enough for the
        // sweep since get_registry replays all current globals immediately.
        queue
            .roundtrip(&mut state)
            .context("failed the initial registry roundtrip")?;

        Ok(Self {
            conn,
            registry,
            globals: state.globals,
        })
    }

    /// Whether the seat currently advertises `cap`. Call it before creating a
    /// virtual device, to learn whether that device is what adds the
    /// capability. Binds a throwaway `wl_seat` on a private queue; the
    /// compositor sends `capabilities` right after the bind, so one roundtrip
    /// delivers it. Any failure answers `false`, which only costs
    /// [`Conn::settle_new_device`] its sleep. So does more than one seat: the
    /// virtual pointer goes to the compositor's default seat, which need not
    /// be the one we would probe.
    pub fn seat_has(&self, cap: wl_seat::Capability) -> bool {
        if self.globals.seat_count != 1 {
            return false;
        }
        let Some(global) = self.globals.seat else {
            return false;
        };
        let mut queue = self.conn.new_event_queue::<SeatProbe>();
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = self.bind(global, 7, &qh, ());
        let mut probe = SeatProbe::default();
        let answered = queue.roundtrip(&mut probe).is_ok();
        if seat.version() >= 5 {
            seat.release();
        }
        answered && probe.caps.is_some_and(|caps| caps.contains(cap))
    }

    /// Call after creating a virtual pointer or keyboard (and, for the
    /// keyboard, uploading its keymap), before sending events. The roundtrip
    /// makes sure the compositor has processed the creation. When the seat
    /// lacked the device's capability before (`had_capability == false`), the
    /// new device is what adds it: Sway, for one, advertises pointer and
    /// keyboard only while some device provides them, so on a seat without
    /// physical input (headless, nested) clients learn of the capability only
    /// now and bind `wl_pointer` / `wl_keyboard` in response. Events sent
    /// before they have done so reach nobody, so we wait [`SETTLE_MS`] for
    /// them. A seat that already had the capability costs only the roundtrip.
    pub fn settle_new_device(&self, had_capability: bool) -> Result<()> {
        let mut queue = self.conn.new_event_queue::<()>();
        queue
            .roundtrip(&mut ())
            .context("failed the new-device roundtrip")?;
        if !had_capability {
            std::thread::sleep(std::time::Duration::from_millis(SETTLE_MS));
        }
        Ok(())
    }

    /// The list of advertised `wl_output` globals, most-recently-added last.
    pub fn output_names(&self) -> &[BoundGlobal] {
        &self.globals.outputs
    }
}

/// State for [`Conn::seat_has`]: the last `wl_seat.capabilities` received.
#[derive(Default)]
struct SeatProbe {
    caps: Option<wl_seat::Capability>,
}

impl Dispatch<wl_seat::WlSeat, ()> for SeatProbe {
    fn event(
        state: &mut Self,
        _seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            state.caps = Some(match capabilities {
                WEnum::Value(caps) => caps,
                WEnum::Unknown(raw) => wl_seat::Capability::from_bits_truncate(raw),
            });
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for RegistryState {
    fn event(
        state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            let bound = BoundGlobal { name, version };
            match interface.as_str() {
                "zwlr_virtual_pointer_manager_v1" => state.globals.virtual_pointer = Some(bound),
                "zwp_virtual_keyboard_manager_v1" => state.globals.virtual_keyboard = Some(bound),
                "zwlr_screencopy_manager_v1" => state.globals.screencopy = Some(bound),
                "zwlr_foreign_toplevel_manager_v1" => {
                    state.globals.foreign_toplevel_wlr = Some(bound)
                }
                "ext_foreign_toplevel_list_v1" => state.globals.foreign_toplevel_ext = Some(bound),
                "zxdg_output_manager_v1" => state.globals.xdg_output_manager = Some(bound),
                "wl_seat" => {
                    state.globals.seat = Some(bound);
                    state.globals.seat_count += 1;
                }
                "wl_shm" => state.globals.shm = Some(bound),
                "wl_output" => state.globals.outputs.push(bound),
                _ => {}
            }
        }
    }
}

/// Detect the running wlroots compositor from its well-known env var.
pub fn detect_compositor() -> String {
    if std::env::var_os("SWAYSOCK").is_some() {
        "sway".to_owned()
    } else if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        "hyprland".to_owned()
    } else if std::env::var_os("NIRI_SOCKET").is_some() {
        "niri".to_owned()
    } else {
        "unknown".to_owned()
    }
}

// --- Bind helpers. Each returns the proxy or an error naming the missing global.

impl Conn {
    /// Bind a fresh registry proxy of type `I` for a discovered global.
    fn bind<I, U, D>(&self, global: BoundGlobal, cap: u32, qh: &QueueHandle<D>, udata: U) -> I
    where
        I: wayland_client::Proxy + 'static,
        D: Dispatch<I, U> + 'static,
        U: Send + Sync + 'static,
    {
        let version = global.version.min(cap);
        self.registry
            .bind::<I, U, D>(global.name, version, qh, udata)
    }

    pub fn bind_virtual_pointer_manager<D>(
        &self,
        qh: &QueueHandle<D>,
    ) -> Result<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>
    where
        D: Dispatch<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, ()> + 'static,
    {
        let g = self.globals.virtual_pointer.ok_or_else(|| {
            anyhow!("compositor does not advertise zwlr_virtual_pointer_manager_v1")
        })?;
        Ok(self.bind(g, 2, qh, ()))
    }

    pub fn bind_virtual_keyboard_manager<D>(
        &self,
        qh: &QueueHandle<D>,
    ) -> Result<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>
    where
        D: Dispatch<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1, ()> + 'static,
    {
        let g = self.globals.virtual_keyboard.ok_or_else(|| {
            anyhow!("compositor does not advertise zwp_virtual_keyboard_manager_v1")
        })?;
        Ok(self.bind(g, 1, qh, ()))
    }

    pub fn bind_screencopy_manager<D>(
        &self,
        qh: &QueueHandle<D>,
    ) -> Result<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>
    where
        D: Dispatch<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, ()> + 'static,
    {
        let g = self
            .globals
            .screencopy
            .ok_or_else(|| anyhow!("compositor does not advertise zwlr_screencopy_manager_v1"))?;
        Ok(self.bind(g, 3, qh, ()))
    }

    pub fn bind_seat<D>(&self, qh: &QueueHandle<D>) -> Result<wl_seat::WlSeat>
    where
        D: Dispatch<wl_seat::WlSeat, ()> + 'static,
    {
        let g = self
            .globals
            .seat
            .ok_or_else(|| anyhow!("compositor does not advertise wl_seat"))?;
        Ok(self.bind(g, 7, qh, ()))
    }

    pub fn bind_shm<D>(&self, qh: &QueueHandle<D>) -> Result<wl_shm::WlShm>
    where
        D: Dispatch<wl_shm::WlShm, ()> + 'static,
    {
        let g = self
            .globals
            .shm
            .ok_or_else(|| anyhow!("compositor does not advertise wl_shm"))?;
        Ok(self.bind(g, 1, qh, ()))
    }

    pub fn bind_xdg_output_manager<D>(
        &self,
        qh: &QueueHandle<D>,
    ) -> Result<zxdg_output_manager_v1::ZxdgOutputManagerV1>
    where
        D: Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> + 'static,
    {
        let g = self
            .globals
            .xdg_output_manager
            .ok_or_else(|| anyhow!("compositor does not advertise zxdg_output_manager_v1"))?;
        Ok(self.bind(g, 3, qh, ()))
    }

    pub fn bind_wl_output<D>(&self, global: BoundGlobal, qh: &QueueHandle<D>) -> wl_output::WlOutput
    where
        D: Dispatch<wl_output::WlOutput, ()> + 'static,
    {
        self.bind(global, 4, qh, ())
    }
}
