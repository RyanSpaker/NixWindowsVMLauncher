use std::{env::{self, VarError}, ffi::OsStr, fs::File, io::{Read, Write}, path::Path, process::{Output, Stdio}};
use dbus::arg::Variant;
use tokio::task::JoinHandle;

use crate::{dbus_manager::DBusError, LaunchConfig, SystemState};

// Calls the command with the args
pub fn call_command<I, S>(command: &str, args: I) -> std::io::Result<Output>
where I: IntoIterator<Item = S>, S: AsRef<OsStr> {
    std::process::Command::new(command).args(args).stderr(Stdio::piped()).stdout(Stdio::piped()).output()
}

pub mod gpu {
    use std::{collections::HashMap, time::Duration};
    use dbus::Path;
    use crate::{dbus_manager::DBusError, SystemState, system_setup::call_command};

    /// Represents all fail state for the process of disconnecting the gpu from linux
    #[derive(Debug)]
    pub enum GpuSetupError{
        FailedToStopDP(DBusError),
        FailedToConnectUsers(DBusError),
        FailedToStopPW(DBusError),
        FailedToStopPWP(DBusError),
        FailedToUnloadKernelModule(String, std::io::Error),
        ModprobeRemoveReturnedErr(String, String),
        FailedToDisconnectGPU(String, std::io::Error),
        FailedToLoadKernelModule(String, std::io::Error),
        FailedToStartPW(DBusError),
        FailedToStartPWP(DBusError),
        FailedToStartDP(DBusError)
    }
    impl ToString for GpuSetupError{
        fn to_string(&self) -> String {
            match self{
                Self::FailedToStopDP(err) => format!("Could not stop the display manager wuth err: {}", err.to_string()),
                Self::FailedToConnectUsers(err) => format!("Could not connect to user busses: {}", err.to_string()),
                Self::FailedToStopPW(err) => format!("Could not stop users pipewire.socket: {}", err.to_string()),
                Self::FailedToStopPWP(err) => format!("Could not stop users pipewire-pulse.socket: {}", err.to_string()),
                Self::FailedToUnloadKernelModule(name, err) => format!("Could not unload kernel module: {}, with err: {}", *name, *err),
                Self::ModprobeRemoveReturnedErr(name, err) => format!("Modprobe while unloading {}, returned error: {}", *name, *err),
                Self::FailedToDisconnectGPU(pci, err) => format!("Could not use virsh to dc {}, with error: {}", *pci, *err),
                Self::FailedToLoadKernelModule(name, err) => format!("Could not load module: {}, with error: {}", *name, *err),
                Self::FailedToStartPW(err) => format!("Could not start users pipewire.socket: {}", err.to_string()),
                Self::FailedToStartPWP(err) => format!("Could not start users pipewire-pulse.socket: {}", err.to_string()),
                Self::FailedToStartDP(err) => format!("Could not start the display manager wuth err: {}", err.to_string())
            }
        }
    }
    
    /// Represents the state of actions taken during gpu setup
    pub struct GpuSetupState{
        pub dp_state: ServiceState,
        pub pw_state: ServiceState,
        pub pwp_state: ServiceState,
        pub vfio: ModuleState,
        pub gpu_attached: (bool, bool),
        pub nvidia: [ModuleState; 4]
    }
    impl Default for GpuSetupState{
        fn default() -> Self {
            Self{dp_state: ServiceState::Untouched, pw_state: ServiceState::Untouched, pwp_state: ServiceState::Untouched, vfio: ModuleState::Unloaded, gpu_attached: (true, true), nvidia: [ModuleState::Loaded; 4]}
        }
    }
    #[derive(Clone, Copy)]
    pub enum ModuleState{
        Loaded,
        Unloaded
    }
    pub enum ServiceState{
        Untouched,
        Stopped,
        Started,
        Reset
    }
    
