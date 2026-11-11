//! Client bindings for the vendored Hyprland protocols in `protocols/`
//! (hyprland-protocols v0.8.0, BSD-3-Clause, copyright notices in the XML).
//!
//! - `hyprland_toplevel_export_v1`: render one window into a client buffer
//!   (`capture_toplevel` by window address, v2 `capture_toplevel_with_wlr_toplevel_handle`).
//! - `hyprland_toplevel_mapping_v1`: the window address of a foreign-toplevel
//!   handle (wlr or ext), the same address `hyprctl -j clients` reports.

#[allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
#[allow(non_upper_case_globals, non_snake_case, unused_imports)]
#[allow(missing_docs, clippy::all, clippy::pedantic)]
pub mod hyprland_toplevel_export {
    use wayland_client;
    use wayland_client::protocol::*;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::*;

    pub mod __interfaces {
        // The generated interface tables name `wayland_backend`; it is
        // wayland-client's re-export, no extra dependency.
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols_wlr::foreign_toplevel::v1::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/hyprland-toplevel-export-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/hyprland-toplevel-export-v1.xml");
}

#[allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
#[allow(non_upper_case_globals, non_snake_case, unused_imports)]
#[allow(missing_docs, clippy::all, clippy::pedantic)]
pub mod hyprland_toplevel_mapping {
    use wayland_client;
    use wayland_client::protocol::*;
    use wayland_protocols::ext::foreign_toplevel_list::v1::client::*;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::*;

    pub mod __interfaces {
        // The generated interface tables name `wayland_backend`; it is
        // wayland-client's re-export, no extra dependency.
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols::ext::foreign_toplevel_list::v1::client::__interfaces::*;
        use wayland_protocols_wlr::foreign_toplevel::v1::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/hyprland-toplevel-mapping-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/hyprland-toplevel-mapping-v1.xml");
}
