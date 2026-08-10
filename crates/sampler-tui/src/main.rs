use std::error::Error;
use std::process::ExitCode;

use sampler_tui::cli::{CliOutcome, USAGE, dispatch_command, parse_args_os};
use sampler_tui::{diagnostic, headless_export, run_tui};

fn main() -> ExitCode {
    let command = match parse_args_os(std::env::args_os()) {
        Ok(command) => command,
        Err(_) => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let result = dispatch_command(command, run_tui, diagnostic::play, headless_export::run);
    match result {
        Ok(CliOutcome::Silent) => ExitCode::SUCCESS,
        Ok(CliOutcome::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(CliOutcome::Export(receipt)) => {
            println!(
                "exported {} pattern={} rate={} frames={} revision={}",
                receipt.destination.display(),
                receipt.slot.get() + 1,
                receipt.sample_rate,
                receipt.rendered_frames,
                receipt.revision
            );
            ExitCode::SUCCESS
        }
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