    /// Removes the gpu from the system
    pub async fn dc_gpu(ss: &mut SystemState) -> Result<(), GpuSetupError> {
        // Stop display manager
        println!("Stopping Display Manager");
        let (dp_stop_job,): (Path,) = ss.dbus.call_system_method(
            "org.freedesktop.systemd1", 
            "/org/freedesktop/systemd1", 
            "org.freedesktop.systemd1.Manager", 
            "StopUnit", 
            ("display-manager.service", "replace")
        ).await.map_err(|err| GpuSetupError::FailedToStopDP(err))?;
        ss.gpu_state.dp_state = ServiceState::Stopped;
        // Stop pipewire
        println!("Connecting to Users");
        ss.dbus.connect_users().await.map_err(|err| GpuSetupError::FailedToConnectUsers(err))?;
        println!("Stopping Pipewire");
        let pw_stop_jobs: HashMap<u32, (Path,)> = ss.dbus.call_user_method(
            "org.freedesktop.systemd1", 
            "/org/freedesktop/systemd1", 
            "org.freedesktop.systemd1.Manager", 
            "StopUnit", 
            ("pipewire.socket", "replace")
        ).await.map_err(|err| GpuSetupError::FailedToStopPW(err))?;
        ss.gpu_state.pw_state = ServiceState::Stopped;
        println!("Stopping Pipewire-Pulse");
        let pwp_stop_jobs: HashMap<u32, (Path,)> = ss.dbus.call_user_method(
            "org.freedesktop.systemd1", 
            "/org/freedesktop/systemd1", 
            "org.freedesktop.systemd1.Manager", 
            "StopUnit", 
            ("pipewire-pulse.socket", "replace")
        ).await.map_err(|err| GpuSetupError::FailedToStopPWP(err))?;
        ss.gpu_state.pwp_state = ServiceState::Stopped;
        // Wait for stop jobs
        loop{
            println!("Waiting for all stop jobs to finish");
            if ss.dbus.get_system_property::<_, _, _, _, (String,)>("org.freedesktop.systemd1", dp_stop_job.clone(), "org.freedesktop.systemd1.Job", "State").await.is_ok() {
                tokio::time::sleep(Duration::from_secs_f32(0.1)).await;
                continue;
            }
            for (_, (path,)) in pw_stop_jobs.iter() {
                if ss.dbus.get_user_property::<_, _, _, _, (String,)>("org.freedesktop.systemd1", path.to_owned(), "org.freedesktop.systemd1.Job", "State").await.is_ok() {
                    tokio::time::sleep(Duration::from_secs_f32(0.1)).await;
                    continue;
                }
            }
            for (_, (path,)) in pwp_stop_jobs.iter() {
                if ss.dbus.get_user_property::<_, _, _, _, (String,)>("org.freedesktop.systemd1", path.to_owned(), "org.freedesktop.systemd1.Job", "State").await.is_ok() {
                    tokio::time::sleep(Duration::from_secs_f32(0.1)).await;
                    continue;
                }
            }
            break;
        }
        // Unload nvidia kernel modules
        println!("Unloading Nvidia Modules");
        let out = call_command("modprobe", ["-f", "-r", "nvidia_uvm"])
            .map_err(|err| GpuSetupError::FailedToUnloadKernelModule("nvidia_uvm".to_string(), err))?;
        if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
            return Err(GpuSetupError::ModprobeRemoveReturnedErr("nvidia_uvm".to_string(), String::from_utf8(out.stderr.clone()).unwrap()));
        }
        ss.gpu_state.nvidia[0] = ModuleState::Unloaded;
        let out = call_command("modprobe", ["-f", "-r", "nvidia_drm"])
            .map_err(|err| GpuSetupError::FailedToUnloadKernelModule("nvidia_drm".to_string(), err))?;
        if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
            return Err(GpuSetupError::ModprobeRemoveReturnedErr("nvidia_drm".to_string(), String::from_utf8(out.stderr.clone()).unwrap()));
        }
        ss.gpu_state.nvidia[1] = ModuleState::Unloaded;
        let out = call_command("modprobe", ["-f", "-r", "nvidia_modeset"])
            .map_err(|err| GpuSetupError::FailedToUnloadKernelModule("nvidia_modeset".to_string(), err))?;
        if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
            return Err(GpuSetupError::ModprobeRemoveReturnedErr("nvidia_modeset".to_string(), String::from_utf8(out.stderr.clone()).unwrap()));
        }
        ss.gpu_state.nvidia[2] = ModuleState::Unloaded;
        let out = call_command("modprobe", ["-f", "-r", "nvidia"])
            .map_err(|err| GpuSetupError::FailedToUnloadKernelModule("nvidia".to_string(), err))?;
        if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
            return Err(GpuSetupError::ModprobeRemoveReturnedErr("nvidia".to_string(), String::from_utf8(out.stderr.clone()).unwrap()));
        }
        ss.gpu_state.nvidia[3] = ModuleState::Unloaded;
        // Disconnect GPU
        println!("Disconnecting GPU");
        call_command("virsh", ["nodedev-detach", "pci_0000_01_00_0"])
            .map_err(|err| GpuSetupError::FailedToDisconnectGPU("pci_0000_01_00_0".to_string(), err))?;
        ss.gpu_state.gpu_attached.0 = false;
        call_command("virsh", ["nodedev-detach", "pci_0000_01_00_1"])
            .map_err(|err| GpuSetupError::FailedToDisconnectGPU("pci_0000_01_00_1".to_string(), err))?;
        ss.gpu_state.gpu_attached.1 = false;
        // Load VFIO drivers
        println!("Loading VFIO modules");
        super::call_command("modprobe", ["vfio-pci"])
            .map_err(|err| GpuSetupError::FailedToLoadKernelModule("vfio-pci".to_string(), err))?;
        ss.gpu_state.vfio = ModuleState::Loaded;
        // Restart pipewire service
        println!("Starting Pipewire");
        let _: HashMap<u32, (Path,)> = ss.dbus.call_user_method(
            "org.freedesktop.systemd1", 
            "/org/freedesktop/systemd1", 
            "org.freedesktop.systemd1.Manager", 
            "StartUnit", 
            ("pipewire.socket", "replace")
        ).await.map_err(|err| GpuSetupError::FailedToStartPW(err))?;
        ss.gpu_state.pw_state = ServiceState::Started;
        let _: HashMap<u32, (Path,)> = ss.dbus.call_user_method(
            "org.freedesktop.systemd1", 
            "/org/freedesktop/systemd1", 
            "org.freedesktop.systemd1.Manager", 
            "StartUnit", 
            ("pipewire-pulse.socket", "replace")
        ).await.map_err(|err| GpuSetupError::FailedToStartPWP(err))?;
        ss.gpu_state.pwp_state = ServiceState::Started;
        // restart display manager
        println!("Starting Display Manager");
        let _: (Path,) = ss.dbus.call_system_method(
            "org.freedesktop.systemd1", 
            "/org/freedesktop/systemd1", 
            "org.freedesktop.systemd1.Manager", 
            "StartUnit", 
            ("display-manager.service", "replace")
        ).await.map_err(|err| GpuSetupError::FailedToStartDP(err))?;
        ss.gpu_state.dp_state = ServiceState::Started;
        Ok(())
    }
    /// undoes gpu meddling
    pub async fn cleanup(ss: &mut SystemState) {
        let mut dp_reset = false;
        let mut pw_reset = false;
        // if vfio loaded, unload
        if let ModuleState::Loaded = ss.gpu_state.vfio {
            println!("Unloading VFIO");
            let _ = call_command("modprobe", ["-f", "-r", "vfio-pci"]);
        }
        // if gpu is disconnected, reconnect
        if ss.gpu_state.gpu_attached.0 {
            println!("Connecting GPU 0");
            let _ = super::call_command("virsh", ["nodedev-reattach", "pci_0000_01_00_0"]);
        }
        if ss.gpu_state.gpu_attached.1 {
            println!("Connecting GPU 1");
            let _ = super::call_command("virsh", ["nodedev-reattach", "pci_0000_01_00_1"]);
        }
        // if nvidia unloaded, load
        if let ModuleState::Unloaded = ss.gpu_state.nvidia[3] {
            println!("Loading nvidia");
            let _ = super::call_command("modprobe", ["nvidia"]);
        }
        if let ModuleState::Unloaded = ss.gpu_state.nvidia[2] {
            println!("Loading nvidia_modeset");
            let _ = super::call_command("modprobe", ["nvidia_modeset"]);
        }
        if let ModuleState::Unloaded = ss.gpu_state.nvidia[1] {
            println!("Loading nvidia_drm");
            let _ = super::call_command("modprobe", ["nvidia_drm"]);
        }
        if let ModuleState::Unloaded = ss.gpu_state.nvidia[0] {
            println!("Loading nvidia_uvm");
            dp_reset = true;
            pw_reset = true;
            let _ = super::call_command("modprobe", ["nvidia_uvm"]);
        }
        if let ServiceState::Stopped = ss.gpu_state.pw_state {pw_reset = true;} 
        if let ServiceState::Stopped = ss.gpu_state.pwp_state {pw_reset = true;}
        if let ServiceState::Stopped = ss.gpu_state.dp_state {dp_reset = true;}
        if ss.dbus.check_system_bus().await.is_err() {return;}
        if pw_reset {
            println!("Connecting Users");
            let _ = ss.dbus.connect_users().await;
            println!("Restarting PW");
            let _: Result<HashMap<u32, (Path,)>, DBusError> = ss.dbus.call_user_method(
                "org.freedesktop.systemd1", 
                "/org/freedesktop/systemd1", 
                "org.freedesktop.systemd1.Manager", 
                "ReloadOrRestartUnit", 
                ("pipewire.socket", "replace")
            ).await;
            println!("Restarting PWP");
            let _: Result<HashMap<u32, (Path,)>, DBusError> = ss.dbus.call_user_method(
                "org.freedesktop.systemd1", 
                "/org/freedesktop/systemd1", 
                "org.freedesktop.systemd1.Manager", 
                "ReloadOrRestartUnit", 
                ("pipewire-pulse.socket", "replace")
            ).await;
        }
        if dp_reset {
            println!("Restarting DP");
            let _: Result<(Path,), DBusError> = ss.dbus.call_system_method(
                "org.freedesktop.systemd1", 
                "/org/freedesktop/systemd1", 
                "org.freedesktop.systemd1.Manager", 
                "RestartUnit", 
                ("display-manager.service", "replace")
            ).await;
        }
    }
}

