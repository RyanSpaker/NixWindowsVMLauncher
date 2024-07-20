/*
    This module is reponsible for the setup of the vm
    It works with the server to execute the necessaty actions and work when requested.
*/

use std::{env::VarError, error::Error, fmt::Display, fs::File, io::{Read, Write}, path::Path, process::Stdio, sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex}, time::Duration};
use dbus::{arg::Variant, nonblock::{Proxy, SyncConnection}};
use crate::server::{ServerData, ServerError, UserConnectedFuture, VmLaunchFuture, VmShutdownFuture};

#[derive(Debug, Default, Clone)]
pub enum VmState{
    #[default] Inactive,
    Activating,
    Launched,
    ShuttingDown
}
impl ToString for VmState{
    fn to_string(&self) -> String {
        match self{
            Self::Inactive => "Not Running",
            Self::Activating => "Starting up",
            Self::Launched => "Running",
            Self::ShuttingDown => "Stopping"
        }.to_string()
    }
}
#[derive(Debug, Default, Clone)]
pub enum VmType{
    #[default] LookingGlass,
    Spice
}
impl ToString for VmType{
    fn to_string(&self) -> String {
        match self {
            Self::LookingGlass => "Looking Glass",
            Self::Spice => "Spice"
        }.to_string()
    }
}

/// Represents all ways the session program can fail
#[derive(Debug)]
pub enum LauncherError{
    ServerError(ServerError),
    FailedToLockData,
    FailedToSetCPUs(dbus::Error),
    FailedToReadCPUDir(std::io::Error),
    FailedToCreateMouse(dbus::Error),
    FailedToGetXmlPath(VarError),
    FailedToReadXmlPath(String, std::io::Error),
    FailedToCreateXmlFile(std::io::Error),
    FailedtoCreateLogFile(std::io::Error),
    FailedToLaunchVM(std::io::Error),
    FailedToStopDP(dbus::Error),
    ProcessesDidNotExit,
    FailedToGetProcesses(std::io::Error),
    FailedToUnloadKernelModule(String, std::io::Error),
    ModprobeRemoveReturnedErr(String, String),
    FailedToDisconnectGPU(String, std::io::Error),
    FailedToLoadKernelModule(String, std::io::Error),
    FailedToStartDP(dbus::Error),
    FailedToShutdownVm(std::io::Error),
    FailedToDestroyVm(std::io::Error),
    FailedToStopVirtualMouse(dbus::Error),
    FailedToConnectGPU(String, std::io::Error),
    FailedToRestartDP(dbus::Error),
    FailedToGetUsers(dbus::Error),
    FailedToGetVmState(std::io::Error)
}
impl Display for LauncherError{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _ = f.write_str(&match self {
            Self::ServerError(err) => err.to_string(),
            Self::FailedToLockData => format!("Could not lock ServerData"),
            Self::FailedToSetCPUs(err) => format!("Could not set AllowedCPUs with err: {}", *err),
            Self::FailedToReadCPUDir(err) => format!("Could not read the cpu directory: {}", *err),
            Self::FailedToCreateMouse(err) => format!("Could not create a virtual mouse: {}", *err),
            Self::FailedToGetXmlPath(err) => format!("Could not get the xml path from the environment variables: {}", *err),
            Self::FailedToReadXmlPath(path, err) => format!("Could not read the xml path: {}, with err: {}", *path, *err),
            Self::FailedToCreateXmlFile(err) => format!("Failed to create the xml file at /tmp/windows.xml: {}", *err),
            Self::FailedtoCreateLogFile(err) => format!("Failed to create vm log file: {}", *err),
            Self::FailedToLaunchVM(err) => format!("Failed to launch the vm with virsh: {}", *err),
            Self::FailedToStopDP(err) => format!("Could not stop the display manager: {}", *err),
            Self::ProcessesDidNotExit => format!("Waited 2 seconds, but processes that use the gpu did not close after stopping the display manager and pipewire"),
            Self::FailedToGetProcesses(err) => format!("Could not get root processes from ps: {}", *err),
            Self::FailedToUnloadKernelModule(name, err) => format!("Failed to unload kernel module {}, with err: {}", *name, *err),
            Self::ModprobeRemoveReturnedErr(name, stderr) => format!("Modprobe returned err while unloading {}, with stderr: {}", *name, *stderr),
            Self::FailedToDisconnectGPU(pci, err) => format!("Failed to disconnect pci {}, with err: {}", *pci, *err),
            Self::FailedToLoadKernelModule(name, err) => format!("Failed to load kernel module {}, with err: {}", *name, *err),
            Self::FailedToStartDP(err) => format!("Failed to start display-manager.service with err: {}", *err),
            Self::FailedToShutdownVm(err) => format!("Failed to shutdown the vm with virsh: {}", *err),
            Self::FailedToDestroyVm(err) => format!("Failed to destroy the vm with virsh: {}", *err),
            Self::FailedToStopVirtualMouse(err) => format!("Failed to stop the virtual mouse: {}", *err),
            Self::FailedToConnectGPU(pci, err) => format!("Failed to reconnect gpu: {}, with err: {}", *pci, *err),
            Self::FailedToRestartDP(err) => format!("Failed to restart display-manager.service: {}", *err),
            Self::FailedToGetUsers(err) => format!("Failed to get users from login1: {}", *err),
            Self::FailedToGetVmState(err) => format!("failed to get vm state from virsh: {}", *err)
        });
        Ok(())
    }
}
impl Error for LauncherError{}

