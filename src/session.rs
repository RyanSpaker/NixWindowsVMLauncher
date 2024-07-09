use std::{collections::HashSet, fs::File, process::Stdio, sync::{Arc, Mutex}, task::{Poll, Waker}};
use tokio::task::JoinHandle;
use dbus::{message::MatchRule, nonblock::{stdintf::org_freedesktop_dbus::Properties, MsgMatch}};
use futures::Future;

use crate::{LaunchConfig, SystemState};

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
    pub displays: Arc<Mutex<HashSet<(u32, String)>>>,
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
            if let Ok(mut guard) = session_list.lock() {
                guard.push(path.to_string());
                if let Ok(Some(waker)) = waker.lock().map(|mut opt| opt.take()) {waker.wake();}
            }
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
            if !guard.contains(&path.to_string()) {guard.push(path.to_string());}
        }
        // spawn handler for new sessions list
        let displays = ss.session.displays.clone();
        let sessions = ss.session.new_sessions.clone();
        let waker = ss.session.session_waker.clone();
        let display_waker = ss.session.display_waker.clone();
        let system_conn = ss.dbus.system_conn.clone();
        let handle = tokio::spawn(async move {
            loop{
                let new_sessions = NewSessionFuture{sessions: sessions.clone(), waker: waker.clone()}.await;
                // find the display values
                let mut new_displays = vec![];
                for path in new_sessions.into_iter(){
                    let proxy = dbus::nonblock::Proxy::new(
                        "org.freedesktop.login1", 
                        path,
                        std::time::Duration::from_secs(2), 
                        system_conn.clone()
                    );
                    let d = match proxy.get::<(String,)>("org.freedesktop.login1.Session", "Display").await{
                        Ok((display,)) => {display},
                        _ => {continue;}
                    };
                    let u = match proxy.get::<(u32,dbus::Path)>("org.freedesktop.login1.Session", "User").await{
                        Ok((user, _)) => {user},
                        _ => {continue;}
                    };
                    new_displays.push((u, d));
                }
                // add display values
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
        let mut known_displays: HashSet<(u32, String)> = HashSet::new();
        let displays = ss.session.displays.clone();
        let wakers = ss.session.display_waker.clone();
        let handle = tokio::spawn(async move {
            let mut processes = vec![];
            loop{
                let new_displays = NewDisplayFuture::new(known_displays.clone(), displays.clone(), wakers.clone()).await;
                for (uid, display) in new_displays.iter(){
                    let _ = users::switch::set_effective_uid(*uid);
                    let child = match vm_type {
                        LaunchConfig::LG => {
                            let log: Stdio = if let Ok(file) = File::create("/tmp/user_".to_string() + uid.to_string().as_str() + "_lg_log.txt") {file.into()} else {Stdio::null()};
                            tokio::process::Command::new("looking-glass-client")
                                .args(["-T", "-s", "input:captureOnFocus"])
                                .env("DISPLAY", display)
                                .stdout(log).spawn()
                        },
                        LaunchConfig::Spice => {
                            let log: Stdio = if let Ok(file) = File::create("/tmp/user_".to_string() + uid.to_string().as_str() + "_virtviewer_log.txt") {file.into()} else {Stdio::null()};
                            tokio::process::Command::new("virt-viewer")
                                .args(["--connect", "qemu:///system", "windows"])
                                .env("DISPLAY", display)
                                .stdout(log).spawn()
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
    displays: Arc<Mutex<HashSet<(u32, String)>>>,
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
    known_displays: HashSet<(u32, String)>,
    displays: Arc<Mutex<HashSet<(u32, String)>>>,
    waker: Arc<Mutex<Vec<Waker>>>
}
impl NewDisplayFuture{
    pub fn new(known_displays: HashSet<(u32, String)>, displays: Arc<Mutex<HashSet<(u32, String)>>>, waker: Arc<Mutex<Vec<Waker>>>) -> Self{
        Self{known_displays, displays, waker}
    }
}
impl Future for NewDisplayFuture{
    type Output = HashSet<(u32, String)>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if let Ok(display_guard) = self.displays.lock(){
            if !display_guard.is_subset(&self.known_displays) {
                Poll::Ready(display_guard.difference(&self.known_displays).cloned().collect::<HashSet<(u32, String)>>())
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
        if let Ok(mut guard) = self.sessions.lock() {
            if guard.is_empty() {
                if let Ok(mut waker_guard) = self.waker.lock() {
                    let _ = waker_guard.insert(cx.waker().to_owned());
                }
                Poll::Pending
            }else {
                Poll::Ready(guard.drain(..).collect::<Vec<String>>())
            }
        }else {Poll::Pending}
    }
}