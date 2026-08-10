use std::error::Error;
use std::process::ExitCode;

use sampler_tui::cli::{
    CliEntryError, CliOutcome, CliStartupFactories, USAGE, dispatch_args_os_with_startup,
};
use sampler_tui::terminal::{
    CrosstermKeyboardEnhancementOps, RatatuiTerminalLifecycle, run_tui_with_startup,
};
use sampler_tui::{MidiService, MidirBackend, diagnostic, headless_export};

fn main() -> ExitCode {
    let result = dispatch_args_os_with_startup(
        std::env::args_os(),
        CliStartupFactories::new(
            RatatuiTerminalLifecycle::default,
            || CrosstermKeyboardEnhancementOps,
            || MidiService::new(Box::new(MidirBackend)),
            sampler_tui::audio::default_audio_input_factory,
            sampler_tui::audio::open_default_audio_output,
        ),
        run_tui_with_startup,
        diagnostic::play,
        headless_export::run,
    );
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
        Err(CliEntryError::Usage(_)) => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
        Err(CliEntryError::Runtime(error)) => {
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
