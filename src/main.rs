//! nui-yalij: session state is authoritative; decoration is layered per viewer.
//! One coordinator hosts one single-writer server/runtime for each session.

#[cfg(not(unix))]
compile_error!("nui-yalij currently requires Unix (macOS or Linux)");

mod app;
mod cli;
mod client;
mod decoration;
mod global;
mod ipc;
mod kdl;
mod keys;
mod layout;
mod nav;
mod plugin;
mod runtime;
mod server;
mod session;
mod state;
mod store;
mod ui;

use std::{env, process::ExitCode};

fn shell() -> String {
    env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".into())
}

fn main() -> ExitCode {
    match cli::Options::parse(env::args().skip(1).collect()).and_then(cli::run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nui-yalij: {error}");
            ExitCode::FAILURE
        }
    }
}
