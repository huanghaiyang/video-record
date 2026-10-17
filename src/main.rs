//! Application entry point: GUI by default, CLI when a subcommand is given.

// Link as a GUI binary so that launching the application never opens a console
// window. Command-line mode re-attaches to the parent console instead, so
// `screenrec record …` started from a terminal still prints (see `console.rs`).
// Scripts should use the console build, `screenrec-cli.exe`, which shells wait
// for; this binary is for launching the application.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    // Only command-line mode needs a console. Doing this before parsing means
    // `--help`, `--version` and clap's error messages are visible too.
    if std::env::args_os().nth(1).is_some() {
        screenrec::console::attach_parent_console();
        std::process::exit(screenrec::cli::main_entry());
    }

    if let Err(err) = screenrec::gui::run() {
        let message = format!("{err:#}");
        // Tell the user first: printing may not be possible without a console,
        // and a message box always is.
        screenrec::gui::show_fatal_error(&message);
        report_error(&format!("界面启动失败：{message}"));
        std::process::exit(1);
    }
}

/// Writes to stderr without panicking.
///
/// With the GUI subsystem the stream may not be connected to anything, and
/// `eprintln!` panics when a write fails — which would replace the real error
/// message with a panic message.
fn report_error(message: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{message}");
}
