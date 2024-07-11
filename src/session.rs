use std::{collections::HashSet, fs::File, process::Stdio, sync::{Arc, Mutex}, task::{Poll, Waker}};
use tokio::task::JoinHandle;
use dbus::{message::MatchRule, nonblock::{stdintf::org_freedesktop_dbus::Properties, MsgMatch}};
use futures::Future;

use crate::{dbus_manager::DBusConnection, LaunchConfig, SystemState};

#[derive(Debug)]
pub enum SessionError{
    FailedToAddSignalMatchToSystemBus(dbus::Error),
    FailedToGetCurrentSessions(dbus::Error),
    FailedToGetSessionListLock(String),
}
impl ToString for SessionError{
    fn to_string(&self) -> String {
        match self{
            Self::FailedToAddSignalMatchToSystemBus(err) => format!("Failed to add signal handler for SessionNew: {}", *err),
            Self::FailedToGetCurrentSessions(err) => format!("Could not get current sessions from login1: {}", *err),
            Self::FailedToGetSessionListLock(err) => format!("Could not aqcuire the lock for new sessions: {}", *err)
        }
    }
}

#[derive(Default)]
pub struct Sessions{
    pub displays: Arc<Mutex<HashSet<(u32, String, String)>>>,
    pub new_sessions: Arc<Mutex<Vec<String>>>,
    pub display_waker: Arc<Mutex<Vec<Waker>>>,
    signal_handle: Option<MsgMatch>,
    session_waker: Arc<Mutex<Option<Waker>>>,
    pub session_handle: Option<JoinHandle<()>>,
    pub viewer_hadle: Option<JoinHandle<()>>
}
impl Sessions{
    pub async fn create_session_handler(ss: &mut SystemState) -> Result<(), SessionError> {
        // create signal handler for new session, which adds to the new session list
        let mr = MatchRule::new_signal("org.freedesktop.login1.Manager", "SessionNew");
        let session_list = ss.session.new_sessions.clone();
        let waker = ss.session.session_waker.clone();
        let incoming_signal = ss.dbus.system_conn.add_match(mr).await
            .map_err(|err| SessionError::FailedToAddSignalMatchToSystemBus(err))?
            .cb(move |_, (_, path): (String, dbus::Path)| 
        {
            println!("Recieve NewSession Signal: {:?}", path);
            let mut guard = session_list.lock().unwrap();
            guard.push(path.to_string());
            if let Some(waker) = waker.lock().unwrap().take() {waker.wake();}
            println!("Processed NewSession Signal: {:?}", path);
            true
        });
        ss.session.signal_handle = Some(incoming_signal);
        // add all currently known sessions to the session list
        let proxy = dbus::nonblock::Proxy::new(
            "org.freedesktop.login1", 
            "/org/freedesktop/login1", 
            std::time::Duration::from_secs(2), 
            ss.dbus.system_conn.clone()
        );
        let (sessions,): (Vec<(String, u32, String, String, dbus::Path)>,) = proxy.method_call("org.freedesktop.login1.Manager", "ListSessions", ()).await
            .map_err(|err| SessionError::FailedToGetCurrentSessions(err))?;
        let mut guard = ss.session.new_sessions.lock().map_err(|err| SessionError::FailedToGetSessionListLock(err.to_string()))?;
        for (_, _, _, _, path) in sessions.into_iter(){
            if !guard.contains(&path.to_string()) {
                guard.push(path.to_string());
            }
            println!("Found initial session: {:?}", path);
        }
        // spawn handler for new sessions list
        let displays = ss.session.displays.clone();
        let sessions = ss.session.new_sessions.clone();
        let waker = ss.session.session_waker.clone();
        let display_waker = ss.session.display_waker.clone();
        let system_conn = ss.dbus.system_conn.clone();
        let handle = tokio::spawn(async move {
            loop{
                println!("Waiting for new Sessions");
                let new_sessions = NewSessionFuture{sessions: sessions.clone(), waker: waker.clone()}.await;
                println!("Found New Sessions at: {:?}", new_sessions);
                // find the display values
                let mut new_displays = vec![];
                for path in new_sessions.into_iter(){
                    let proxy = dbus::nonblock::Proxy::new(
                        "org.freedesktop.login1", 
                        &path,
                        std::time::Duration::from_secs(2), 
                        system_conn.clone()
                    );
                    let d = match proxy.get::<String>("org.freedesktop.login1.Session", "Display").await{
                        Ok(display) => { if display == "" {continue;} display},
                        Err(err) => {println!("Failed to get display from {}, with err {}", path, err); continue;}
                    };
                    let (u, user_path) = match proxy.get::<(u32,dbus::Path)>("org.freedesktop.login1.Session", "User").await{
                        Ok(result) => {result},
                        Err(err) => {println!("Failed to get user from {}, with err {}", path, err); continue;}
                    };
                    let c = match proxy.get::<String>("org.freedesktop.login1.Session", "Class").await{
                        Ok(class) => {class},
                        Err(err) => {println!("Failed to get class from {}, with err {}", path, err); continue;}
                    };
                    if c == "greeter" {println!("Session rejected for being greeter"); continue;}
                    let n = match proxy.get::<String>("org.freedesktop.login1.Session", "Name").await{
                        Ok(name) => {name},
                        Err(err) => {println!("Failed to get name from {}, with err {}", path, err); continue;}
                    };
                    // find xauth path
                    // connect to user bus
                    let proxy = dbus::nonblock::Proxy::new(
                        "org.freedesktop.login1", 
                        user_path.to_owned(),
                        std::time::Duration::from_secs(2), 
                        system_conn.clone()
                    );
                    let runtime_path = match proxy.get::<String>("org.freedesktop.login1.User", "RuntimePath").await {
                        Ok(path) => {path},
                        Err(err) => {println!("Failed to get runtime from {}, with err {}", user_path, err); continue;}
                    };
                    let addr = "unix:path=".to_string() + runtime_path.as_str() + "/bus";
                    let channel = match DBusConnection::open_channel(u, &addr) {
                        Ok(result) => result, 
                        Err(err) => {println!("Failed to open channel from {}, with err {}", addr, err.to_string()); continue;}
                    };
                    let (resource, conn) = match dbus_tokio::connection::from_channel::<dbus::nonblock::SyncConnection>(channel) {
                        Ok(result) => {result},
                        Err(err) => {println!("Failed to connect to user dbus for {}, with err {}", user_path, err); continue;}
                    };
                    let handle = tokio::spawn(resource);
                    // query systemd1 for environment
                    let proxy = dbus::nonblock::Proxy::new(
                        "org.freedesktop.systemd1", 
                        "/org/freedesktop/systemd1",
                        std::time::Duration::from_secs(2), 
                        conn.clone()
                    );
                    let env_vars = match proxy.get::<Vec<String>>("org.freedesktop.systemd1.Manager", "Environment").await {
                        Ok(result) => result,
                        Err(err) => {println!("Failed to get environment from user systemd {}, with err {}", user_path, err); handle.abort(); continue;}
                    };
                    // find xauth path from env
                    let mut found = false;
                    for var in env_vars.iter() {
                        if var.starts_with("XAUTHORITY="){
                            let xauth_path = var.strip_prefix("XAUTHORITY=").unwrap();
                            println!("Found new Display: {} {} {} {} {}", u, n, c, d, xauth_path);
                            new_displays.push((u, d, xauth_path.to_string()));
                            found = true;
                            break;
                        }
                    }
                    if !found {println!("Failed to find XAUTHORITY in {:?}", env_vars);}
                    handle.abort();
                }
                // add display values
                if new_displays.len() == 0 {continue;}
                if let Ok(mut d) = displays.clone().lock() {
                    d.extend(new_displays.into_iter());
                }else {continue;}
                // wake up any futures
                if let Ok(mut waker_lock) = display_waker.clone().lock() {
                    waker_lock.drain(..).for_each(|waker| waker.wake());
                }
            }
        });
        ss.session.session_handle = Some(handle);
        Ok(())
    }
    pub async fn create_viewer_session_handler(vm_type: LaunchConfig, ss: &mut SystemState) -> Result<(), SessionError> {
        let mut known_displays: HashSet<(u32, String, String)> = HashSet::new();
        let displays = ss.session.displays.clone();
        let wakers = ss.session.display_waker.clone();
        let handle = tokio::spawn(async move {
            let mut processes = vec![];
            loop{
                let new_displays = NewDisplayFuture::new(known_displays.clone(), displays.clone(), wakers.clone()).await;
                for (uid, display, xauth) in new_displays.iter(){
                    let _ = users::switch::set_effective_uid(*uid);
                    let child = match vm_type {
                        LaunchConfig::LG => {
                            let (log, err_log): (Stdio, Stdio) = if let Ok(file) = File::create("/tmp/user_".to_string() + uid.to_string().as_str() + "_lg_log.txt") {
                                (file.try_clone().unwrap().into(), file.into())
                            } else {(Stdio::null(), Stdio::null())};
                            tokio::process::Command::new("looking-glass-client")
                                .args(["-T", "-s", "input:captureOnFocus"])
                                .envs([("DISPLAY", display), ("XAUTHORITY", xauth)])
                                .uid(*uid)
                                .stdout(log).stderr(err_log).spawn()
                        },
                        LaunchConfig::Spice => {
                            let (log, err_log): (Stdio, Stdio) = if let Ok(file) = File::create("/tmp/user_".to_string() + uid.to_string().as_str() + "_virtviewer_log.txt") {
                                (file.try_clone().unwrap().into(), file.into())
                            } else {(Stdio::null(), Stdio::null())};
                            tokio::process::Command::new("virt-viewer")
                                .args(["--connect", "qemu:///system", "windows"])
                                .envs([("DISPLAY", display), ("XAUTHORITY", xauth)])
                                .uid(*uid)
                                .stdout(log).stderr(err_log).spawn()
                        },
                        _ => panic!("How did we get here?")
                    };
                    processes.push(child);
                }
                known_displays.extend(new_displays);
            }
        });
        ss.session.viewer_hadle = Some(handle);
        Ok(())
    }
}

