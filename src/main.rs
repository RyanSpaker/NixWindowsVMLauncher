pub mod session;
pub mod cli;
pub mod server;
pub mod launcher;

use std::{env::args, error::Error, fmt::Display};
use cli::{cli, CliError, Command};
use launcher::LauncherError;
use nix::unistd::Uid;
use server::ServerError;
use session::SessionError;

/// Enum representing app errors
#[derive(Debug)]
pub enum AppError{
    MalformedCommand,
    ServerNotRunAsRoot,
    ServerError(ServerError),
    SessionError(SessionError),
    LauncherError(LauncherError),
    CliError(CliError)
}
impl Display for AppError{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&match self {
            AppError::MalformedCommand => format!("Command was Malformed"),
            AppError::ServerNotRunAsRoot => format!("The Server was not run as root"),
            AppError::ServerError(err) => format!("The system server returned with err: {}", *err),
            AppError::SessionError(err) => format!("Session server returned with err: {}", *err),
            AppError::LauncherError(err) => format!("Launcher failed with err: {}", *err),
            AppError::CliError(err) => format!("The command failed with err: {}", *err)
        })?;
        Ok(())
    }
}
impl Error for AppError{}

pub async fn app() -> Result<(), AppError> {
    let arguments = args().skip(1).collect::<Vec<String>>();

    if arguments.len() == 0 {return cli(Command::Help).await.map_err(|err| AppError::CliError(err));}

    //server
    if arguments[0] == "--server" {
        // make sure we are root
        if !Uid::effective().is_root() {
            return Err(AppError::ServerNotRunAsRoot);
        }
        let server_state = server::server().await.map_err(|err| AppError::ServerError(err))?;
        let result = launcher::launcher(server_state.data.clone(), server_state.conn.clone()).await;
        let _ = server_state.conn.remove_match(server_state.signal_handle.token()).await;
        server_state.handle.abort();
        // killing is the only correct way to end the program, as it shouldnt end by itself
        return result.map_err(|err| AppError::LauncherError(err));
    }

    //session server
    if arguments[0] == "--session" {
        return session::session().await.map_err(|err| AppError::SessionError(err));
    }

    //cli
    let command = match arguments[0].as_str() {
        "--spice" => {
            if !arguments.len() == 2 {Command::Help}
            else {Command::Start(launcher::VmType::Spice, arguments[1].to_string())}
        },
        "--lg" => {
            if !arguments.len() == 2 {Command::Help}
            else {Command::Start(launcher::VmType::LookingGlass, arguments[1].to_string())}
        }
        "--open" => {Command::Open},
        "--query" => {Command::Query},
        "--shutdown" => {Command::Shutdown},
        _ => {Command::Help}
    };
    cli(command).await.map_err(|err| AppError::CliError(err))
}

/// Main function. Run server, or client commands
#[tokio::main]
async fn main() -> Result<(), AppError> {
    app().await
}