/// Represents the state of the system, and all changes we have made
#[derive(Default, Debug)]
pub struct SystemState{
    cpus_limited: (AtomicBool, AtomicBool, AtomicBool),
    performance_governor: AtomicBool,
    virtual_mouse_create: AtomicBool,
    vm_launched: AtomicBool,
    dp_stopped: AtomicBool,
    pw_stopped: AtomicBool,
    nvidia_unloaded: (AtomicBool, AtomicBool, AtomicBool, AtomicBool),
    gpu_dettached: (AtomicBool, AtomicBool),
    vfio_loaded: AtomicBool
}
impl SystemState {
    pub fn revert(&self) {
        self.cpus_limited.0.store(false, Ordering::Relaxed);
        self.cpus_limited.1.store(false, Ordering::Relaxed);
        self.cpus_limited.2.store(false, Ordering::Relaxed);
        self.performance_governor.store(false, Ordering::Relaxed);
        self.virtual_mouse_create.store(false, Ordering::Relaxed);
        self.vm_launched.store(false, Ordering::Relaxed);
        self.dp_stopped.store(false, Ordering::Relaxed);
        self.pw_stopped.store(false, Ordering::Relaxed);
        self.nvidia_unloaded.0.store(false, Ordering::Relaxed);
        self.nvidia_unloaded.1.store(false, Ordering::Relaxed);
        self.nvidia_unloaded.2.store(false, Ordering::Relaxed);
        self.nvidia_unloaded.3.store(false, Ordering::Relaxed);
        self.gpu_dettached.0.store(false, Ordering::Relaxed);
        self.gpu_dettached.1.store(false, Ordering::Relaxed);
        self.vfio_loaded.store(false, Ordering::Relaxed);
    }
}

/// Asynchronous loop which handles all system setup. should never return
pub async fn launcher(data: Arc<Mutex<ServerData>>, conn: Arc<SyncConnection>) -> LauncherError{
    let system_state = Arc::new(SystemState::default());
    loop{
        // wait for vm to be requested
        println!("Waiting for vm launch to be requested...");
        if let Err(err) = (VmLaunchFuture{data: data.clone()}).await {return LauncherError::ServerError(err);};
        // do work
        println!("Spawning VM Launch");
        let handle = tokio::spawn(launch_vm(data.clone(), system_state.clone(), conn.clone()));
        // wait for work to finish, or shutdown signal
        tokio::select! {
            result = handle => {
                println!("VM Launch Finished");
                if let Ok(Err(err)) = result {  
                    let _ = cleanup(system_state, conn).await;
                    return err;
                }
                if let Ok(mut guard) = data.lock() {guard.vm_state.set(VmState::ShuttingDown);}
            },
            result = VmShutdownFuture{data: data.clone()} => {
                println!("Shutdown Interrupted Vm Launch");
                if let Err(err) = result {return LauncherError::ServerError(err);}
            }
        }
        // cleanup
        println!("Cleaning up...");
        let mut errors = cleanup(system_state.clone(), conn.clone()).await;
        if errors.len() > 0 {return errors.remove(0);};
        let mut guard = match data.lock() {Ok(guard) => guard, _ => {return LauncherError::FailedToLockData;}};
        guard.user_connected.set(false);
        guard.vm_state.set(VmState::Inactive);
    }
}

