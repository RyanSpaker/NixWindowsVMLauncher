use std::{env::{self, VarError}, fs::File, io::{Read, Write}, path::Path, time::Duration};
use crate::{dbus_server::{DBusError, DBusState}, LaunchConfig, SystemState};

#[derive(Debug)]
pub enum SetupError{
    BusError(DBusError),
    FailedToUnloadKernelModule(String, std::io::Error),
    FailedToLoadKernelModule(String, std::io::Error),
    FailedToDetachGPU(String, std::io::Error),
    FailedToAttachGPU(String, std::io::Error),
    FailedToReadCPUDir(std::io::Error),
    FailedToCreateCPUFile(std::io::Error),
    FailedToWriteCPUGovernor(std::io::Error),
    FailedToGetEnvVar(String, VarError),
    FailedToReadXmlFile(std::io::Error),
    FailedToCreateTmpXmlFile(std::io::Error),
    FailedToWriteTmpXmlFile(std::io::Error),
    ModProbeUnloadFailed(String, String, String)
}
impl ToString for SetupError{
    fn to_string(&self) -> String {
        match self{
            SetupError::BusError(err) => err.to_string(),
            SetupError::FailedToUnloadKernelModule(name, err) => format!("Failed to unload kernel module {} with error: {}", *name, *err),
            SetupError::FailedToLoadKernelModule(name, err) => format!("Failed to load kernel module {} with error: {}", *name, *err),
            SetupError::FailedToDetachGPU(pci, err) => format!("Failed to detach pci_device {} with error: {}", *pci, *err),
            SetupError::FailedToAttachGPU(pci, err) => format!("Failed to reattach pci_device {} with error: {}", *pci, *err),
            SetupError::FailedToReadCPUDir(err) => format!("Failed to read the sys/device/system/cpu dir: {}", *err),
            SetupError::FailedToCreateCPUFile(err) => format!("Failed to open as write file at /sys/device/system/cpu/cpu*/cpufreq/scaling_governor: {}", *err),
            SetupError::FailedToWriteCPUGovernor(err) => format!("Failed to write to file at /sys/device/system/cpu/cpu*/cpufreq/scaling_governor: {}", *err),
            SetupError::FailedToGetEnvVar(name, err) => format!("Failed to get env var {} with error: {}", *name, *err),
            SetupError::FailedToReadXmlFile(err) => format!("Failed to read vm xml file: {}", *err),
            SetupError::FailedToCreateTmpXmlFile(err) => format!("Failed to create the /tmp/windows.xml file: {}", *err),
            SetupError::FailedToWriteTmpXmlFile(err) => format!("Failed to write to /tmp/windows.xml: {}", *err),
            SetupError::ModProbeUnloadFailed(name, out, err) => format!("Failed to unload module {} with stdout: {}, and stderr: {}", *name, *out, *err)
        }
    }
}

pub fn get_vm_xml(vm_type: LaunchConfig) -> Result<String, SetupError>{
    let xml_path = env::var(match vm_type {
        LaunchConfig::LG => "WINDOWS_LG_XML_PATH",
        LaunchConfig::Spice => "WINDOWS_SPICE_XML_PATH",
        _ => panic!("How did we get here?")
    }).map_err(|err| SetupError::FailedToGetEnvVar(
        match vm_type {
            LaunchConfig::LG => "WINDOWS_LG_XML_PATH",
            LaunchConfig::Spice => "WINDOWS_SPICE_XML_PATH",
            _ => ""
        }.to_string(),
        err
    ))?;
    let mut xml_string = String::with_capacity(10000);
    File::open(xml_path).unwrap().read_to_string(&mut xml_string).map_err(|err| SetupError::FailedToReadXmlFile(err))?;
    return Ok(xml_string);
}

pub fn write_xml(xml: String) -> Result<(), SetupError>{
    File::create("/tmp/windows.xml").map_err(|err| SetupError::FailedToCreateTmpXmlFile(err))?
        .write(xml.as_bytes()).map_err(|err| SetupError::FailedToWriteTmpXmlFile(err))?;
    Ok(())
}

