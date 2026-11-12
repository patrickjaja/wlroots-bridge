use clap::{Parser, Subcommand};

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

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Report Wayland environment, detected compositor, and advertised globals.
    Doctor,
    /// Long-running Escape watcher (optional contract addition): JSON lines on stdout,
    /// `suspend` / `resume` on stdin. sway and Hyprland only.
    AbortWatch,
    /// Long-running Computer Use overlay (optional contract addition): JSON lines,
    /// commands on stdin, events on stdout. Needs wlr-layer-shell.
    ///
    /// Draws Claude's glow and drawn cursor on a click-through wlr-layer-shell
    /// surface (namespace `claude-cu-overlay`), cu-overlay protocol 1. stdout
    /// carries protocol lines only; diagnostics go to stderr. Exit 0 on stdin
    /// EOF or the parent's death (CLAUDE_CU_PARENT_PID, else the direct
    /// parent), 1 when the display or stdout is lost, 2 without layer-shell or
    /// when `show` names an unknown output.
    Overlay,
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
        /// app id the caller listed for this window; a mismatch yields `window_changed`.
        #[arg(long)]
        expect_app_id: Option<String>,
        /// title the caller listed for this window; a mismatch yields `window_changed`
        /// (not when `--window-ref` was used and `--expect-app-id` matches).
        #[arg(long)]
        expect_title: Option<String>,
        #[command(flatten)]
        ax: cu_atspi::cli::AxFindFlags,
    },
    /// Act on an element returned by `ax-find` (click, focus, set-text, ensure-*,
    /// scroll-to, set-value; text and expectations via stdin).
    AxAction {
        #[command(flatten)]
        ax: cu_atspi::cli::AxActionFlags,
    },
    /// Print the accessibility features this build implements (JSON). Opens no
    /// compositor, D-Bus or session connection.
    AxFeatures,
}

#[cfg(test)]
mod tests {
    use super::*;
    use cu_atspi::Action;

    fn parse(args: &[&str]) -> Result<Command, clap::Error> {
        Cli::try_parse_from(std::iter::once("wlroots-bridge").chain(args.iter().copied()))
            .map(|c| c.command)
    }

    fn usage(args: &[&str]) {
        assert_eq!(parse(args).unwrap_err().exit_code(), 2, "{args:?}");
    }

    #[test]
    fn ax_find_old_flag_sets_parse() {
        let Command::AxFind {
            window,
            expect_app_id,
            expect_title,
            ax,
        } = parse(&[
            "ax-find",
            "--window",
            "7",
            "--role",
            "AXButton",
            "--title-contains",
            "ok",
            "--all",
            "--include-hidden",
            "--max-nodes",
            "50",
            "--max-depth",
            "6",
            "--expect-app-id",
            "foot",
            "--expect-title",
            "t",
        ])
        .unwrap()
        else {
            panic!("not ax-find")
        };
        assert_eq!(window, "7");
        assert_eq!(expect_app_id.as_deref(), Some("foot"));
        assert_eq!(expect_title.as_deref(), Some("t"));
        assert_eq!(ax.role.as_deref(), Some("AXButton"));
        assert!(ax.all && ax.include_hidden);
        assert_eq!((ax.max_nodes, ax.max_depth), (Some(50), Some(6)));
        assert!(parse(&["ax-find", "--window", "7"]).is_ok());
    }

    #[test]
    fn ax_find_new_flags_and_usage_errors() {
        let Command::AxFind { ax, .. } = parse(&[
            "ax-find",
            "--window",
            "1",
            "--window-ref",
            "r",
            "--deadline-ms",
            "300",
            "--root-ref",
            "x",
            "--scope",
            "s",
            "--skeleton",
        ])
        .unwrap() else {
            panic!()
        };
        assert!(ax.skeleton);
        assert_eq!(ax.window_ref.as_deref(), Some("r"));
        usage(&["ax-find", "--window", "1", "--root-ref", "x"]);
        usage(&[
            "ax-find",
            "--window",
            "1",
            "--skeleton",
            "--role",
            "AXButton",
        ]);
        usage(&["ax-find"]);
    }

    #[test]
    fn ax_action_kinds_parse() {
        for (arg, ok) in [
            ("click", true),
            ("focus", true),
            ("set-text", true),
            ("ensure-checked", true),
            ("scroll-to", true),
            ("set-value", true),
            ("bogus", false),
        ] {
            let r = parse(&["ax-action", "--scope", "s", "--ref", "r", "--action", arg]);
            assert_eq!(r.is_ok(), ok, "{arg}");
        }
        let Command::AxAction { ax } = parse(&[
            "ax-action",
            "--scope",
            "s",
            "--ref",
            "r",
            "--action",
            "scroll-to",
        ])
        .unwrap() else {
            panic!()
        };
        assert_eq!(
            ax.request(false, &b""[..]).unwrap().action,
            Action::ScrollTo
        );
        let Command::AxAction { ax } = parse(&[
            "ax-action",
            "--scope",
            "s",
            "--ref",
            "r",
            "--action",
            "set-value",
        ])
        .unwrap() else {
            panic!()
        };
        assert!(ax.request(false, &b"1"[..]).unwrap_err().is_usage());
    }

    #[test]
    fn ax_features_takes_no_flags() {
        assert!(matches!(parse(&["ax-features"]), Ok(Command::AxFeatures)));
        usage(&["ax-features", "--x"]);
    }
}
