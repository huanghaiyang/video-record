//! Console build of the command line interface.
//!
//! `screenrec.exe` is linked as a Windows GUI binary so that launching the
//! application never opens a console window — but that also means a shell does
//! not wait for it, and its output cannot be captured reliably. This build is a
//! normal console application instead, which is what scripts should call:
//!
//! ```text
//! screenrec-cli record --monitor 1 --duration 10
//! screenrec-cli history --json
//! ```
//!
//! Both builds accept exactly the same arguments.

fn main() {
    std::process::exit(screenrec::cli::main_entry());
}