/// asynchronous function, responsible for doing essentially all of the vm launching
pub async fn launch_vm(data: Arc<Mutex<ServerData>>, state: Arc<SystemState>, conn: Arc<SyncConnection>) -> Result<(), LauncherError>{
    let vm_type = data.lock().map_err(|_| LauncherError::FailedToLockData)?.vm_type.clone();
    // if type is lg, we need to dc the gpu
    if let VmType::LookingGlass = vm_type {
        println!("Disconnecting GPU");
        dc_gpu(state.clone(), conn.clone()).await?;
    }
    // we need to wait for a user session to connect before continuing
    println!("Waiting for user connection");
    UserConnectedFuture{data: data.clone()}.await.map_err(|err| LauncherError::ServerError(err))?;
    // setup the pc
    println!("Setting up PC...");
    let mouse_path = data.lock().map_err(|_|LauncherError::FailedToLockData)?.mouse_path.clone();
    setup_pc(state.clone(), conn.clone(), mouse_path, vm_type.clone()).await?;
    // launch vm
    println!("Starting VM");
    start_vm(state.clone()).await?;
    // inform users that state has changed
    if let Ok(mut guard) = data.lock() {guard.vm_state.set(VmState::Launched);} else {return Err(LauncherError::FailedToLockData);}
    // wait for vm to shutdown
    println!("Waiting for vm to close");
    wait_on_vm(state.clone()).await?;
    Ok(())
}

