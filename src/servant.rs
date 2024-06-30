use std::{process::Child, string::FromUtf8Error, sync::{atomic::AtomicBool, Arc}, time::Duration};
use dbus::{message::MatchRule, nonblock};
use dbus_tokio::connection::{self, IOResourceError};
use tokio::time::sleep;

/// Enum representing all failstates for a client app
#[derive(Debug)]
pub enum ServantError{
    FailedToConnectToSystemBus(dbus::Error),
    FailedToCallDBusMethod(dbus::Error),
    FailedToCallXInputList(std::io::Error),
    FailedToParseDataFromXInput,
    FailedToToggleMouse(std::io::Error),
    FailedToStartLookingGlass(std::io::Error),
    FailedToStartVirtViewer(std::io::Error),
    LostConnectionToDBus(IOResourceError),
    FailedToConvertOutputToString(FromUtf8Error),
    FailedToFindMouseIDFromXInput
}
impl ToString for ServantError{
    fn to_string(&self) -> String {
        match self {
            ServantError::FailedToConnectToSystemBus(err) => format!("Failed to connect to the system dbus: {}", *err),
            ServantError::FailedToCallDBusMethod(err) => format!("Failed to call the UserReady method on the system bus: {}", *err),
            ServantError::FailedToCallXInputList(err) => format!("Call to xinput list failed: {}", *err),
            ServantError::FailedToParseDataFromXInput => format!("Parsing data from xinput failed, unknown cause"),
            ServantError::FailedToToggleMouse(err) => format!("Using xinput to toggle mouse failed: {}", *err),
            ServantError::FailedToStartLookingGlass(err) => format!("Failed to start looking glass client: {}", *err),
            ServantError::FailedToStartVirtViewer(err) => format!("Failed to start virt viewer: {}", *err),
            ServantError::LostConnectionToDBus(err) => format!("Connection to system bus was dropped: {}", *err),
            ServantError::FailedToConvertOutputToString(err) => format!("could not convert output of xinput list to a string: {}", err),
            ServantError::FailedToFindMouseIDFromXInput => format!("Could not find the correct mouse id from xinput")
        }
    }
}

/// App logic for client apps which run for each user, when they log in
pub async fn client_app() -> Result<(), ServantError>{
    // connect to system bus
    let (resource, conn) = connection::new_system_sync()
        .map_err(|err| ServantError::FailedToConnectToSystemBus(err))?;
    let handle = tokio::spawn(async {
        resource.await
    });
    // setup proxy
    let proxy = nonblock::Proxy::new("org.cowsociety.vmlauncher", "/org/cowsociety/vmlauncher", std::time::Duration::from_secs(30), conn.clone());
    // tell server that we are ready
    // wait for response
    let (id, vm_type): (u32, String) = proxy.method_call("org.cowsociety.vmlauncher.Manager", "UserReady", ()).await
        .map_err(|err| ServantError::FailedToCallDBusMethod(err))?;
    // xinput changes
    toggle_mouse(id, false)?;
    // launch looking glass
    let mut child = if vm_type == "lg" {
        std::process::Command::new("looking-glass-client").args(["-T", "-s", "input:captureOnFocus"]).spawn()
            .map_err(|err| ServantError::FailedToStartLookingGlass(err))?
    } else {
        std::process::Command::new("virt-viewer").args(["--connect", "qemu:///system", "windows"]).spawn()
            .map_err(|err| ServantError::FailedToStartVirtViewer(err))?
    };
    // wait, respond to shutdown signals from dbus, or if  looking glass closes
    let signal_recieved: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let signal_handle = signal_recieved.clone();
    let mr = MatchRule::new_signal("org.cowsociety.vmlauncher.Manager", "Shutdown");
    let incoming_signal = conn.add_match(mr).await.unwrap().cb(move |_, (_,): (String,)| {
        signal_handle.store(true, std::sync::atomic::Ordering::Relaxed);
        true
    });
    loop{
        sleep(Duration::from_secs(5)).await;
        if check_futures(&mut child, signal_recieved.clone()) {break;}
        if handle.is_finished() {
            return Err(ServantError::LostConnectionToDBus(handle.await.unwrap()));
        }
    }
    // undo xinput changes
    toggle_mouse(id, true)?;
    conn.remove_match(incoming_signal.token()).await.unwrap();
    Ok(())
}

/// Helper function, determines if the looking glass client has closed or if a shutdown signal has been sent
pub fn check_futures(child: &mut Child, signal: Arc<AtomicBool>) -> bool{
    if let Ok(Some(_)) = child.try_wait() {return true;}
    if signal.load(std::sync::atomic::Ordering::Relaxed) == true {return true;}
    false
}

// Helper function to take an input id and use xinput to disable/enable the corresponding mouse
pub fn toggle_mouse(input_id: u32, enable: bool) -> Result<(), ServantError> {
    let event_string = "event".to_owned() + &input_id.to_string();
    let output = std::process::Command::new("xinput").args(["list", "--id-only"]).output()
        .map_err(|err| ServantError::FailedToCallXInputList(err))?.stdout;
    let output_text = String::from_utf8(output).map_err(|err| ServantError::FailedToConvertOutputToString(err))?;
    let ids = output_text.split("\n").filter_map(|id| id.parse::<u32>().ok()).collect::<Vec<u32>>();
    let id = ids.into_iter().find(|id| {
        std::process::Command::new("xinput").args(["list-props", id.to_string().as_str()]).output()
            .ok().map(|output| String::from_utf8(output.stdout).ok()).flatten().is_some_and(|props| props.contains(event_string.as_str()))
    }).ok_or(ServantError::FailedToFindMouseIDFromXInput)?;
    std::process::Command::new("xinput").args([(if enable {"--enable"} else {"--disable"}).to_string(), id.to_string()]).output()
        .map_err(|err| ServantError::FailedToToggleMouse(err))?;
    if enable {println!("Enabled mouse {}", id);} else {println!("Disabled mouse {}", id);}
    Ok(())
}