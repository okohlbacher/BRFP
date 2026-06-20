use std::process::ExitCode;

use brfp::{cli::Cli, run};

fn main() -> ExitCode {
    let cli = Cli::parse_compat();
    brfp::logging::init(cli.log_filter());

    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
