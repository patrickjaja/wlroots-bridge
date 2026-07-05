mod capture;
mod cli;
mod conn;
mod doctor;
mod input;
mod output;
mod screens;
mod wm;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::conn::Conn;
use crate::input::pointer::Button;
use crate::output::print_json;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        // Session lifecycle is a no-op on wlroots-Wayland: the virtual-input and
        // screencopy protocols are stateless per-request (no portal session to
        // keep alive, unlike KWin). We report success so the JS session
        // bookkeeping proceeds. See DESIGN.md.
        Command::SessionStart { foreground } => {
            let _ = foreground;
            print_json(&serde_json::json!({ "ok": true, "session": "noop" }))?;
        }
        Command::SessionEnd => {
            print_json(&serde_json::json!({ "ok": true, "ended": true }))?;
        }

        Command::Doctor => {
            let conn = Conn::connect()?;
            print_json(&doctor::doctor(&conn))?;
        }
        Command::Screens => {
            let conn = Conn::connect()?;
            print_json(&screens::screens(&conn)?)?;
        }
        Command::Windows => {
            let conn = Conn::connect()?;
            print_json(&wm::windows(&conn)?)?;
        }
        Command::CursorPosition => {
            // Not queryable on Wayland: there is no protocol to read the global
            // pointer position without owning input focus. The JS executor uses
            // Electron's getCursorScreenPoint() on this path anyway. Fail clean.
            anyhow::bail!(
                "cursor-position is not available on Wayland (no protocol exposes the global pointer)"
            );
        }
        Command::FrontmostApp => {
            let conn = Conn::connect()?;
            print_json(&wm::frontmost_app(&conn)?)?;
        }
        Command::AppUnderPoint { x, y } => {
            let conn = Conn::connect()?;
            print_json(&wm::app_under_point(&conn, x, y)?)?;
        }
        Command::ActivateWindow { window } => {
            let conn = Conn::connect()?;
            print_json(&wm::activate_window(&conn, &window)?)?;
        }
        Command::Screenshot { display } => {
            let conn = Conn::connect()?;
            print_json(&capture::screenshot(&conn, display.as_deref())?)?;
        }
        Command::Zoom {
            display,
            x,
            y,
            w,
            h,
        } => {
            let conn = Conn::connect()?;
            print_json(&capture::zoom(&conn, display.as_deref(), x, y, w, h)?)?;
        }
        Command::PointerMove { x, y } => {
            let conn = Conn::connect()?;
            print_json(&input::pointer::move_pointer(&conn, x, y)?)?;
        }
        Command::PointerClick {
            modifiers,
            x,
            y,
            button,
            count,
        } => {
            let conn = Conn::connect()?;
            let button = Button::parse(&button)?;
            print_json(&input::pointer::click(
                &conn, x, y, button, count, &modifiers,
            )?)?;
        }
        Command::PointerScroll { x, y, dx, dy } => {
            let conn = Conn::connect()?;
            print_json(&input::pointer::scroll(&conn, x, y, dx, dy)?)?;
        }
        Command::PointerDrag {
            from_x,
            from_y,
            to_x,
            to_y,
        } => {
            let conn = Conn::connect()?;
            print_json(&input::pointer::drag(&conn, from_x, from_y, to_x, to_y)?)?;
        }
        Command::LeftMouseDown => {
            let conn = Conn::connect()?;
            print_json(&input::pointer::left_mouse_down(&conn)?)?;
        }
        Command::LeftMouseUp => {
            let conn = Conn::connect()?;
            print_json(&input::pointer::left_mouse_up(&conn)?)?;
        }
        Command::KeySequence { keys, repeat } => {
            let conn = Conn::connect()?;
            print_json(&input::keyboard::key_sequence(&conn, &keys, repeat)?)?;
        }
        Command::Type { text, delay_ms } => {
            let conn = Conn::connect()?;
            print_json(&input::keyboard::type_text(&conn, &text, delay_ms)?)?;
        }
        Command::HoldKey { keys, duration_ms } => {
            let conn = Conn::connect()?;
            print_json(&input::keyboard::hold_key(&conn, &keys, duration_ms)?)?;
        }
    }

    Ok(())
}