/// future which waits for at least 1 display to be created
pub struct AnyDisplayFuture{
    displays: Arc<Mutex<HashSet<(u32, String, String)>>>,
    waker: Arc<Mutex<Vec<Waker>>>
}
impl AnyDisplayFuture{
    pub fn new(sessions: &mut Sessions) -> Self {
        Self{displays: sessions.displays.clone(), waker: sessions.display_waker.clone()}
    }
}
impl Future for AnyDisplayFuture{
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if let Ok(guard) = self.displays.lock(){
            if !guard.is_empty() {
                Poll::Ready(())
            } else {
                if let Ok(mut waker_guard) = self.waker.lock() {waker_guard.push(cx.waker().to_owned());}
                Poll::Pending
            }
        }else {
            if let Ok(mut waker_guard) = self.waker.lock() {waker_guard.push(cx.waker().to_owned());}
            Poll::Pending
        }
    }
}

/// future which waits for a new display to be added
pub struct NewDisplayFuture{
    known_displays: HashSet<(u32, String, String)>,
    displays: Arc<Mutex<HashSet<(u32, String, String)>>>,
    waker: Arc<Mutex<Vec<Waker>>>
}
impl NewDisplayFuture{
    pub fn new(known_displays: HashSet<(u32, String, String)>, displays: Arc<Mutex<HashSet<(u32, String, String)>>>, waker: Arc<Mutex<Vec<Waker>>>) -> Self{
        Self{known_displays, displays, waker}
    }
}
impl Future for NewDisplayFuture{
    type Output = HashSet<(u32, String, String)>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if let Ok(display_guard) = self.displays.lock(){
            if !display_guard.is_subset(&self.known_displays) {
                Poll::Ready(display_guard.difference(&self.known_displays).cloned().collect::<HashSet<(u32, String, String)>>())
            }else {
                if let Ok(mut waker_guard) = self.waker.lock() {waker_guard.push(cx.waker().to_owned());}
                Poll::Pending
            }
        }else {
            if let Ok(mut waker_guard) = self.waker.lock() {waker_guard.push(cx.waker().to_owned());}
            Poll::Pending
        }
    }
}

/// Future which waits for a new session to be ready to be handled
pub struct NewSessionFuture{
    pub sessions: Arc<Mutex<Vec<String>>>,
    pub waker: Arc<Mutex<Option<Waker>>>
}
impl Future for NewSessionFuture{
    type Output = Vec<String>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        let mut guard = self.sessions.lock().unwrap();
        if guard.is_empty() {
            let _ = self.waker.lock().unwrap().insert(cx.waker().to_owned());
            Poll::Pending
        }else {
            Poll::Ready(guard.drain(..).collect::<Vec<String>>())
        }
    }
}