pub async fn unload_gpu(dbus_state: &mut DBusState, ss: &mut SystemState) -> Result<(), SetupError> {
    // Stop display manager
    println!("Stopping Display Manager");
    stop_display_manager(dbus_state).await?;
    ss.dp_stopped = true;
    // Stop pipewire
    println!("Stopping pipewire");
    stop_pipewire().await;
    ss.pw_stopped = true;
    // Unload kernel Modules
    println!("Unloading nvidia Modules");
    tokio::time::sleep(Duration::from_secs(1)).await;
    unload_nvidia_modules(ss)?;
    // Disconnect GPU
    println!("Disconnecting GPU");
    disconnect_gpu(ss)?;
    // Load VFIO drivers
    println!("Loading VFIO modules");
    load_vfio_modules()?;
    ss.vfio_loaded = true;
    // Restart pipewire service
    println!("Starting Pipewire");
    start_pipewire().await;
    ss.pw_stopped = false;
    // restart display manager
    println!("Starting Display Manager");
    start_display_manager(dbus_state).await?;
    ss.dp_stopped = false;
    Ok(())
}

pub async fn reattach_gpu(dbus_state: &mut DBusState, ss: &mut SystemState) -> Result<(), SetupError> {
    // unload vfio
    println!("Unloading VFIO modules");
    unload_vfio_modules()?;
    ss.vfio_loaded = false;
    // reattach gpu
    println!("Connecting GPU");
    connect_gpu(ss)?;
    // load nvidia
    println!("Loading Nvidia Modules");
    load_nvidia_modules(ss)?;
    // restart pipewire and display manager
    println!("Restarting Display Manager");
    restart_display_manager(dbus_state).await?;
    ss.dp_reset = true;
    println!("Restarting Pipewire");
    restart_pipewire().await;
    ss.pw_reset = true;
    Ok(())
}

pub async fn cleanup(mut dbus_state: DBusState, ss: SystemState) {
    // use system state to undo modifications
    // undo cpu governor
    if ss.power_rule_set {
        if let Ok(read) = Path::new("/sys/devices/system/cpu/").read_dir(){
            read.into_iter().flatten().filter_map(|dir| {
                if dir.file_type().unwrap().is_file() || !dir.file_name().to_str().unwrap().starts_with("cpu") {return None;}
                Some(dir.path().join("cpufreq/scaling_governor"))
            }).for_each(|cpu_path| {
                if let Ok(mut file) = File::create(cpu_path) {let _ = file.write("powersave".as_bytes());}
            });
        }
    }
    // unload vfio if needed
    if ss.vfio_loaded {
        let _ = super::call_command("modprobe", ["-r", "vfio-pci"]);
    }
    // reconnect gpu
    if ss.gpu_disconnected.0 {let _ = super::call_command("virsh", ["nodedev-reattach", "pci_0000_01_00_0"]);}
    if ss.gpu_disconnected.1 {let _ = super::call_command("virsh", ["nodedev-reattach", "pci_0000_01_00_1"]);}
    // load nvidia if needed
    if ss.nvidia_unloaded.3 {let _ = super::call_command("modprobe", ["nvidia"]);}
    if ss.nvidia_unloaded.2 {let _ = super::call_command("modprobe", ["nvidia_modeset"]);}
    if ss.nvidia_unloaded.1 {let _ = super::call_command("modprobe", ["nvidia_uvm"]);}
    if ss.nvidia_unloaded.0 {let _ = super::call_command("modprobe", ["nvidia_drm"]);}
    // actions that require the system bus
    if dbus_state.check_system_bus().await.is_ok() {
        // undo cpu limiting
        if ss.cpus_limited.0 || ss.cpus_limited.1 || ss.cpus_limited.2 {
            let _ =dbus_state.call_system_method::<_, ()>(
                "org.freedesktop.systemd1", 
                "/org/freedesktop/systemd1/unit/user_2eslice", 
                "org.freedesktop.Unit", 
                "SetProperties", (true, vec![("AllowedCPUs", vec![255_u8, 255_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8])])
            ).await;
            let _ =dbus_state.call_system_method::<_, ()>(
                "org.freedesktop.systemd1", 
                "/org/freedesktop/systemd1/unit/system_2eslice", 
                "org.freedesktop.Unit", 
                "SetProperties", (true, vec![("AllowedCPUs", vec![255_u8, 255_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8])])
            ).await;
            let _ = dbus_state.call_system_method::<_, ()>(
                "org.freedesktop.systemd1", 
                "/org/freedesktop/systemd1/unit/unit_2escope", 
                "org.freedesktop.Unit", 
                "SetProperties", (true, vec![("AllowedCPUs", vec![255_u8, 255_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8])])
            ).await;
        }
        // restart display manager
        if !ss.dp_reset {
            if ss.dp_stopped {
                let _ = dbus_state.call_system_method::<_, (String,)>(
                    "org.freedesktop.systemd1", 
                    "/org/freedesktop/systemd1", 
                    "org.freedesktop.systemd1.Manager", 
                    "StartUnit", 
                    ("display-manager.service", "replace")
                ).await;
            }else {
                let _ = dbus_state.call_system_method::<_, (String,)>(
                    "org.freedesktop.systemd1", 
                    "/org/freedesktop/systemd1", 
                    "org.freedesktop.systemd1.Manager", 
                    "RestartUnit", 
                    ("display-manager.service", "replace")
                ).await;
            }
        }
        // restart pipewire
        if !ss.pw_reset{
            if ss.pw_stopped {
                start_pipewire().await;
            }else{
                restart_pipewire().await;
            }
        }
    }
}

