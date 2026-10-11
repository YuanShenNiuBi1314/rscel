use rscel_cli::run;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let code = run(&args);
    match code {
        0 => ExitCode::SUCCESS,
        1 => ExitCode::from(1),
        2 => ExitCode::from(2),
        3 => ExitCode::from(3),
        _ => ExitCode::from(code as u8),
    }
}