#[derive(Debug)]
pub enum SetupError{
    FailedToSetAllowedCPUs(DBusError),
    FailedToReadCPUDir(std::io::Error),
    FailedToFindVmXmlVar(VarError),
    FailedToOpenVmXmlFile(std::io::Error),
    FailedToReadVmXmlFile(std::io::Error),
    FailedToCreateTmpXmlFile(std::io::Error),
    FailedToWriteTmpXmlFile(std::io::Error),
    FailedToLaunchVM(std::io::Error),
    CouldNotCreateWindowsLogFile(std::io::Error)
}
impl ToString for SetupError{
    fn to_string(&self) -> String {
        match self{
            SetupError::FailedToSetAllowedCPUs(err) => format!("Could not set allowed cpus: {}", err.to_string()),
            SetupError::FailedToReadCPUDir(err) => format!("Could not read the cpu dir: {}", *err),
            SetupError::FailedToFindVmXmlVar(err) => format!("Could not find the vm xml env variable: {}", *err),
            SetupError::FailedToOpenVmXmlFile(err) => format!("Could not open vm xml file: {}", *err),
            SetupError::FailedToReadVmXmlFile(err) => format!("Could not read vm xml file: {}", *err),
            SetupError::FailedToCreateTmpXmlFile(err) => format!("Could not create the temp xml file: {}", *err),
            SetupError::FailedToWriteTmpXmlFile(err) => format!("Could not write to temp xml file: {}", *err),
            SetupError::FailedToLaunchVM(err) => format!("Could not launch the vm: {}", *err),
            SetupError::CouldNotCreateWindowsLogFile(err) => format!("Could not create /tmp/windows_vm_log.txt: {}", *err)
        }
    }
}