pub async fn performance_enhancements(dbus_state: &mut DBusState, ss: &mut SystemState) -> Result<(), SetupError> {
    dbus_state.call_system_method::<_, ()>(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1/unit/user_2eslice", 
        "org.freedesktop.Unit", 
        "SetProperties", (true, vec![("AllowedCPUs", vec![0_u8, 240_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8])])
    ).await.map_err(|err| SetupError::BusError(err))?;
    ss.cpus_limited.0 = true;
    dbus_state.call_system_method::<_, ()>(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1/unit/system_2eslice", 
        "org.freedesktop.Unit", 
        "SetProperties", (true, vec![("AllowedCPUs", vec![0_u8, 240_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8])])
    ).await.map_err(|err| SetupError::BusError(err))?;
    ss.cpus_limited.1 = true;
    dbus_state.call_system_method::<_, ()>(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1/unit/unit_2escope", 
        "org.freedesktop.Unit", 
        "SetProperties", (true, vec![("AllowedCPUs", vec![0_u8, 240_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8])])
    ).await.map_err(|err| SetupError::BusError(err))?;
    ss.cpus_limited.2 = true;
    Path::new("/sys/devices/system/cpu/").read_dir().map_err(|err| SetupError::FailedToReadCPUDir(err))?
        .into_iter().flatten().filter_map(|dir| {
            if dir.file_type().unwrap().is_file() || !dir.file_name().to_str().unwrap().starts_with("cpu") {return None;}
            Some(dir.path().join("cpufreq/scaling_governor"))
        }).for_each(|cpu_path| {
            if let Ok(mut file) = File::create(cpu_path) {let _ = file.write("performance".as_bytes());}
        });
    ss.power_rule_set = true;
    Ok(())
}

pub async fn undo_performance_enhancements(dbus_state: &mut DBusState, ss: &mut SystemState) -> Result<(), SetupError> {
    dbus_state.call_system_method::<_, ()>(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1/unit/user_2eslice", 
        "org.freedesktop.Unit", 
        "SetProperties", (true, vec![("AllowedCPUs", vec![255_u8, 255_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8])])
    ).await.map_err(|err| SetupError::BusError(err))?;
    ss.cpus_limited.0 = false;
    dbus_state.call_system_method::<_, ()>(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1/unit/system_2eslice", 
        "org.freedesktop.Unit", 
        "SetProperties", (true, vec![("AllowedCPUs", vec![255_u8, 255_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8])])
    ).await.map_err(|err| SetupError::BusError(err))?;
    ss.cpus_limited.1 = false;
    dbus_state.call_system_method::<_, ()>(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1/unit/unit_2escope", 
        "org.freedesktop.Unit", 
        "SetProperties", (true, vec![("AllowedCPUs", vec![255_u8, 255_u8, 15_u8, 0_u8, 0_u8, 0_u8, 0_u8, 0_u8])])
    ).await.map_err(|err| SetupError::BusError(err))?;
    ss.cpus_limited.2 = false;
    Path::new("/sys/devices/system/cpu/").read_dir().map_err(|err| SetupError::FailedToReadCPUDir(err))?
        .into_iter().flatten().filter_map(|dir| {
            if dir.file_type().unwrap().is_file() || !dir.file_name().to_str().unwrap().starts_with("cpu") {return None;}
            Some(dir.path().join("cpufreq/scaling_governor"))
        }).for_each(|cpu_path| {
            if let Ok(mut file) = File::create(cpu_path) {let _ = file.write("powersave".as_bytes());}
        });
    ss.power_rule_set = false;
    Ok(())
}

