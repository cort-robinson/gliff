//! gliff: a window that connects to a gliff server, decodes the video, shows
//! it, and forwards keyboard and pointer input.
//!
//! The network and decode worker (`net`) and the worker's half of the
//! clipboard are shared; each platform brings its own window and GPU tier.
//! Linux uses GTK4/libadwaita (`gtk_app`) with Vulkan and VA-API; Windows
//! uses Win32 (`win`) with Direct3D 11 and Media Foundation.

mod clipboard;
mod cursor;
mod keymap;
mod net;
mod recent;

#[cfg(target_os = "linux")]
mod clipboard_ui;
#[cfg(target_os = "linux")]
mod gtk_app;
#[cfg(target_os = "linux")]
mod paintable;
#[cfg(target_os = "linux")]
mod theme;

// The pure parts of the Windows client (key tables, layout, hotkeys) also
// build for tests on Linux, so CI covers them.
#[cfg(any(windows, test))]
mod win;

#[cfg(not(any(target_os = "linux", windows)))]
compile_error!("the gliff client runs on Linux and Windows");

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}

#[cfg(target_os = "linux")]
fn main() -> gtk4::glib::ExitCode {
    init_logging();
    gtk_app::main()
}

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    init_logging();
    win::main()
}
