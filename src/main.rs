// A GUI-subsystem binary: no console window.
#![windows_subsystem = "windows"]

fn main() {
    std::process::exit(clip4::win::app::run());
}