pub async fn stop_display_manager(dbus_state: &mut DBusState) -> Result<(), SetupError> {
    dbus_state.call_system_method::<_, (dbus::Path,)>(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1", 
        "org.freedesktop.systemd1.Manager", 
        "StopUnit", 
        ("display-manager.service", "replace")
    ).await.map(|_| ()).map_err(|err| SetupError::BusError(err))
}

pub async fn start_display_manager(dbus_state: &mut DBusState) -> Result<(), SetupError> {
    dbus_state.call_system_method::<_, (dbus::Path,)>(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1", 
        "org.freedesktop.systemd1.Manager", 
        "StartUnit", 
        ("display-manager.service", "replace")
    ).await.map(|_| ()).map_err(|err| SetupError::BusError(err))
}

pub async fn restart_display_manager(dbus_state: &mut DBusState) -> Result<(), SetupError> {
    dbus_state.call_system_method::<_, (dbus::Path,)>(
        "org.freedesktop.systemd1", 
        "/org/freedesktop/systemd1", 
        "org.freedesktop.systemd1.Manager", 
        "RestartUnit", 
        ("display-manager.service", "replace")
    ).await.map(|_|()).map_err(|err| SetupError::BusError(err))
}

pub async fn stop_pipewire() {
    Path::new("/run/user").read_dir().unwrap().flatten().for_each(|user| {
        let name = user.file_name().into_string().unwrap();
        let output = super::call_command("systemctl", ["--user", ("--machine=".to_string() + name.as_str() + "@.host").as_str(), "stop", "pipewire.socket"]);
        if let Ok(out) = output{
            if out.status.success() {
                println!("Successfully stopped user {}'s pipewire socket!", name);
            }
        }
    });
}

pub async fn start_pipewire() {
    Path::new("/run/user").read_dir().unwrap().flatten().for_each(|user| {
        let name = user.file_name().into_string().unwrap();
        let output = super::call_command("systemctl", ["--user", ("--machine=".to_string() + name.as_str() + "@.host").as_str(), "stop", "pipewire.socket"]);
        if let Ok(out) = output{
            if out.status.success() {
                println!("Successfully started user {}'s pipewire socket!", name);
            }
        }
    });
}

pub async fn restart_pipewire() {
    Path::new("/run/user").read_dir().unwrap().flatten().for_each(|user| {
        let name = user.file_name().into_string().unwrap();
        let output = super::call_command("systemctl", ["--user", ("--machine=".to_string() + name.as_str() + "@.host").as_str(), "stop", "pipewire.socket"]);
        if let Ok(out) = output{
            if out.status.success() {
                println!("Successfully restarted user {}'s pipewire socket!", name);
            }
        }
    });
}