/// asynchronous function responsible for reverting changes done in launch_vm. any errors are stored and returned at the end, will attempt to revert all changes regardless of errors
pub async fn cleanup(state: Arc<SystemState>, conn: Arc<SyncConnection>) -> Vec<LauncherError>{
    let mut errors: Vec<LauncherError> = vec![];
    // make sure vm is shutdown
    if state.vm_launched.load(Ordering::Relaxed) {
        println!("Shutting Down VM");
        if let Err(err) = tokio::process::Command::new("virsh").args(["-cqemu:///windows", "shutdown", "windows"]).status().await {
            errors.push(LauncherError::FailedToShutdownVm(err));
        };
        let mut success = false;
        println!("Waiting for vm to shutdown");
        for _ in 0..30 {
            match tokio::process::Command::new("virsh").args(["-cqemu:///system", "domstate", "windows"]).output().await {
                Err(err) => {errors.push(LauncherError::FailedToGetVmState(err)); break;}
                Ok(output) => {
                    if !output.status.success() {success = true; break;}
                    else {tokio::time::sleep(Duration::from_secs(1)).await;}
                }
            }
        }
        if !success {
            println!("Destroying VM");
            if let Err(err) = tokio::process::Command::new("virsh").args(["-cqemu:///windows", "destroy", "windows"]).status().await {
                errors.push(LauncherError::FailedToDestroyVm(err));
            }
        }
    }
    // undo state changes
    // stop virtual mouse
    if state.virtual_mouse_create.load(Ordering::Relaxed) {
        println!("Stopping Virtual Mouse");
        let proxy = Proxy::new("org.cws.VirtualMouse", "/org/cws/VirtualMouse", Duration::from_secs(2), conn.clone());
        if let Err(err) = proxy.method_call::<(String, String, String), _, _, _>("org.cws.VirtualMouse.Manager", "DestroyMouse", ("WindowsMouse",)).await {
            errors.push(LauncherError::FailedToStopVirtualMouse(err));
        }
    }
    println!("Undoing governor and cpu limiting");
    // undo performance governor
    if state.performance_governor.load(Ordering::Relaxed) {
        match Path::new("/sys/devices/system/cpu/").read_dir() {
            Err(err) => {errors.push(LauncherError::FailedToReadCPUDir(err));}
            Ok(dir) => {
                let mut files = dir.into_iter().flatten().filter_map(|dir| {
                    if dir.file_type().unwrap().is_file() || !dir.file_name().to_str().unwrap().starts_with("cpu") {return None;}
                    File::create(dir.path().join("cpufreq/scaling_governor")).ok()
                }).collect::<Vec<File>>();
                for file in files.iter_mut(){
                    let _ = file.write("performance".as_bytes());
                }
            }
        };
    }
    // undo cpu limiting
    if state.cpus_limited.0.load(Ordering::Relaxed) {
        let proxy = Proxy::new(
            "org.freedesktop.systemd1", 
            "/org/freedesktop/systemd1/unit/user_2eslice", 
            Duration::from_secs(2), conn.clone());
        if let Err(err) = proxy.method_call::<(), _, _, _>(
            "org.freedesktop.systemd1.Unit", 
            "SetProperties", 
            (true, vec![("AllowedCPUs", Variant(vec![0_u8, 240_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8]))])
        ).await {errors.push(LauncherError::FailedToSetCPUs(err));}
    }
    if state.cpus_limited.1.load(Ordering::Relaxed) {
        let proxy = Proxy::new(
            "org.freedesktop.systemd1", 
            "/org/freedesktop/systemd1/unit/system_2eslice", 
            Duration::from_secs(2), conn.clone());
        if let Err(err) = proxy.method_call::<(), _, _, _>(
            "org.freedesktop.systemd1.Unit", 
            "SetProperties", 
            (true, vec![("AllowedCPUs", Variant(vec![0_u8, 240_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8]))])
        ).await {errors.push(LauncherError::FailedToSetCPUs(err));}
    }
    if state.cpus_limited.2.load(Ordering::Relaxed) {
        let proxy = Proxy::new(
            "org.freedesktop.systemd1", 
            "/org/freedesktop/systemd1/unit/unit_2escope", 
            Duration::from_secs(2), conn.clone());
        if let Err(err) = proxy.method_call::<(), _, _, _>(
            "org.freedesktop.systemd1.Unit", 
            "SetProperties", 
            (true, vec![("AllowedCPUs", Variant(vec![0_u8, 240_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8]))])
        ).await {errors.push(LauncherError::FailedToSetCPUs(err));}
    }
    // undo gpu disconnection
    println!("Reconnecting gpu");
    errors.extend(rc_gpu(state.clone(), conn.clone()).await);
    // revert state to default
    state.revert();
    errors
}

