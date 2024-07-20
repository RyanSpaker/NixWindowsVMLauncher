/*
    The server is responsible for creating lines of communication between the cli, session, and vm launcher
    It holds the current state of the system, and uses it to queue actions like starting the vm
*/

use std::{error::Error, fmt::Display, sync::{Arc, Mutex}, task::Poll};
use dbus::{channel::MatchingReceiver, message::MatchRule, nonblock::SyncConnection, MethodErr};
use dbus_crossroads::{Crossroads, IfaceBuilder};
use dbus_tokio::connection::IOResourceError;
use futures::Future;
use hookable::Hookable;
use tokio::task::JoinHandle;
use crate::launcher::{VmState, VmType};

/// Represents all ways the server can fail
#[derive(Debug)]
pub enum ServerError{
    FailedToConnectToSystemBus(dbus::Error),
    FailedToGetName(dbus::Error),
    FailedToFindServerData,
    CouldNotLockServerData
}
impl Display for ServerError{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _ = f.write_str(&match self {
            Self::FailedToConnectToSystemBus(err) => format!("Could not connect to the system dbus: {}", *err),
            Self::FailedToGetName(err) => format!("Could not get the name org.cws.WindowsLauncher on the system dbus: {}", *err),
            Self::FailedToFindServerData => format!("Could not find ServerData"),
            Self::CouldNotLockServerData => format!("Could not lock ServerData")
        });
        Ok(())
    }
}
impl Error for ServerError{}

pub mod hookable{
    use std::task::Waker;

    /// Type that allows one to attach wakers to an object and have the wakers called any time the object is changed
    #[derive(Default, Debug, Clone)]
    pub struct Hookable<T: Default+std::fmt::Debug+Clone>{
        data: T, 
        wakers: Vec<Waker>
    }
    impl<T: Default+std::fmt::Debug+Clone> Hookable<T> {
        pub fn set(&mut self, data: T) {self.data = data; self.wakers.drain(..).for_each(|waker| waker.wake());}
        pub fn get(&self) -> &T {&self.data}
        pub fn hook(&mut self, waker: Waker) {self.wakers.push(waker);}
    }    
}
/// Data held by the server, represents the state of the system
#[derive(Default, Debug, Clone)]
pub struct ServerData{
    pub vm_state: Hookable<VmState>,
    pub vm_type: VmType,
    /// whether or not a user has connected, and a waker to call when the variable changes
    pub user_connected: Hookable<bool>,
    /// path of the mouse to create for the vm
    pub mouse_path: String
}

/// Future which waits for the vm to be launched
pub struct VmLaunchedFuture{
    pub data: Arc<Mutex<ServerData>>
}
impl Future for VmLaunchedFuture{
    type Output = Result<(), ServerError>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        match self.data.lock() {
            Ok(mut guard) => {
                if let VmState::Launched = guard.vm_state.get() {Poll::Ready(Ok(()))}
                else {
                    guard.vm_state.hook(cx.waker().clone());
                    Poll::Pending
                }
            },
            _ => {Poll::Ready(Err(ServerError::CouldNotLockServerData))}
        }
    }
}

/// Future which waits for the vm to be requested to launch
pub struct VmLaunchFuture{
    pub data: Arc<Mutex<ServerData>>
}
impl Future for VmLaunchFuture{
    type Output = Result<(), ServerError>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        match self.data.lock() {
            Ok(mut guard) => {
                if let VmState::Activating = guard.vm_state.get() {Poll::Ready(Ok(()))}
                else {
                    guard.vm_state.hook(cx.waker().clone());
                    Poll::Pending
                }
            },
            _ => {Poll::Ready(Err(ServerError::CouldNotLockServerData))}
        }
    }
}

/// Future which waits for a user session server to connect
pub struct UserConnectedFuture{
    pub data: Arc<Mutex<ServerData>>
}
impl Future for UserConnectedFuture{
    type Output = Result<(), ServerError>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        match self.data.lock() {
            Ok(mut guard) => {
                if *guard.user_connected.get() {Poll::Ready(Ok(()))}
                else {
                    guard.user_connected.hook(cx.waker().clone());
                    Poll::Pending
                }
            },
            _ => {Poll::Ready(Err(ServerError::CouldNotLockServerData))}
        }
    }
}


/// Future which waits for the vm to be shutdown
pub struct VmShutdownFinishedFuture{
    pub data: Arc<Mutex<ServerData>>
}
impl Future for VmShutdownFinishedFuture{
    type Output = Result<(), ServerError>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        match self.data.lock() {
            Ok(mut guard) => {
                if let VmState::Inactive = guard.vm_state.get() {Poll::Ready(Ok(()))}
                else {
                    guard.vm_state.hook(cx.waker().clone());
                    Poll::Pending
                }
            },
            _ => {Poll::Ready(Err(ServerError::CouldNotLockServerData))}
        }
    }
}

/// Future which waits for the vm to be requested to shutdown
pub struct VmShutdownFuture{
    pub data: Arc<Mutex<ServerData>>
}
impl Future for VmShutdownFuture{
    type Output = Result<(), ServerError>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        match self.data.lock() {
            Ok(mut guard) => {
                if let VmState::ShuttingDown = guard.vm_state.get() {Poll::Ready(Ok(()))}
                else {
                    guard.vm_state.hook(cx.waker().clone());
                    Poll::Pending
                }
            },
            _ => {Poll::Ready(Err(ServerError::CouldNotLockServerData))}
        }
    }
}


