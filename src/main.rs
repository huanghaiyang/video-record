//! Application entry point: GUI by default, CLI when a subcommand is given.

use clap::Parser;

use screenrec::cli::{self, Cli};

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(command) => {
            if let Err(err) = cli::run(command) {
                eprintln!("错误：{err:#}");
                std::process::exit(1);
            }
        }
        None => {
            if let Err(err) = screenrec::gui::run() {
                let message = format!("{err:#}");
                eprintln!("界面启动失败：{message}");
                screenrec::gui::show_fatal_error(&message);
                std::process::exit(1);
            }
        }
    }
}
