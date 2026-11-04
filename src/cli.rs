use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "wlroots-bridge",
    version,
    about = "wlroots-Wayland support bridge for Linux computer-use tooling."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// `ax-action --action` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AxActionKind {
    Click,
    Focus,
    /// Text comes from stdin (UTF-8, whole stdin, no trimming).
    SetText,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Report Wayland environment, detected compositor, and advertised globals.
    Doctor,
    /// Enumerate screens (wl_output + xdg-output).
    Screens,
    /// Enumerate windows (foreign-toplevel).
    Windows,
    /// Report the current global cursor position (unsupported on Wayland).
    CursorPosition,
    /// Report the currently active (frontmost) app.
    FrontmostApp,
    /// Report the topmost app at a global point (best-effort; unsupported).
    AppUnderPoint {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
    },
    /// Activate (raise + focus) a window by id.
    ActivateWindow {
        #[arg(long)]
        window: String,
    },
    /// Capture a full monitor and return an executor-style screenshot.
    Screenshot {
        #[arg(long)]
        display: Option<String>,
        /// Mask mode: keep this window (an id from `windows`; repeatable) and
        /// paint everything else #1E1E1E. sway and Hyprland only; fails closed.
        #[arg(long = "keep-window", value_name = "ID")]
        keep_window: Vec<String>,
        /// Mask mode with nothing kept: the whole image is #1E1E1E.
        #[arg(long, conflicts_with = "keep_window")]
        mask_all: bool,
    },
    /// Capture a logical region within a display and return an executor-style zoom image.
    Zoom {
        #[arg(long)]
        display: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long)]
        w: i32,
        #[arg(long)]
        h: i32,
        /// Mask mode: keep this window (an id from `windows`; repeatable) and
        /// paint everything else #1E1E1E. sway and Hyprland only; fails closed.
        #[arg(long = "keep-window", value_name = "ID")]
        keep_window: Vec<String>,
        /// Mask mode with nothing kept: the whole image is #1E1E1E.
        #[arg(long, conflicts_with = "keep_window")]
        mask_all: bool,
    },
    /// Move the pointer to a global point.
    PointerMove {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
    },
    /// Click at a global point.
    PointerClick {
        #[arg(long = "modifier")]
        modifiers: Vec<String>,
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long, default_value = "left")]
        button: String,
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    /// Scroll at a global point.
    PointerScroll {
        #[arg(long, allow_hyphen_values = true)]
        x: i32,
        #[arg(long, allow_hyphen_values = true)]
        y: i32,
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        dx: f64,
        #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
        dy: f64,
    },
    /// Drag between two global points with the left button.
    PointerDrag {
        #[arg(long, allow_hyphen_values = true)]
        from_x: i32,
        #[arg(long, allow_hyphen_values = true)]
        from_y: i32,
        #[arg(long, allow_hyphen_values = true)]
        to_x: i32,
        #[arg(long, allow_hyphen_values = true)]
        to_y: i32,
    },
    /// Press and hold the left mouse button until explicitly released.
    LeftMouseDown,
    /// Release the left mouse button if it is currently held.
    LeftMouseUp,
    /// Send an executor-style key sequence such as `ctrl+c`.
    KeySequence {
        #[arg(long)]
        keys: String,
        #[arg(long)]
        repeat: Option<u32>,
    },
    /// Type text as individual key events.
    Type {
        #[arg(long)]
        text: String,
        #[arg(long, default_value_t = 12)]
        delay_ms: u64,
    },
    /// Hold one or more keys for a fixed duration in milliseconds.
    HoldKey {
        #[arg(long = "key", required = true)]
        keys: Vec<String>,
        #[arg(long)]
        duration_ms: u64,
    },
    /// Begin a session lock. On Wayland this is a no-op that reports success.
    SessionStart {
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },
    /// End a session lock. On Wayland there is no daemon: reports `ended:false`.
    SessionEnd,
    /// Forget the remembered screen-sharing grant. Nothing to forget on wlroots.
    ForgetSession,
    /// Turn accessibility on for the Claude process (leased; screen readers untouched).
    AxEnable {
        #[arg(long)]
        holder_pid: u32,
    },
    /// Release the accessibility lease of a holder.
    AxRestore {
        #[arg(long)]
        holder_pid: u32,
    },
    /// List a window's accessible elements (AT-SPI).
    AxFind {
        #[arg(long)]
        window: String,
        #[arg(long)]
        role: Option<String>,
        #[arg(long)]
        title_contains: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        include_hidden: bool,
        #[arg(long)]
        max_nodes: Option<usize>,
        #[arg(long)]
        max_depth: Option<u32>,
        /// app id the caller listed for this window; a mismatch yields `window_changed`.
        #[arg(long)]
        expect_app_id: Option<String>,
        /// title the caller listed for this window; a mismatch yields `window_changed`.
        #[arg(long)]
        expect_title: Option<String>,
    },
    /// Act on an element returned by `ax-find` (click, focus, set-text via stdin).
    AxAction {
        #[arg(long)]
        scope: String,
        #[arg(long = "ref")]
        element_ref: String,
        #[arg(long, value_enum)]
        action: AxActionKind,
    },
}