/// Disconnects the gpu from the system
pub async fn dc_gpu(state: Arc<SystemState>, conn: Arc<SyncConnection>) -> Result<(), LauncherError>{
    // stop display manager
    println!("Stopping Display Manager");
    let proxy = Proxy::new("org.freedesktop.systemd1", "/org/freedesktop/systemd1", Duration::from_secs(2), conn.clone());
    let _: (dbus::Path,) = proxy.method_call("org.freedesktop.systemd1.Manager", "StopUnit", ("display-manager.service", "replace")).await
        .map_err(|err| LauncherError::FailedToStopDP(err))?;
    state.dp_stopped.store(true, Ordering::Relaxed);
    // stop pipewire
    println!("Stopping Pipewire");
    let login_proxy = Proxy::new("org.freedesktop.login1", "/org/freedesktop/login1", Duration::from_secs(2), conn.clone());
    let (users,) = login_proxy.method_call::<(Vec<(u32, String, dbus::Path)>,), _, _, _>("org.freedesktop.login1.Manager", "ListUsers", ()).await
        .map_err(|err| LauncherError::FailedToGetUsers(err))?;
    for (user, _, _) in users.iter(){
        let _ = tokio::process::Command::new("systemctl").args(["--user", &format!("--machine={}@", user), "stop", "pipewire.socket"])
            .stderr(Stdio::null()).stdout(Stdio::null()).status().await;
        let _ = tokio::process::Command::new("systemctl").args(["--user", &format!("--machine={}@", user), "stop", "pipewire-pulse.socket"])
            .stderr(Stdio::null()).stdout(Stdio::null()).status().await;
    }
    state.pw_stopped.store(true, Ordering::Release);
    // wait for processes to close
    println!("Waiting for processes to close");
    let mut success = false;
    for _ in 0..20{
        let output = tokio::process::Command::new("ps").args(["-u", "root"]).stderr(Stdio::null()).stdout(Stdio::piped()).output().await
            .map_err(|err| LauncherError::FailedToGetProcesses(err))?.stdout;
        let output = String::from_utf8_lossy(&output);
        if output.contains("sddm") || output.contains("X") {
            tokio::time::sleep(Duration::from_secs_f32(0.1)).await;
            continue;
        };
        success = true; break;
    }
    if !success {return Err(LauncherError::ProcessesDidNotExit);}
    // unload nvidia
    println!("Unloading Nvidia Modules");
    let out = tokio::process::Command::new("modprobe").args(["-f", "-r", "nvidia_uvm"]).output().await
        .map_err(|err| LauncherError::FailedToUnloadKernelModule("nvidia_uvm".to_string(), err))?;
    if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
        return Err(LauncherError::ModprobeRemoveReturnedErr("nvidia_uvm".to_string(), String::from_utf8(out.stderr.clone()).unwrap()));
    }
    state.nvidia_unloaded.0.store(true, Ordering::Relaxed);
    let out = tokio::process::Command::new("modprobe").args(["-f", "-r", "nvidia_drm"]).output().await
        .map_err(|err| LauncherError::FailedToUnloadKernelModule("nvidia_drm".to_string(), err))?;
    if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
        return Err(LauncherError::ModprobeRemoveReturnedErr("nvidia_drm".to_string(), String::from_utf8(out.stderr.clone()).unwrap()));
    }
    state.nvidia_unloaded.1.store(true, Ordering::Relaxed);
    let out = tokio::process::Command::new("modprobe").args(["-f", "-r", "nvidia_modeset"]).output().await
        .map_err(|err| LauncherError::FailedToUnloadKernelModule("nvidia_modeset".to_string(), err))?;
    if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
        return Err(LauncherError::ModprobeRemoveReturnedErr("nvidia_modeset".to_string(), String::from_utf8(out.stderr.clone()).unwrap()));
    }
    state.nvidia_unloaded.2.store(true, Ordering::Relaxed);
    let out = tokio::process::Command::new("modprobe").args(["-f", "-r", "nvidia"]).output().await
        .map_err(|err| LauncherError::FailedToUnloadKernelModule("nvidia".to_string(), err))?;
    if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
        return Err(LauncherError::ModprobeRemoveReturnedErr("nvidia".to_string(), String::from_utf8(out.stderr.clone()).unwrap()));
    }
    state.nvidia_unloaded.3.store(true, Ordering::Relaxed);
    // disconnect
    println!("Disconnecting GPU");
    let _ = tokio::process::Command::new("virsh").args(["nodedev-detach", "pci_0000_01_00_0"]).status().await
        .map_err(|err| LauncherError::FailedToDisconnectGPU("pci_0000_01_00_0".to_string(), err))?;
    state.gpu_dettached.0.store(true, Ordering::Relaxed);
    let _ = tokio::process::Command::new("virsh").args(["nodedev-detach", "pci_0000_01_00_1"]).status().await
        .map_err(|err| LauncherError::FailedToDisconnectGPU("pci_0000_01_00_1".to_string(), err))?;
    state.gpu_dettached.1.store(true, Ordering::Relaxed);
    // load vfio
    println!("Loading VFIO");
    let _ = tokio::process::Command::new("modprobe").args(["vfio-pci"]).status().await
        .map_err(|err| LauncherError::FailedToLoadKernelModule("vfio-pci".to_string(), err))?;
    state.vfio_loaded.store(true, Ordering::Relaxed);
    // restart pipewire
    println!("Starting Pipewire");
    for (user, _, _) in users.iter(){
        let _ = tokio::process::Command::new("systemctl").args(["--user", &format!("--machine={}@", user), "start", "pipewire.socket"])
            .stderr(Stdio::null()).stdout(Stdio::null()).status().await;
        let _ = tokio::process::Command::new("systemctl").args(["--user", &format!("--machine={}@", user), "start", "pipewire-pulse.socket"])
            .stderr(Stdio::null()).stdout(Stdio::null()).status().await;
    }
    state.pw_stopped.store(false, Ordering::Relaxed);
    // restart display manager
    println!("Starting Display Manager");
    let _: (dbus::Path,) = proxy.method_call("org.freedesktop.systemd1.Manager", "StartUnit", ("display-manager.service", "replace")).await
        .map_err(|err| LauncherError::FailedToStartDP(err))?;
    state.dp_stopped.store(false, Ordering::Relaxed);
    Ok(())
}