pub struct ServerStuff{
    pub data: Arc<Mutex<ServerData>>,
    pub handle: JoinHandle<IOResourceError>,
    pub conn: Arc<SyncConnection>
}

pub async fn server() -> Result<ServerStuff, ServerError>{
    let (r, conn) = dbus_tokio::connection::new_system_sync().map_err(|err| ServerError::FailedToConnectToSystemBus(err))?;
    let handle = tokio::spawn(r);
    let data = define_server(conn.clone()).await?;
    Ok(ServerStuff { data, handle, conn })
}

/// setup the dbus server
pub async fn define_server(conn: Arc<SyncConnection>) -> Result<Arc<Mutex<ServerData>>, ServerError>{
    // get name
    conn.request_name("org.cws.WindowsLauncher", false, false, true).await
        .map_err(|err| ServerError::FailedToGetName(err))?;
    // setup crossroads for managing interface
    let mut cr = Crossroads::new();
    cr.set_async_support(Some((conn.clone(), Box::new(|x| {tokio::spawn(x);}))));
    // define main interface
    let manager = cr.register("org.cws.WindowsLauncher.Manager", |b: &mut IfaceBuilder<Arc<Mutex<ServerData>>>| {
        // Tells the system that a user has connected, returns when the vm is ready to launch
        // Returns "" if the vm is not being launched
        b.method_with_cr_async("UserConnected", (), ("VmType",), 
        |mut ctx, cr, _: ()| {
            println!("User Connected!");
            let object = cr.data_mut::<Arc<Mutex<ServerData>>>(&"/org/cws/WindowsLauncher".into()).cloned();
            async move {
                let Some(data) = object else {return ctx.reply(Err(MethodErr::failed(&ServerError::FailedToFindServerData)));};
                let vm_type = if let Ok(mut guard) = data.lock() {
                    if let VmState::Inactive = guard.vm_state.get() {return ctx.reply(Ok(("".to_string(),)));}
                    guard.user_connected.set(true);
                    guard.vm_type.clone()
                } else {return ctx.reply(Err(MethodErr::failed(&ServerError::CouldNotLockServerData)));};
                if let Err(err) = (VmLaunchedFuture{data}).await {return ctx.reply(Err(MethodErr::failed(&err)));}
                ctx.reply(Ok((vm_type.to_string(),)))
            }
        });
        // tells the system to shutdown the vm
        // returns when the vm is fully shutdown
        b.method_with_cr_async("Shutdown", (), (), 
        |mut ctx, cr, _: ()| {
            println!("Shutdown Requested!");
            let object = cr.data_mut::<Arc<Mutex<ServerData>>>(&"/org/cws/WindowsLauncher".into()).cloned();
            async move {
                let Some(data) = object else {return ctx.reply(Err(MethodErr::failed(&ServerError::FailedToFindServerData)));};
                if let Ok(mut guard) = data.lock() {
                    if let VmState::Inactive = guard.vm_state.get() {return ctx.reply(Ok(()));}
                    if let VmState::ShuttingDown = guard.vm_state.get() {} else{
                        guard.vm_state.set(VmState::ShuttingDown);
                    }
                } else {return ctx.reply(Err(MethodErr::failed(&ServerError::CouldNotLockServerData)));}
                if let Err(err) = (VmShutdownFinishedFuture{data}).await {return ctx.reply(Err(MethodErr::failed(&err)));}
                ctx.reply(Ok(()))
            }
        });
        // returns the vm state and type
        b.method::<_, (String, String), _, _>("Query", (), ("VmState", "VmType"), 
        |_, data, _: ()| {
            println!("Query Requested!");
            if let Ok(guard) = data.lock() {
                Ok((guard.vm_state.get().to_string(), guard.vm_type.to_string()))
            }else {Ok(("None".to_string(), "Not Running".to_string()))}
        });
        // tells the server to launch looking glass, returns immediately
        b.method("LaunchLG", ("MousePath",), (), 
        |_, data, (path,): (String,)| {
            println!("LG Launch Requested!");
            if let Ok(mut guard) = data.lock() {
                match guard.vm_state.get() {
                    VmState::Inactive => {
                        guard.vm_type = VmType::LookingGlass;
                        guard.vm_state.set(VmState::Activating);
                        guard.user_connected.set(false);
                        guard.mouse_path = path;
                        Ok(())
                    }, 
                    _ => {
                        Err(MethodErr::failed("Vm Already Launched"))
                    }
                }
            }else{Err(MethodErr::failed("Could not lock ServerData"))}
        });
        // tells the server to launch spice. returns immediately
        b.method("LaunchSpice", ("MousePath",), (), 
        |_, data, (path,): (String,)| {
            println!("Spice Launch Requested!");
            if let Ok(mut guard) = data.lock() {
                match guard.vm_state.get() {
                    VmState::Inactive => {
                        guard.vm_type = VmType::Spice;
                        guard.vm_state.set(VmState::Activating);
                        guard.user_connected.set(false);
                        guard.mouse_path = path;
                        Ok(())
                    }, 
                    _ => {
                        Err(MethodErr::failed("Vm Already Launched"))
                    }
                }
            }else{Err(MethodErr::failed("Could not lock ServerData"))}
        });
    });
    let server_data = Arc::new(Mutex::new(ServerData::default()));
    cr.insert("/org/cws/WindowsLauncher", &[manager, cr.introspectable(), cr.properties()], server_data.clone());
    // start handling interface functions
    conn.start_receive(MatchRule::new_method_call(), Box::new(move |msg, conn| {
        cr.handle_message(msg, conn).unwrap();
        true
    }));
    Ok(server_data)
}