pub fn unload_nvidia_modules(ss: &mut SystemState) -> Result<(), SetupError> {
    let out = super::call_command("modprobe", ["-r", "nvidia_uvm"])
        .map_err(|err| SetupError::FailedToUnloadKernelModule("nvidia_uvm".to_string(), err))?;
    if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
        return Err(SetupError::ModProbeUnloadFailed("nvidia_uvm".to_string(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap()))
    }
    println!("Unloading nvidia_uvm with status {}, out {}, and err {}", out.status.to_string(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap());
    ss.nvidia_unloaded.0 = true;
    let out = super::call_command("modprobe", ["-r", "nvidia_drm"])
        .map_err(|err| SetupError::FailedToUnloadKernelModule("nvidia_drm".to_string(), err))?;
    if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
        return Err(SetupError::ModProbeUnloadFailed("nvidia_drm".to_string(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap()))
    }
    println!("Unloading nvidia_drm with status {}, out {}, and err {}", out.status.to_string(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap());
    ss.nvidia_unloaded.1 = true;
    let out = super::call_command("modprobe", ["-r", "nvidia_modeset"])
        .map_err(|err| SetupError::FailedToUnloadKernelModule("nvidia_modeset".to_string(), err))?;
    if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
        return Err(SetupError::ModProbeUnloadFailed("nvidia_modeset".to_string(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap()))
    }
    println!("Unloading nvidia_modeset with status {}, out {}, and err {}", out.status.to_string(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap());
    ss.nvidia_unloaded.2 = true;
    let out = super::call_command("modprobe", ["-r", "nvidia"])
        .map_err(|err| SetupError::FailedToUnloadKernelModule("nvidia".to_string(), err))?;
    if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
        return Err(SetupError::ModProbeUnloadFailed("nvidia".to_string(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap()))
    }
    println!("Unloading nvidia with status {}, out {}, and err {}", out.status.to_string(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap());
    ss.nvidia_unloaded.3 = true;
    Ok(())
}

pub fn load_nvidia_modules(ss: &mut SystemState) -> Result<(), SetupError> {
    super::call_command("modprobe", ["nvidia"])
        .map_err(|err| SetupError::FailedToLoadKernelModule("nvidia".to_string(), err))?;
    ss.nvidia_unloaded.3 = false;
    super::call_command("modprobe", ["nvidia_modeset"])
        .map_err(|err| SetupError::FailedToLoadKernelModule("nvidia_modeset".to_string(), err))?;
    ss.nvidia_unloaded.2 = false;
    super::call_command("modprobe", ["nvidia_drm"])
        .map_err(|err| SetupError::FailedToLoadKernelModule("nvidia_drm".to_string(), err))?;
    ss.nvidia_unloaded.1 = false;
    super::call_command("modprobe", ["nvidia_uvm"])
        .map_err(|err| SetupError::FailedToLoadKernelModule("nvidia_uvm".to_string(), err))?;
    ss.nvidia_unloaded.0 = false;
    Ok(())
}

pub fn disconnect_gpu(ss: &mut SystemState) -> Result<(), SetupError> {
    // Disconnect nvidia gpu
    println!("detach 1: {:?}", super::call_command("virsh", ["nodedev-detach", "pci_0000_01_00_0"])
        .map_err(|err| SetupError::FailedToDetachGPU("pci_0000_01_00_0".to_string(), err))?);
    ss.gpu_disconnected.0 = true;
    println!("detach 2: {:?}", super::call_command("virsh", ["nodedev-detach", "pci_0000_01_00_1"])
        .map_err(|err| SetupError::FailedToDetachGPU("pci_0000_01_00_1".to_string(), err))?);
    ss.gpu_disconnected.1 = true;
    Ok(())
}

pub fn connect_gpu(ss: &mut SystemState) -> Result<(), SetupError> {
    // connect nvidia gpu
    super::call_command("virsh", ["nodedev-reattach", "pci_0000_01_00_0"])
        .map_err(|err| SetupError::FailedToAttachGPU("pci_0000_01_00_0".to_string(), err))?;
    ss.gpu_disconnected.0 = false;
    super::call_command("virsh", ["nodedev-reattach", "pci_0000_01_00_1"])
        .map_err(|err| SetupError::FailedToAttachGPU("pci_0000_01_00_1".to_string(), err))?;
    ss.gpu_disconnected.1 = false;
    Ok(())
}

pub fn load_vfio_modules() -> Result<(), SetupError>{
    super::call_command("modprobe", ["vfio-pci"])
        .map_err(|err| SetupError::FailedToLoadKernelModule("vfio-pci".to_string(), err)).map(|_| ())
}

pub fn unload_vfio_modules() -> Result<(), SetupError>{
    let out = super::call_command("modprobe", ["-r", "vfio-pci"])
        .map_err(|err| SetupError::FailedToUnloadKernelModule("vfio-pci".to_string(), err))?;
    if out.stderr.len() > 0 && !String::from_utf8(out.stderr.clone()).unwrap().contains("not found") {
        return Err(SetupError::ModProbeUnloadFailed("vfio-pci".to_string(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap()))
    }
    Ok(())
}