/// Reconnects the gpu, by doing any necessary steps as determined by state. errors are ignored, and returned at the end as a list
pub async fn rc_gpu(state: Arc<SystemState>, conn: Arc<SyncConnection>) -> Vec<LauncherError> {
    let mut errors: Vec<LauncherError> = vec![];
    let mut reset_dp = false; let mut reset_pw = false;
    // do any work to reconnect the gpu
    // unload vfio
    if state.vfio_loaded.load(Ordering::Relaxed) {
        println!("Unloading vfio");
        match tokio::process::Command::new("modprobe").args(["-f", "-r", "vfio-pci"]).output().await {
            Err(err) => {errors.push(LauncherError::FailedToUnloadKernelModule("vfio-pci".to_string(), err));},
            Ok(out) => {
                if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
                    errors.push(LauncherError::ModprobeRemoveReturnedErr("vfio-pci".to_string(), String::from_utf8(out.stderr.clone()).unwrap()));
                }
            }
        }
        reset_dp = true; reset_pw = true;
    }
    // reattach gpu
    if state.gpu_dettached.0.load(Ordering::Relaxed) {
        println!("Reconnecting gpu 0");
        if let Err(err) = tokio::process::Command::new("virsh").args(["nodedev-reattach", "pci_0000_01_00_0"]).status().await{
            errors.push(LauncherError::FailedToConnectGPU("pci_0000_01_00_0".to_string(), err));
        }
        reset_dp = true; reset_pw = true;
    }
    if state.gpu_dettached.1.load(Ordering::Relaxed) {
        println!("Reconnecting gpu 1");
        if let Err(err) = tokio::process::Command::new("virsh").args(["nodedev-reattach", "pci_0000_01_00_1"]).status().await{
            errors.push(LauncherError::FailedToConnectGPU("pci_0000_01_00_1".to_string(), err));
        }
        reset_dp = true; reset_pw = true;
    }
    // load nvidia
    if state.nvidia_unloaded.3.load(Ordering::Relaxed) {
        println!("Loading nvidia");
        if let Err(err) = tokio::process::Command::new("modprobe").args(["nvidia"]).status().await{
            errors.push(LauncherError::FailedToLoadKernelModule("nvidia".to_string(), err));
        }
        reset_dp = true; reset_pw = true;
    }
    if state.nvidia_unloaded.2.load(Ordering::Relaxed) {
        println!("Loading nvidia");
        if let Err(err) = tokio::process::Command::new("modprobe").args(["nvidia_modeset"]).status().await{
            errors.push(LauncherError::FailedToLoadKernelModule("nvidia_modeset".to_string(), err));
        }
        reset_dp = true; reset_pw = true;
    }
    if state.nvidia_unloaded.1.load(Ordering::Relaxed) {
        println!("Loading nvidia");
        if let Err(err) = tokio::process::Command::new("modprobe").args(["nvidia_drm"]).status().await{
            errors.push(LauncherError::FailedToLoadKernelModule("nvidia_drm".to_string(), err));
        }
        reset_dp = true; reset_pw = true;
    }
    if state.nvidia_unloaded.0.load(Ordering::Relaxed) {
        println!("Loading nvidia");
        if let Err(err) = tokio::process::Command::new("modprobe").args(["nvidia_uvm"]).status().await{
            errors.push(LauncherError::FailedToLoadKernelModule("nvidia_uvm".to_string(), err));
        }
        reset_dp = true; reset_pw = true;
    }
    // if the dp or pw is not started, start it
    if state.dp_stopped.load(Ordering::Relaxed) {
        println!("Starting Display Manager");
        let proxy = Proxy::new("org.freedesktop.systemd1", "/org/freedesktop/systemd1", Duration::from_secs(2), conn.clone());
        if let Err(err) = proxy.method_call::<(dbus::Path,), _, _, _>("org.freedesktop.systemd1.Manager", "StartUnit", ("display-manager.service", "replace")).await{
            errors.push(LauncherError::FailedToStartDP(err));
        }
        reset_dp = false;
    }
    if state.pw_stopped.load(Ordering::Relaxed) {
        println!("Starting Pipewire");
        unsafe{for user in users::all_users() {
            let _ = tokio::process::Command::new("systemctl").args(["--user", &format!("--machine={}@", user.uid()), "start", "pipewire.socket"])
                .stderr(Stdio::null()).stdout(Stdio::null()).status().await;
            let _ = tokio::process::Command::new("systemctl").args(["--user", &format!("--machine={}@", user.uid()), "start", "pipewire-pulse.socket"])
                .stderr(Stdio::null()).stdout(Stdio::null()).status().await;
        }}
        reset_pw = false;
    }
    // if we did any work to reconnect the gpu, restart dp
    if reset_pw {
        println!("Resetting Pipewire");
        unsafe{for user in users::all_users() {
            let _ = tokio::process::Command::new("systemctl").args(["--user", &format!("--machine={}@", user.uid()), "restart", "pipewire.socket"])
                .stderr(Stdio::null()).stdout(Stdio::null()).status().await;
            let _ = tokio::process::Command::new("systemctl").args(["--user", &format!("--machine={}@", user.uid()), "restart", "pipewire-pulse.socket"])
                .stderr(Stdio::null()).stdout(Stdio::null()).status().await;
        }}
    }
    if reset_dp {
        println!("Resetting Display Manager");
        let proxy = Proxy::new("org.freedesktop.systemd1", "/org/freedesktop/systemd1", Duration::from_secs(2), conn.clone());
        if let Err(err) = proxy.method_call::<(dbus::Path,), _, _, _>("org.freedesktop.systemd1.Manager", "RestartUnit", ("display-manager.service", "replace")).await{
            errors.push(LauncherError::FailedToRestartDP(err));
        }
    }
    errors
}