#[derive(Default, Debug)]
pub struct PerformanceState{
    cpu_limited: [bool; 3],
    governor_performance: bool,
    governor_files: Vec<File>
}
/// Performs quick and easy performance enhancements
pub async fn performance_enhancements(ss: &mut SystemState) -> Result<(), SetupError> {
    let _: () = ss.dbus.call_system_method(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1/unit/user_2eslice", 
        "org.freedesktop.systemd1.Unit", 
        "SetProperties", (true, vec![("AllowedCPUs", Variant(vec![0_u8, 240_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8]))])
    ).await.map_err(|err| SetupError::FailedToSetAllowedCPUs(err))?;
    ss.performance.cpu_limited[0] = true;
    let _: () = ss.dbus.call_system_method(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1/unit/system_2eslice", 
        "org.freedesktop.systemd1.Unit", 
        "SetProperties", (true, vec![("AllowedCPUs", Variant(vec![0_u8, 240_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8]))])
    ).await.map_err(|err| SetupError::FailedToSetAllowedCPUs(err))?;
    ss.performance.cpu_limited[1] = true;
    let _: () = ss.dbus.call_system_method(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1/unit/unit_2escope", 
        "org.freedesktop.systemd1.Unit", 
        "SetProperties", (true, vec![("AllowedCPUs", Variant(vec![0_u8, 240_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8]))])
    ).await.map_err(|err| SetupError::FailedToSetAllowedCPUs(err))?;
    ss.performance.cpu_limited[2] = true;
    let mut files = Path::new("/sys/devices/system/cpu/").read_dir().map_err(|err| SetupError::FailedToReadCPUDir(err))?
        .into_iter().flatten().filter_map(|dir| {
            if dir.file_type().unwrap().is_file() || !dir.file_name().to_str().unwrap().starts_with("cpu") {return None;}
            File::create(dir.path().join("cpufreq/scaling_governor")).ok()
        }).collect::<Vec<File>>();
    for file in files.iter_mut(){
        let _ =file.write("performance".as_bytes());
    }
    ss.performance.governor_files = files;
    ss.performance.governor_performance = true;
    Ok(())
}
/// Undoes any performance enhancements
pub async fn revert_performance_enhancements(ss: &mut SystemState) {
    if ss.performance.governor_performance {
        for file in ss.performance.governor_files.iter_mut() {
            let _ = file.write("powersave".as_bytes());
        }        
    }
    if ss.dbus.check_system_bus().await.is_ok() {
        let _: Result<(), DBusError> = ss.dbus.call_system_method(
            "org.freedesktop.systemd1", 
            "/org/freedesktop/systemd1/unit/user_2eslice", 
            "org.freedesktop.systemd1.Unit", 
            "SetProperties", (true, vec![("AllowedCPUs", Variant(vec![255_u8, 255_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8]))])
        ).await;
        let _: Result<(), DBusError> = ss.dbus.call_system_method(
            "org.freedesktop.systemd1", 
            "/org/freedesktop/systemd1/unit/system_2eslice", 
            "org.freedesktop.systemd1.Unit", 
            "SetProperties", (true, vec![("AllowedCPUs", Variant(vec![255_u8, 255_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8]))])
        ).await;
        let _: Result<(), DBusError> = ss.dbus.call_system_method(
            "org.freedesktop.systemd1", 
            "/org/freedesktop/systemd1/unit/unit_2escope", 
            "org.freedesktop.systemd1.Unit", 
            "SetProperties", (true, vec![("AllowedCPUs", Variant(vec![255_u8, 255_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8]))])
        ).await;
    }
}

