use std::time::Duration;

use crate::{dbus_server::{DBusState, NoUserFuture, UserLoginFuture}, system_setup, virtual_mouse, AppError, LaunchConfig, SystemState};

/// Async fn which queries vm state until it closes, then returns
async fn vm_close() -> Result<(), AppError> {
    loop{
        if !super::call_command("virsh", ["-cqemu:///system", "domstate", "windows"])
        .map_err(|err| AppError::FailedToQueryVMState(err))?.status.success() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

pub async fn master(dbus_state: &mut DBusState, system_state: &mut SystemState) -> Result<(), AppError>{
    if system_state.launch_type.dc_gpu() {
        // remove gpu
        println!("Disconnecting GPU");
        system_setup::unload_gpu(dbus_state, system_state).await.map_err(|err| AppError::SetupError(err))?;
        println!("GPU Disconnected");
    }

    // read vm xml
    println!("Reading XML");
    let xml = system_setup::get_vm_xml(system_state.launch_type).map_err(|err| AppError::SetupError(err))?;

    if let LaunchConfig::LG = system_state.launch_type {
        // Wait for at least 1 user to login
        println!("Waiting for login...");
        UserLoginFuture(dbus_state.communicator.as_ref().unwrap().to_owned()).await;
    }
    
    // Create virtual mouse
    println!("Creating virtual mouse");
    let mouse_manager = virtual_mouse::MouseManager::new(&system_state.mouse_path)
        .map_err(|err| AppError::MouseError(err))?;
    let input_id = mouse_manager.input_id.clone();
    let output_id = mouse_manager.output_id.clone();
    let mouse_handle = virtual_mouse::spawn_mouse_update_loop(mouse_manager);

    // export finished xml
    println!("Writing finished xml");
    system_setup::write_xml(xml.replace("VIRTUAL_MOUSE_EVENT_ID", output_id.to_string().as_str())).map_err(|err| AppError::SetupError(err))?;

    // quick performance enhancements
    println!("Doing performance enhancements");
    system_setup::performance_enhancements(dbus_state, system_state).await.map_err(|err| AppError::SetupError(err))?;

    // launch vm
    println!("Launching VM");
    let output = super::call_command("virsh", ["-cqemu:///system", "create", "/tmp/windows.xml"])
        .map_err(|err| AppError::FailedToLaunchVM(err))?;
    if !output.status.success(){
        return Err(AppError::VMLaunchFailed(String::from_utf8(output.stderr).unwrap()));
    }

    // Inform users that the vm is launched
    println!("Informing Users of launch");
    dbus_state.inform_users(input_id).await;
    
    // Wait until vm is shutdown naturally, or all users have closed the vm window
    println!("Waiting for vm to close");
    let mut vm_close_result = None;
    tokio::select! {
        _ = NoUserFuture(dbus_state.communicator.to_owned().unwrap()) => {}
        result = vm_close() => {vm_close_result = Some(result)}
    };
    println!("VM closed");
    match vm_close_result {
        // All users closed the vm
        None => {
            // shutdown the vm
            println!("Shutting down VM");
            super::call_command("virsh", ["-cqemu:///system", "shutdown", "windows"])
                .map_err(|err| AppError::FailedToShutdownVM(err))?;
            // wait for the vm to shutdown
            vm_close().await?;
        },
        // The vm shutdown
        Some(Ok(())) => {},
        // Querying vm state failed
        Some(Err(err)) => {return Err(err);}
    }
    // send shutdown signal
    println!("Sending shutdown signal");
    dbus_state.send_shutdown_signal().map_err(|err| AppError::BusError(err))?;
    // Stop virtual mouse
    if mouse_handle.is_finished() {mouse_handle.await.unwrap().map_err(|err| AppError::MouseError(err))?;}
    else {mouse_handle.abort();}
    // wait 1 second for users to finish their work
    tokio::time::sleep(Duration::from_secs(1)).await;
    // undo performance
    println!("Undoing performance enhancments");
    system_setup::undo_performance_enhancements(dbus_state, system_state).await.map_err(|err| AppError::SetupError(err))?;
    // reattach gpu if needed
    if system_state.launch_type.dc_gpu() {
        println!("Reconnecting GPU");
        system_setup::reattach_gpu(dbus_state, system_state).await.map_err(|err| AppError::SetupError(err))?;
    }
    // exit
    Ok(())
}