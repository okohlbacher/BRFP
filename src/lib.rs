pub mod baf;
pub mod cli;
pub mod input;
pub mod logging;
pub mod mzpeak_writer;
pub mod pipeline;
pub mod schema;
pub mod sdk;
pub mod tsf;
pub mod uv;
pub mod validation;
pub mod vendor_metadata;

use cli::{Cli, Command};
use pipeline::{BrfpError, BrfpResult, inspect_run, run_convert, validate_output};

pub fn run(cli: Cli) -> BrfpResult<()> {
    match cli.command {
        Command::Convert(args) => run_convert(args),
        Command::Inspect(args) => inspect_run(args),
        Command::Validate(args) => validate_output(args),
        Command::Query(_) => Err(BrfpError::NotImplemented(
            "ThermoRawFileParser-compatible query is planned but not implemented yet",
        )),
        Command::Xic(_) => Err(BrfpError::NotImplemented(
            "ThermoRawFileParser-compatible XIC extraction is planned but not implemented yet",
        )),
    }
}
