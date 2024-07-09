pub mod dbus_manager;
pub mod virtual_mouse;
pub mod system_setup;
pub mod session;

use dbus_manager::DBusError;
use session::{SessionError, Sessions};
use system_setup::{create_xml, gpu::{GpuSetupError, GpuSetupState}, revert_performance_enhancements, PerformanceState, SetupError};
use virtual_mouse::{MouseError, MouseState};
use std::env::args;
use nix::unistd::Uid;


/// Represents different types of launches for the app
#[derive(Debug, Default, Clone, Copy)]
pub enum LaunchConfig{
    Spice,
    LG,
    #[default] Help
}
impl LaunchConfig{
    pub fn requires_gpu_dc(&self) -> bool {match self {LaunchConfig::LG => true, _ => false}}
}

/// Errors returned by the application
#[derive(Debug)]
pub enum AppError{
    AppWasNotRunAsRoot,
    DBusError(DBusError),
    FailedToDCGpu(GpuSetupError),
    FailedToCreateSessionHandler(SessionError),
    FailedToDoPerformanceEnhancements(SetupError),
    FailedToCreateVirtualMouse(MouseError),
    FailedToSetupMouseSessionHandler(MouseError),
    FailedToCreateXml(SetupError),
    FailedToLaunchVm(SetupError),
    FailedToCreateViewAppHandler(SessionError)
}
impl std::fmt::Display for AppError{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let string = match self{
            AppError::AppWasNotRunAsRoot => format!("App must be run as Root"),
            AppError::DBusError(err) => format!("Failed to create dbus connection: {}", err.to_string()),
            AppError::FailedToDCGpu(err) => format!("Could not dc gpu: {}", err.to_string()),
            AppError::FailedToCreateSessionHandler(err) => format!("Could not create the session handler: {}", err.to_string()),
            AppError::FailedToDoPerformanceEnhancements(err) => format!("Could not do the performance enhancements: {}", err.to_string()),
            AppError::FailedToCreateVirtualMouse(err) => format!("Creating virtual mosue failed: {}", err.to_string()),
            AppError::FailedToSetupMouseSessionHandler(err) => format!("Could not setup mouse session handler: {}", err.to_string()),
            AppError::FailedToCreateXml(err) => format!("Could not create vm xml file: {}", err.to_string()),
            AppError::FailedToLaunchVm(err) => format!("Could not launch the vm: {}", err.to_string()),
            AppError::FailedToCreateViewAppHandler(err) => format!("Creating handler for launching vm viewer failed: {}", err.to_string())
        };
        f.write_str(string.as_str())
    }
}
impl std::error::Error for AppError{}

#[tokio::main]
async fn main() -> Result<(), AppError>{
    let arguments = args().skip(1).collect::<Vec<String>>();
    let config = match arguments.get(0).map(|arg| arg.as_str()) {
        Some("-l") => LaunchConfig::LG,
        Some("-s") => LaunchConfig::Spice,
        _ => LaunchConfig::Help
    };
    return match config {
        LaunchConfig::Help => {print_help(); Ok(())},
        default => {
            if arguments.len() < 2 {print_help(); return Ok(());}
            if !Uid::effective().is_root() {
                return Err(AppError::AppWasNotRunAsRoot);
            }
            let mut ss = SystemState::new().map_err(|err| AppError::DBusError(err))?;
            let result = root_app(&mut ss, default, arguments[1].to_owned()).await;
            println!("Finished, with result: {:?}", result);
            println!("Starting Cleanup");
            cleanup(ss).await;
            println!("Cleanup Finished");
            result
        }
    };
}
fn print_help() {
    println!("-l for full gpu passthrough, -s for spice, Must specify mouse input path")
}

/// State of the system
pub struct SystemState{
    dbus: dbus_manager::DBusConnection,
    gpu_state: GpuSetupState,
    performance: PerformanceState,
    mouse: MouseState,
    session: Sessions
}
impl SystemState{
    pub fn new() -> Result<Self, DBusError>{
        Ok(Self{
            dbus: dbus_manager::DBusConnection::new()?, 
            gpu_state: GpuSetupState::default(),
            performance: PerformanceState::default(),
            mouse: MouseState::default(),
            session: Sessions::default()
        })
    }
}

async fn root_app(ss: &mut SystemState, vm_type: LaunchConfig, mouse_path: String) -> Result<(), AppError> {
    // dc gpu
    println!("Start");
    if vm_type.requires_gpu_dc() {
        system_setup::gpu::dc_gpu(ss).await.map_err(|err| AppError::FailedToDCGpu(err))?;
    }
    // create session handler
    println!("Creating Session Handler");
    Sessions::create_session_handler(ss).await.map_err(|err| AppError::FailedToCreateSessionHandler(err))?;
    // wait for login if requires
    if vm_type.requires_gpu_dc() {
        println!("Waiting for session");
        session::AnyDisplayFuture::new(&mut ss.session).await;
    }
    // easy performance enhancements
    println!("Performing quick enhancements");
    system_setup::performance_enhancements(ss).await.map_err(|err| AppError::FailedToDoPerformanceEnhancements(err))?;
    // create virtual mouse
    println!("Creating virtual mouse");
    let mut mouse_manager = virtual_mouse::MouseManager::new(&mouse_path)
        .map_err(|err| AppError::FailedToCreateVirtualMouse(err))?;
    let output_id = mouse_manager.output_id.clone();
    ss.mouse.input_id = mouse_manager.input_id.clone();
    println!("Setting up mouse session handler");
    ss.mouse.session_handle = Some(mouse_manager.setup_session_handler(ss).await.map_err(|err| AppError::FailedToSetupMouseSessionHandler(err))?);
    println!("Spawning mouse update loop");
    ss.mouse.handle = Some(mouse_manager.spawn_update_loop());
    // finish xml
    println!("Finishing and writing vm xml");
    let xml_path = create_xml(vm_type.clone(), output_id).map_err(|err| AppError::FailedToCreateXml(err))?;
    // launch the vm
    println!("Launching the vm");
    let handle = system_setup::launch_vm(xml_path).map_err(|err| AppError::FailedToLaunchVm(err))?;
    // setup session handler for launching vm viewer
    println!("Setting up viewer session handler");
    Sessions::create_viewer_session_handler(vm_type, ss).await.map_err(|err| AppError::FailedToCreateViewAppHandler(err))?;
    // wait for vm to close
    println!("Waiting for vm to close");
    let _ = handle.await;
    Ok(())
}
/// reverts pc configuration
async fn cleanup(mut ss: SystemState){
    // reverse performance
    println!("Reverting Performance Enhancements");
    revert_performance_enhancements(&mut ss).await;
    // kill session handlers
    println!("Ending Session Handlers");
    if let Some(handle) = ss.mouse.handle.as_mut() {handle.abort();}
    if let Some(handle) = ss.mouse.session_handle.as_mut() {handle.abort();}
    if let Some(handle) = ss.session.viewer_hadle.as_mut() {handle.abort();}
    if let Some(handle) = ss.session.session_handle.as_mut() {handle.abort();}
    // unlock mouse
    println!("Resetting Mouse Locks");
    virtual_mouse::MouseManager::reset_sessions(&mut ss).await;
    // revert gpu state,
    println!("Resetting GPU config");
    system_setup::gpu::cleanup(&mut ss).await;
}