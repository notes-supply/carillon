use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    match carillon_discord::application::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
