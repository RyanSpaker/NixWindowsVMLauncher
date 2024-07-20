/*
    Session: 
    Test for system dbus connection
    if connceted, call UserConnected
    launch vm viewing software based on return of UserConnected
    wait for software to close
*/

use std::{error::Error, fmt::Display, fs::File, process::Stdio, time::Duration};
use dbus::nonblock::Proxy;

/// Represents all ways the session program can fail
#[derive(Debug)]
pub enum SessionError{
    FailedToConnectToSystemBus(dbus::Error),
    UnknownLaunchType(String),
    FailedToLaunchLookingGlass(std::io::Error),
    FailedToWaitOnViewer(std::io::Error),
    LookingGlassFailed,
    FailedToLaunchVirtViewer(std::io::Error),
    VirtViewerFailed,
    FailedtoCreateLogFile(std::io::Error),
    ServerError(dbus::Error)
}
impl Display for SessionError{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _ = f.write_str(&match self {
            Self::FailedToConnectToSystemBus(err) => format!("Could not connect to the system dbus: {}", *err),
            Self::FailedToLaunchLookingGlass(err) => format!("Could not launch looking-glass-client: {}", *err),
            Self::UnknownLaunchType(launch_type) => format!("The UserConnected method of org.cws.WindowsLauncher return an unknown launch type: {}", *launch_type),
            Self::FailedToWaitOnViewer(err) => format!("Asynchronously waiting on the launched viewer process failed: {}", *err),
            Self::LookingGlassFailed => format!("Looking glass returned with error"),
            Self::FailedToLaunchVirtViewer(err) => format!("Could not launch virt-viewer: {}", *err),
            Self::VirtViewerFailed => format!("virt-viewer returned with error"),
            Self::FailedtoCreateLogFile(err) => format!("Could not create the log files: {}", *err),
            Self::ServerError(err) => format!("Server return error: {}", *err)
        });
        Ok(())
    }
}
impl Error for SessionError{}

pub async fn session()->Result<(), SessionError> {
    if users::get_current_groupname().is_some_and(|name| name.eq_ignore_ascii_case("sddm")) {return Ok(());}
    let (r, conn) = dbus_tokio::connection::new_system_sync()
        .map_err(|err| SessionError::FailedToConnectToSystemBus(err))?;
    let handle = tokio::spawn(r);
    let proxy = Proxy::new("org.cws.WindowsLauncher", "/org/cws/WindowsLauncher", Duration::from_secs(30), conn.clone());
    let launch_type = match proxy.method_call::<(String,), _, _, _>("org.cws.WindowsLauncher.Manager", "UserConnected", ()).await {
        Err(err) => {
            return Err(SessionError::ServerError(err));
        },
        Ok((launch_type,)) => {
            if launch_type == "" {
                println!("Got empty launch type, vm is not running");
                return Ok(());
            }
            launch_type
        }
    };
    println!("Got vm type of: {}", launch_type);
    let log_file = File::create(format!("/var/log/windows/viewer/log-{}.txt", chrono::Local::now().to_string()))
        .map_err(|err| SessionError::FailedtoCreateLogFile(err))?;
    let log = Stdio::from(log_file.try_clone().map_err(|err| SessionError::FailedtoCreateLogFile(err))?);
    let log_err = Stdio::from(log_file);
    if launch_type == "LG" {
        launch_lg(log, log_err).await?;
    }else if launch_type == "Spice" {
        launch_spice(log, log_err).await?;
    }else {
        return Err(SessionError::UnknownLaunchType(launch_type));
    }
    handle.abort();
    Ok(())
}

pub async fn launch_lg(log: Stdio, log_err: Stdio) -> Result<(), SessionError> {
    let status = tokio::process::Command::new("looking-glass-client")
        .args(["-T", "-s", "input:captureOnFocus"])
        .stdout(log).stderr(log_err).spawn()
        .map_err(|err| SessionError::FailedToLaunchLookingGlass(err))?
        .wait().await.map_err(|err| SessionError::FailedToWaitOnViewer(err))?;
    if !status.success() {return Err(SessionError::LookingGlassFailed);}
    Ok(())
}

pub async fn launch_spice(log: Stdio, log_err: Stdio) -> Result<(), SessionError> {
    let status = tokio::process::Command::new("virt-viewer")
        .args(["--connect", "qemu:///system", "windows"])
        .stdout(log).stderr(log_err).spawn()
        .map_err(|err| SessionError::FailedToLaunchVirtViewer(err))?
        .wait().await.map_err(|err| SessionError::FailedToWaitOnViewer(err))?;
    if !status.success() {return Err(SessionError::VirtViewerFailed);}
    Ok(())
}