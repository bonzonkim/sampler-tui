use std::error::Error;
use std::process::ExitCode;

use sampler_tui::cli::{CliCommand, USAGE, parse_args_os};
use sampler_tui::{OfflineExportError, diagnostic, run_tui};

fn main() -> ExitCode {
    let command = match parse_args_os(std::env::args_os()) {
        Ok(command) => command,
        Err(_) => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let result = match command {
        CliCommand::Tui => run_tui(None),
        CliCommand::Open(directory) => run_tui(Some(directory)),
        CliCommand::Play(path) => diagnostic::play(path),
        CliCommand::Export { .. } => {
            Err(Box::new(OfflineExportError::RendererUnavailable) as Box<dyn Error>)
        }
        CliCommand::Help => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report_error(error.as_ref());
            ExitCode::FAILURE
        }
    }
}

fn report_error(error: &dyn Error) {
    eprintln!("sampler-tui: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
        eprintln!("  caused by: {cause}");
        source = cause.source();
    }
}
