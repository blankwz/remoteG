//! rd-agent — Windows 被控端（T-001 脚手架空壳，功能自 T-006 起）。

mod config;
mod server;
mod session;
mod term_handler;

#[cfg(windows)]
mod input_handler;
#[cfg(windows)]
mod spike;
#[cfg(windows)]
mod tray;
#[cfg(windows)]
mod window_session;

fn main() {
    println!("rd-agent scaffold (T-001)");
}
