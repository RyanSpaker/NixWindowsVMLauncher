pub mod dbus_server;
pub mod virtual_mouse;
pub mod system_setup;
pub mod servant;
pub mod master;

use std::{env::args, ffi::OsStr, process::{Output, Stdio}};
use dbus_server::DBusError;
use nix::unistd::Uid;
use servant::ServantError;
use system_setup::SetupError;
use virtual_mouse::MouseError;

/// Represents different types of launches for the app
#[derive(Debug, Default, Clone, Copy)]
pub enum LaunchConfig{
    Servant,
    Spice,
    LG,
    #[default] Help
}
impl LaunchConfig{
    pub fn dc_gpu(&self) -> bool {match self {LaunchConfig::LG => true, _ => false}}
}

/// Errors returned by the application
#[derive(Debug)]
pub enum AppError{
    AppWasNotRunAsRoot,
    ServantError(ServantError),
    BusError(DBusError),
    SetupError(SetupError),
    MouseError(MouseError),
    VMLaunchFailed(String),
    FailedToLaunchVM(std::io::Error),
    FailedToQueryVMState(std::io::Error),
    FailedToShutdownVM(std::io::Error)
}
impl std::fmt::Display for AppError{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let string = match self{
            AppError::AppWasNotRunAsRoot => format!("App must be run as Root"),
            AppError::ServantError(err) => err.to_string(),
            AppError::BusError(err) => err.to_string(),
            AppError::SetupError(err) => err.to_string(),
            AppError::MouseError(err) => err.to_string(),
            AppError::VMLaunchFailed(output) => format!("Launching the vm failed with stderr: {}", output),
            AppError::FailedToLaunchVM(err) => format!("Could not launch vm: {}", err),
            AppError::FailedToQueryVMState(err) => format!("Could not query the vm state with virsh: {}", err),
            AppError::FailedToShutdownVM(err) => format!("Could not shutdown the vm with virsh: {}", err)
        };
        f.write_str(string.as_str())
    }
}
impl std::error::Error for AppError{}

// Calls the command with the args
pub fn call_command<I, S>(command: &str, args: I) -> std::io::Result<Output>
where I: IntoIterator<Item = S>, S: AsRef<OsStr> {
    std::process::Command::new(command).args(args).stderr(Stdio::null()).stdout(Stdio::piped()).output()
}

#[derive(Debug, Default)]
pub struct SystemState{
    pub launch_type: LaunchConfig,
    pub mouse_path: String,
    pub dp_stopped: bool,
    pub pw_stopped: bool,
    pub nvidia_unloaded: (bool, bool, bool, bool),
    pub gpu_disconnected: (bool, bool),
    pub vfio_loaded: bool,
    pub cpus_limited: (bool, bool, bool),
    pub power_rule_set: bool,
    pub dp_reset: bool,
    pub pw_reset: bool
}
impl SystemState{
    pub fn new(launch_type: LaunchConfig, mouse_path: String) -> Self{
        let mut s = Self::default(); s.launch_type = launch_type; s.mouse_path = mouse_path; s
    }
}

#[tokio::main]
async fn main() -> Result<(), AppError>{
    let arguments = args().skip(1).collect::<Vec<String>>();
    let config = match arguments.get(0).map(|arg| arg.as_str()) {
        Some("--client-daemon") => LaunchConfig::Servant,
        Some("-l") => LaunchConfig::LG,
        Some("-s") => LaunchConfig::Spice,
        _ => LaunchConfig::Help
    };
    return match config {
        LaunchConfig::Help => {print_help(); Ok(())},
        LaunchConfig::Servant => {servant::client_app().await.map_err(|err| AppError::ServantError(err))},
        default => {
            if arguments.len() < 2 {print_help(); return Ok(());}
            root_app(default, arguments[1].to_owned()).await
        }
    };
}
fn print_help() {
    println!("-l for full gpu passthrough, -s for spice, --client-daemon for servant program (not for normal use). Must specify mouse input path")
}
async fn root_app(vm_type: LaunchConfig, mouse_path: String) -> Result<(), AppError> {
    if !Uid::effective().is_root() {
        return Err(AppError::AppWasNotRunAsRoot);
    }
    // Connect to the dbus
    let mut dbus_state = dbus_server::connect_dbus().map_err(|err| AppError::BusError(err))?;
    dbus_state.create_dbus_service(vm_type.clone()).await.map_err(|err| AppError::BusError(err))?;
    let mut system_state = SystemState::new(vm_type, mouse_path);
    println!("Starting Setup");
    // Run VM
    let local = tokio::task::LocalSet::new();
    if let Err(err) = local.run_until(master::master(&mut dbus_state, &mut system_state)).await {
        println!("Starting Cleanup");
        system_setup::cleanup(dbus_state, system_state).await;
        return Err(err);
    }
    println!("Finished");
    Ok(())
}
