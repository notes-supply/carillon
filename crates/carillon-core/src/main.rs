#![forbid(unsafe_code)]
#![feature(must_not_suspend)]

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match carillon_core::application::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
