// This module is responsible for creating and managing dbus server connections
use std::{ffi::OsString, future::Future, sync::{atomic::{AtomicBool, AtomicU32, Ordering}, Arc, Mutex}, task::{Poll, Waker}};
use dbus::{arg::{AppendAll, ReadAll}, channel::{MatchingReceiver, Sender}, message::MatchRule, nonblock::SyncConnection, strings::{Interface, Member}, Message, Path};
use dbus_crossroads::{Crossroads, IfaceBuilder};
use dbus_tokio::connection::IOResourceError;
use futures::task::AtomicWaker;
use tokio::task::JoinHandle;

use crate::LaunchConfig;

/// represents the different ways a system bus method call can fail
#[derive(Debug)]
pub enum DBusError{
    SystemBusLost(IOResourceError),
    MethodCallError(dbus::Error),
    RequestNameFailed(dbus::Error),
    FailedToSendShutdownSignal,
    FailedToReadRunUserDir(std::io::Error),
    FailedToTurnFilenameIntoString(OsString),
    FailedToOpenUserChannel(dbus::Error),
    FailedToRegisterUserChannel(dbus::Error),
    FailedToOpenDBusSession(dbus::Error),
    FailedToConnectToSystemBus(dbus::Error)
}
impl ToString for DBusError{
    fn to_string(&self) -> String {
        match self{
            DBusError::SystemBusLost(err) => format!("Connection to System Bus was lost: {}", *err),
            DBusError::MethodCallError(err) => format!("Could not call dbus method: {}", *err),
            DBusError::RequestNameFailed(err) => format!("Failed to request name from system bus: {}", *err),
            DBusError::FailedToSendShutdownSignal => format!("Failed to send shutdown signal"),
            DBusError::FailedToReadRunUserDir(err) => format!("Failed to read /run/user directory: {}", *err),
            DBusError::FailedToTurnFilenameIntoString(err) => format!("Could not convert filename to string: {:?}", *err),
            DBusError::FailedToOpenUserChannel(err) => format!("Could not open user channel: {}", *err),
            DBusError::FailedToRegisterUserChannel(err) => format!("Could not register user channel: {}", *err),
            DBusError::FailedToOpenDBusSession(err) => format!("Failed to open dbus session: {}", *err),
            DBusError::FailedToConnectToSystemBus(err) => format!("Failed to connect to system bus: {}", *err),
        }
    }
}

