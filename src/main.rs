//! Blurman puts adjustable frosted glass behind the Windows apps you choose.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod autostart;
mod glass;
mod ipc;
mod mapping;
mod rules;
mod shared;
mod target;
mod theme;
mod tray;
mod worker;

use clap::{Parser, Subcommand};
use shared::Shared;
use std::time::Duration;
use windows::Win32::System::Console::{AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

#[derive(Parser)]
#[command(name = "blurman", about = "Frosted glass for the apps you pick. Run with no command to open the window.")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Set by the Windows startup entry: run in the tray without opening the window.
    #[arg(long, hide = true)]
    startup: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Print visible apps Blurman can frost.
    List,
    /// Save a rule for an app. Opens Blurman if it is not running.
    Apply {
        process: String,
        #[arg(long, default_value_t = mapping::TRANSPARENCY_DEFAULT,
              value_parser = clap::value_parser!(u8).range(mapping::TRANSPARENCY_MIN as i64..=mapping::TRANSPARENCY_MAX as i64))]
        transparency: u8,
        #[arg(long, default_value_t = mapping::BLUR_DEFAULT,
              value_parser = clap::value_parser!(u8).range(mapping::BLUR_MIN as i64..=mapping::BLUR_MAX as i64))]
        blur: u8,
    },
    /// Take the glass off one app, or every app with --all.
    Clear {
        #[arg(required_unless_present = "all")]
        process: Option<String>,
        #[arg(long, conflicts_with = "process")]
        all: bool,
    },
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            ensure_console();
            err.exit();
        }
    };
    let Some(command) = cli.command else {
        launch(cli.startup);
        return;
    };
    ensure_console();
    if let Err(err) = run_command(command) {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run_command(command: Command) -> Result<(), String> {
    match command {
        Command::List => {
            for group in target::list_groups() {
                println!(
                    "{:<28} {:>2} window{}{}",
                    group.process,
                    group.windows,
                    if group.windows == 1 { " " } else { "s" },
                    if group.elevated { "  administrator" } else { "" }
                );
                if !group.sample_title.is_empty() {
                    println!("  {}", group.sample_title);
                }
            }
        }
        Command::Apply {
            process,
            transparency,
            blur,
        } => {
            let rule = rules::new_rule(&process, transparency, blur);
            if rule.process.is_empty() {
                return Err("Give an app name, like chrome.exe.".into());
            }
            let mut store = rules::load();
            store.paused = false;
            store.upsert(rule.clone());
            rules::save(&store)?;
            ipc::reload_or_launch()?;
            println!(
                "Frosting {} at transparency {} and blur {}.",
                rule.process, rule.transparency, rule.blur
            );
        }
        Command::Clear { process, all } => {
            let mut store = rules::load();
            match process {
                Some(process) if !all => {
                    if !store.remove(&process) {
                        println!("{} had no rule.", mapping::normalize_process(&process));
                    }
                }
                _ => store.clear(),
            }
            rules::save(&store)?;
            if !ipc::signal(ipc::msg_reload()) {
                worker::restore_leftovers();
            }
            println!("Restored.");
        }
    }
    Ok(())
}

fn launch(startup: bool) {
    if startup && ipc::find_host().is_some() {
        return;
    }
    if ipc::show_running() {
        return;
    }
    autostart::refresh();
    let shared = Shared::new();
    let worker_shared = shared.clone();
    let worker = std::thread::Builder::new()
        .name("blurman-glass".into())
        .spawn(move || worker::run(worker_shared))
        .expect("effect thread");
    let result = app::run(shared.clone(), startup);
    shared.shutdown_worker(Duration::from_secs(3));
    if shared.worker_done.load(std::sync::atomic::Ordering::SeqCst) {
        let _ = worker.join();
    }
    if let Err(err) = result {
        ensure_console();
        eprintln!("Blurman could not open its window: {err}");
        std::process::exit(1);
    }
}

fn ensure_console() {
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            let _ = AllocConsole();
        }
    }
}
