mod abort;
mod ax;
mod capture;
mod cli;
mod conn;
mod doctor;
mod input;
mod ipc;
mod mask;
mod output;
mod screens;
mod transform;
mod wm;

use std::process::ExitCode;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::conn::Conn;
use crate::input::pointer::Button;
use crate::output::print_json;

fn main() -> ExitCode {
    let cli = Cli::parse();
    // abort-watch owns stdout/stdin/signals and its own exit codes.
    if matches!(cli.command, Command::AbortWatch) {
        return abort::run();
    }
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error:#}");
            if error.downcast_ref::<ax::UsageError>().is_some() {
                return ExitCode::from(2);
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::AbortWatch => unreachable!("handled in main"),
        // Session lifecycle is a no-op on wlroots-Wayland: the virtual-input and
        // screencopy protocols are stateless per-request (no portal session to
        // keep alive, unlike KWin). We report success so the JS session
        // bookkeeping proceeds. See DESIGN.md.
        Command::SessionStart { foreground } => {
            let _ = foreground;
            print_json(&serde_json::json!({ "ok": true, "session": "noop" }))?;
        }
        Command::SessionEnd => {
            println!(r#"{{"ok":true,"ended":false,"daemon":"none"}}"#);
        }
        // No session means no grant to forget: the contract's fixed answer for
        // bridges with `session_supported:false`. No side effects.
        Command::ForgetSession => {
            println!(
                r#"{{"ok":true,"ended":false,"daemon":"none","token_removed":false,"token_path":null}}"#
            );
        }

        Command::Doctor => {
            print_json(&doctor::doctor())?;
        }
        Command::AxEnable { holder_pid } => {
            print_json(&cu_atspi::blocking::enable(holder_pid)?)?;
        }
        Command::AxRestore { holder_pid } => {
            print_json(&cu_atspi::blocking::restore(holder_pid)?)?;
        }
        Command::AxFind {
            window,
            role,
            title_contains,
            all,
            include_hidden,
            max_nodes,
            max_depth,
            expect_app_id,
            expect_title,
        } => {
            let conn = Conn::connect()?;
            let opts = ax::FindOptions {
                role,
                title_contains,
                all,
                include_hidden,
                max_nodes,
                max_depth,
                expect_app_id,
                expect_title,
            };
            print_json(&ax::find(&conn, &window, opts)?)?;
        }
        Command::AxAction {
            scope,
            element_ref,
            action,
        } => {
            // set-text reads the text from stdin only (never argv, never logged).
            let action = match action {
                cli::AxActionKind::Click => cu_atspi::Action::Click,
                cli::AxActionKind::Focus => cu_atspi::Action::Focus,
                cli::AxActionKind::SetText => cu_atspi::Action::SetText(ax::read_stdin_text()?),
            };
            print_json(&cu_atspi::blocking::action(&element_ref, &action, &scope)?)?;
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
        Command::Screenshot {
            display,
            keep_window,
            mask_all,
        } => {
            let conn = Conn::connect()?;
            let mask = mask::MaskRequest::from_flags(keep_window, mask_all);
            print_json(&capture::screenshot(&conn, display.as_deref(), &mask)?)?;
        }
        Command::Zoom {
            display,
            x,
            y,
            w,
            h,
            keep_window,
            mask_all,
        } => {
            let conn = Conn::connect()?;
            let mask = mask::MaskRequest::from_flags(keep_window, mask_all);
            print_json(&capture::zoom(
                &conn,
                display.as_deref(),
                x,
                y,
                w,
                h,
                &mask,
            )?)?;
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