/// Struct to hold all connection resources.
/// System connection dropping is an immediate failure. User connections dropping can be ignored.
pub struct DBusState{
    pub system_conn: Arc<SyncConnection>,
    pub system_handle: Box<JoinHandle<IOResourceError>>,
    pub communicator: Option<Communicator>
}
impl DBusState{
    /// Call a method on the system bus
    pub async fn call_system_method<A: AppendAll, R: ReadAll + 'static>(&mut self, dest: &str, path: &str, interface: &str, method: &str, args: A) -> Result<R, DBusError>{
        self.check_system_bus().await?;
        let proxy = dbus::nonblock::Proxy::new(
            dest, 
            path, 
            std::time::Duration::from_secs(2), 
            self.system_conn.clone()
        );
        proxy.method_call(interface, method, args).await.map_err(|err| DBusError::MethodCallError(err))
    }
    /// Check status of system bus
    pub async fn check_system_bus(&mut self) -> Result<(), DBusError>{
        if self.system_handle.is_finished() {
            Err(DBusError::SystemBusLost(self.system_handle.as_mut().await.unwrap()))
        }else {Ok(())}
    }
    /// Create the dbus service
    pub async fn create_dbus_service(&mut self, config: LaunchConfig) -> Result<(), DBusError> {
        self.check_system_bus().await?;
        // Get name
        self.system_conn.request_name("org.cowsociety.vmlauncher", false, false, false).await
            .map_err(|err| DBusError::RequestNameFailed(err))?;
        // Setup crossroads
        let mut cr = Crossroads::new();
        cr.set_async_support(Some((self.system_conn.clone(), Box::new(|x| {tokio::spawn(x);}))));
        let vm_type = match config {LaunchConfig::LG => "lg", LaunchConfig::Spice => "spice", _ => panic!("Uh Oh")}.to_string();
        // setup interface methods
        let interface = cr.register("org.cowsociety.vmlauncher.Manager", move |b: &mut IfaceBuilder<Communicator>| {
            // Sent when the windows vm is shutdown
            b.signal::<(), &str>("Shutdown", ());
            // Called by clients to inform the server that a user is ready
            b.method_with_cr_async("UserReady", (), ("mouse_input_id", "vm_type"), move |mut ctx, cr, _: ()| {
                // Grab a copy of the communicator
                let mut data: Communicator = cr.data_mut::<Communicator>(&dbus::Path::new("/org/cowsociety/vmlauncher").unwrap()).unwrap().clone();
                // Ping the communicator
                data.user_ready();
                // wait for vm to launch, getting the mouse input id for use in the user client (disabling trackpad in xinput)
                let value = vm_type.clone();
                async move {
                    let mouse_id = VmLaunchFuture(data.clone()).await;
                    return ctx.reply(Ok((mouse_id, value)));
                }     
            });
            // Called by clients to inform the server that a user is no longer using the vm
            b.method_with_cr("UserClosed", (), (), |_, cr, _: ()| {
                let mut data: Communicator = cr.data_mut::<Communicator>(&dbus::Path::new("/org/cowsociety/vmlauncher").unwrap()).unwrap().clone();
                data.user_closed();
                return Ok(());
            });
        });
        self.communicator = Some(Communicator{
            user_count: Arc::new(AtomicU32::new(0)), 
            user_ready_waker: Arc::new(AtomicWaker::default()),
            no_user_waker: Arc::new(AtomicWaker::default()), 
            vm_launched: Arc::new(AtomicBool::new(false)),
            mouse_input_id: Arc::new(AtomicU32::new(10000)),
            vm_launched_wakers: Arc::new(Mutex::new(vec![]))
        });
        cr.insert("/org/cowsociety/vmlauncher", &[interface], self.communicator.as_ref().unwrap().clone());
        self.system_conn.start_receive(MatchRule::new_method_call(), Box::new(move |msg, conn| {
            cr.handle_message(msg, conn).unwrap();
            true
        }));
        Ok(())
    }
    /// Tell the communicator that a user is ready
    pub async fn inform_users(&mut self, input_mouse_id: u32){
        self.communicator.as_mut().unwrap().inform_users(input_mouse_id);
    }
    /// Send a shutdown signal on the system bus
    pub fn send_shutdown_signal(&self) -> Result<(), DBusError>{
        self.system_conn.send(Message::signal(
            &Path::new("/org/cowsociety/vmlauncher").unwrap(), 
            &Interface::new("org.cowsociety.vmlauncher.Manager").unwrap(), 
            &Member::new("Shutdown").unwrap()
        )).map_err(|_| DBusError::FailedToSendShutdownSignal)?;
        Ok(())
    }
}
/// Function to connect to all needed dbus connections.
pub fn connect_dbus() -> Result<DBusState, DBusError>{
    // Connect to system bus
    let (resource, system_conn) = dbus_tokio::connection::new_system_sync()
        .map_err(|err| DBusError::FailedToConnectToSystemBus(err))?;
    let system_handle: Box<JoinHandle<IOResourceError>> = Box::new(tokio::spawn(async {
        resource.await
    }));
    Ok(DBusState { system_conn, system_handle, communicator: None})
}
/// Struct used to communicate between the dbus server and the main program
#[derive(Debug, Default, Clone)]
pub struct Communicator{
    user_count: Arc<AtomicU32>,
    user_ready_waker: Arc<AtomicWaker>,
    no_user_waker: Arc<AtomicWaker>,
    vm_launched: Arc<AtomicBool>,
    mouse_input_id: Arc<AtomicU32>,
    vm_launched_wakers: Arc<Mutex<Vec<Waker>>>
}
impl Communicator{
    /// Inform the communicator that a user is ready, and call waker if needed
    pub fn user_ready(&mut self) {
        let cur_count = self.user_count.load(Ordering::Relaxed) + 1;
        self.user_count.store(cur_count, Ordering::Relaxed);
        if let Some(waker) = self.user_ready_waker.take() {waker.wake();}
    }
    /// Inform communicator that a user has closed the vm viewer, when the user count goes to 0, the vm should be shutdown
    pub fn user_closed(&mut self) {
        let cur_count = self.user_count.load(Ordering::Relaxed).max(1) - 1;
        self.user_count.store(cur_count, Ordering::Relaxed);
        if cur_count == 0 {if let Some(waker) = self.no_user_waker.take() { waker.wake(); }}
    }
    /// Updates communicator state for when the vm has been launched
    pub fn inform_users(&mut self, input_mouse_id: u32) {
        self.mouse_input_id.store(input_mouse_id, Ordering::Relaxed);
        self.vm_launched.store(true, Ordering::Relaxed);
        self.vm_launched_wakers.lock().unwrap().drain(..).for_each(|waker| waker.wake());
    }
}
pub struct VmLaunchFuture(pub Communicator);
impl Future for VmLaunchFuture{
    type Output = u32;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        if self.0.vm_launched.load(Ordering::Relaxed) {return Poll::Ready(self.0.mouse_input_id.load(Ordering::Relaxed));}
        self.0.vm_launched_wakers.lock().unwrap().push(cx.waker().to_owned());
        Poll::Pending
    }
}
pub struct UserLoginFuture(pub Communicator);
impl Future for UserLoginFuture{
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if self.0.user_count.load(Ordering::Relaxed) > 0 {return Poll::Ready(());}
        self.0.user_ready_waker.register(cx.waker());
        Poll::Pending
    }
}
pub struct NoUserFuture(pub Communicator);
impl Future for NoUserFuture{
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if self.0.user_count.load(Ordering::Relaxed) == 0 {return Poll::Ready(());}
        self.0.no_user_waker.register(cx.waker());
        Poll::Pending
    }
}