/// Performance Enhancements, Virtual Mouse, Create Xml
pub async fn setup_pc(state: Arc<SystemState>, conn: Arc<SyncConnection>, mouse_path: String, vm_type: VmType) -> Result<(), LauncherError>{
    // set available gpu's
    let proxy = Proxy::new(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1/unit/user_2eslice", 
        Duration::from_secs(2), conn.clone());
    let _: () = proxy.method_call(
        "org.freedesktop.systemd1.Unit", 
        "SetProperties", 
        (true, vec![("AllowedCPUs", Variant(vec![0_u8, 240_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8]))])
    ).await.map_err(|err| LauncherError::FailedToSetCPUs(err))?;
    state.cpus_limited.0.store(true, Ordering::Relaxed);
    let proxy = Proxy::new(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1/unit/system_2eslice", 
        Duration::from_secs(2), conn.clone());
    let _: () = proxy.method_call(
        "org.freedesktop.systemd1.Unit", 
        "SetProperties", 
        (true, vec![("AllowedCPUs", Variant(vec![0_u8, 240_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8]))])
    ).await.map_err(|err| LauncherError::FailedToSetCPUs(err))?;
    state.cpus_limited.1.store(true, Ordering::Relaxed);
    let proxy = Proxy::new(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1/unit/unit_2escope", 
        Duration::from_secs(2), conn.clone());
    let _: () = proxy.method_call(
        "org.freedesktop.systemd1.Unit", 
        "SetProperties", 
        (true, vec![("AllowedCPUs", Variant(vec![0_u8, 240_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8]))])
    ).await.map_err(|err| LauncherError::FailedToSetCPUs(err))?;
    state.cpus_limited.2.store(true, Ordering::Relaxed);
    // Set cpu governor
    let mut files = Path::new("/sys/devices/system/cpu/").read_dir().map_err(|err| LauncherError::FailedToReadCPUDir(err))?
        .into_iter().flatten().filter_map(|dir| {
            if dir.file_type().unwrap().is_file() || !dir.file_name().to_str().unwrap().starts_with("cpu") {return None;}
            File::create(dir.path().join("cpufreq/scaling_governor")).ok()
        }).collect::<Vec<File>>();
    for file in files.iter_mut(){
        let _ = file.write("performance".as_bytes());
    }
    state.performance_governor.store(true, Ordering::Relaxed);
    // create virtual mouse
    let proxy = Proxy::new(
        "org.cws.VirtualMouse", 
        "/org/cws/VirtualMouse", 
        Duration::from_secs(2), conn.clone());
    let (_, _, outputpath): (String, String, String) = proxy.method_call(
        "org.cws.VirtualMouse.Manager", 
        "CreateMouse", 
        ("WindowsMouse", mouse_path)
    ).await.map_err(|err| LauncherError::FailedToCreateMouse(err))?;
    state.virtual_mouse_create.store(true, Ordering::Relaxed);
    // create xml
    let xml_source_path = match vm_type {
        VmType::LookingGlass => {std::env::var("WINDOWS_LG_XML")},
        VmType::Spice => {std::env::var("WINDOWS_SPICE_XML")}
    }.map_err(|err| LauncherError::FailedToGetXmlPath(err))?;
    let mut xml_string = String::with_capacity(10000);
    match File::open(xml_source_path.clone()).map(|mut file| file.read_to_string(&mut xml_string)) {
        Ok(Ok(_)) => {},
        Ok(Err(err)) => {return Err(LauncherError::FailedToReadXmlPath(xml_source_path, err));}
        Err(err) => {return Err(LauncherError::FailedToReadXmlPath(xml_source_path, err));}
    };
    xml_string = xml_string.replace("VIRTUAL_MOUSE_EVENT_PATH", &outputpath);
    match File::create("/tmp/windows.xml").map(|mut file| file.write(xml_string.as_bytes())) {
        Ok(Ok(_)) => {},
        Ok(Err(err)) => {return Err(LauncherError::FailedToCreateXmlFile(err));}
        Err(err) => {return Err(LauncherError::FailedToCreateXmlFile(err));}
    };
    Ok(())
}