/// creates the vm xml, returning the path to it
pub fn create_xml(vm_type: LaunchConfig, mouse_id: u32) -> Result<String, SetupError> {
    let xml_path = env::var(match vm_type {
        LaunchConfig::LG => "WINDOWS_LG_XML_PATH",
        LaunchConfig::Spice => "WINDOWS_SPICE_XML_PATH",
        _ => panic!("How did we get here?")
    }).map_err(|err| SetupError::FailedToFindVmXmlVar(err))?;
    let mut xml_string = String::with_capacity(10000);
    File::open(xml_path).map_err(|err| SetupError::FailedToOpenVmXmlFile(err))?
        .read_to_string(&mut xml_string).map_err(|err| SetupError::FailedToReadVmXmlFile(err))?;
    xml_string = xml_string.replace("VIRTUAL_MOUSE_EVENT_ID", mouse_id.to_string().as_str());
    File::create("/tmp/windows.xml").map_err(|err| SetupError::FailedToCreateTmpXmlFile(err))?
        .write(xml_string.as_bytes()).map_err(|err| SetupError::FailedToWriteTmpXmlFile(err))?;
    Ok("/tmp/windows.xml".to_string())
}

/// Launches the vm, returning a handle to the process. process should finish after vm is shutdown
pub fn launch_vm(path: String) -> Result<JoinHandle<()>, SetupError>{
    let log_file = File::create("/tmp/windows_vm_log.txt").map_err(|err| SetupError::CouldNotCreateWindowsLogFile(err))?;
    let log_file2 = File::create("/tmp/windows_vm_err_log.txt").map_err(|err| SetupError::CouldNotCreateWindowsLogFile(err))?;
    let mut child = tokio::process::Command::new("virsh").args(["-cqemu:///system", "create", &path, "--console"])
        .stdout(log_file).stderr(log_file2).spawn()
        .map_err(|err| SetupError::FailedToLaunchVM(err))?;
    Ok(tokio::spawn(async move {let _ = child.wait().await;}))
}
