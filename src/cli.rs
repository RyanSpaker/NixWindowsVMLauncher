/*
    allows interaction with the vm launcher servers with easy to call commands
*/
use std::{error::Error, fmt::Display, sync::Arc, time::Duration};
use dbus::{nonblock::{Proxy, SyncConnection}, Path};
use dbus_tokio::connection::IOResourceError;
use tokio::task::JoinHandle;
use crate::LaunchConfig;

/// all operations supported on the command line
pub enum Command{
    Start(LaunchConfig, String),
    Open,
    Shutdown,
    Query,
    Help
}

/// Represents all ways the cli program can fail
#[derive(Debug)]
pub enum CliError{
    FailedToConnectToSystemBus(dbus::Error),
    FailedToStartUserService(dbus::Error),
    FailedToQueryState(dbus::Error),
    FailedToCallShutdown(dbus::Error),
    FailedToLaunchLG(dbus::Error),
    FailedToLaunchSpice(dbus::Error),
    FailedToConnectToSessionBus(dbus::Error)
}
impl Display for CliError{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _ = f.write_str(&match self {
            Self::FailedToConnectToSystemBus(err) => format!("Could not connect to the system dbus: {}", *err),
            Self::FailedToConnectToSessionBus(err) => format!("Could not connect to the session dbus: {}", *err),
            Self::FailedToStartUserService(err) => format!("DBus session call to start the user windows-launcher.service failed: {}", *err),
            Self::FailedToQueryState(err) => format!("Failed to query the system server for the vm state: {}", *err),
            Self::FailedToCallShutdown(err) => format!("Failed to call shutdown on the system server: {}", *err),
            Self::FailedToLaunchLG(err) => format!("Failed to call LaunchLG on the system server: {}", *err),
            Self::FailedToLaunchSpice(err) => format!("Failed to call LaunchSpice on the system server: {}", *err),
        });
        Ok(())
    }
}
impl Error for CliError{}


pub async fn cli(command: Command) -> Result<(), CliError> {
    match command{
        Command::Start(LaunchConfig::LG, path) => start_lg(path).await,
        Command::Start(LaunchConfig::Spice, path) => start_spice(path).await,
        Command::Open => open().await,
        Command::Query => query().await,
        Command::Shutdown => shutdown().await,
        Command::Help => help().await
    }
}
// start the looking glass windows vm
pub async fn start_lg(path: String) -> Result<(), CliError> {
    let (conn, h) = get_system_conn()?;
    let proxy = Proxy::new("org.cws.WindowsLauncher", "/org/cws/WindowsLauncher", Duration::from_secs(2), conn.clone());
    let _: () = proxy.method_call("org.cws.WindowsLauncher.Manager", "LaunchLG", (path,)).await.map_err(|err| CliError::FailedToLaunchLG(err))?;
    h.abort();
    Ok(())
}
// start the spice windows vm
pub async fn start_spice(path: String) -> Result<(), CliError> {
    let (conn, h) = get_system_conn()?;
    let proxy = Proxy::new("org.cws.WindowsLauncher", "/org/cws/WindowsLauncher", Duration::from_secs(2), conn.clone());
    let _: () = proxy.method_call("org.cws.WindowsLauncher.Manager", "LaunchSpice", (path,)).await.map_err(|err| CliError::FailedToLaunchSpice(err))?;
    h.abort();
    open().await?;
    Ok(())
}
// start the user session
pub async fn open() -> Result<(), CliError> {
    let (conn, h) = get_session_conn()?;
    let proxy = Proxy::new("org.freedesktop.systemd1", "/org/freedesktop.systemd1", Duration::from_secs(2), conn.clone());
    let _: (Path,) = proxy.method_call("org.freedesktop.systemd1.Manager", "StartUnit", ("windows-launcher.service", "replace")).await
        .map_err(|err| CliError::FailedToStartUserService(err))?;
    h.abort();
    Ok(())
}
// query the state of the vm
pub async fn query() -> Result<(), CliError> {
    let (conn, h) = get_system_conn()?;
    let proxy = Proxy::new("org.cws.WindowsLauncher", "/org/cws/WindowsLauncher", Duration::from_secs(2), conn.clone());
    let (state, t): (String, String) = proxy.method_call("org.cws.WindowsLauncher.Manager", "Query", ()).await
        .map_err(|err| CliError::FailedToQueryState(err))?;
    println!("VM State: {}", state);
    println!("VM Type: {}", t);
    h.abort();
    Ok(())
}
// shutdown the vm
pub async fn shutdown() -> Result<(), CliError> {
    let (conn, h) = get_system_conn()?;
    let proxy = Proxy::new("org.cws.WindowsLauncher", "/org/cws/WindowsLauncher", Duration::from_secs(2), conn.clone());
    let _: () = proxy.method_call("org.cws.WindowsLauncher.Manager", "Shutdown", ()).await
        .map_err(|err| CliError::FailedToCallShutdown(err))?;
    h.abort();
    Ok(())
}
// print a help message
pub async fn help() -> Result<(), CliError> {
    println!("This is the windows vm launcher command line tool");
    println!("Usage:");
    println!("--server: starts the system server, used as a start command for a systemd service");
    println!("--session: start the session server, used as a start command foir a systemd user service");
    println!("--spice: starts the spice vm by starting the spice system service, and then the user service. requires mouse evdev path as second arg");
    println!("--lg: start the looking glass vm by starting the looking glass systemd service. requires mouse evdev path as second arg");
    println!("--open: starts the user session service to open the correct vm viewer");
    println!("--query: returns the state of the vm");
    println!("--shutdown: stops the vm");
    println!("--help: shows this help message");
    Ok(())
}

pub fn get_system_conn() -> Result<(Arc<SyncConnection>, JoinHandle<IOResourceError>), CliError>{
    let (r, conn) = dbus_tokio::connection::new_system_sync().map_err(|err| CliError::FailedToConnectToSystemBus(err))?;
    let handle = tokio::spawn(r);
    return Ok((conn, handle));
}

pub fn get_session_conn() -> Result<(Arc<SyncConnection>, JoinHandle<IOResourceError>), CliError>{
    let (r, conn) = dbus_tokio::connection::new_session_sync().map_err(|err| CliError::FailedToConnectToSessionBus(err))?;
    let handle = tokio::spawn(r);
    return Ok((conn, handle));
}