/// Launch vm
pub async fn start_vm(state: Arc<SystemState>) -> Result<(), LauncherError>{
    let log_file = File::create(format!("/var/log/windows/vm/log-{}.txt", chrono::Local::now().to_string()))
        .map_err(|err| LauncherError::FailedtoCreateLogFile(err))?;
    let log = Stdio::from(log_file.try_clone().map_err(|err| LauncherError::FailedtoCreateLogFile(err))?);
    let log_err = Stdio::from(log_file);
    let _ = tokio::process::Command::new("virsh").args(["-cqemu:///system", "create", "/tmp/windows.xml"])
        .stdout(log).stderr(log_err).spawn()
        .map_err(|err| LauncherError::FailedToLaunchVM(err))?.wait().await;
    state.vm_launched.store(true, Ordering::Relaxed);
    Ok(())
}

/// wait for vm
pub async fn wait_on_vm(state: Arc<SystemState>) -> Result<(), LauncherError>{
    loop{
        let state = tokio::process::Command::new("virsh").args(["-cqemu:///system", "domstate", "windows"]).output().await
            .map_err(|err| LauncherError::FailedToGetVmState(err))?;
        if !state.status.success() {break;}
    }
    state.vm_launched.store(false, Ordering::Relaxed);
    Ok(())
}