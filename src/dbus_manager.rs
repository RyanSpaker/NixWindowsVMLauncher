// This module is responsible for creating and managing dbus server connections
use std::{collections::HashMap, sync::Arc};
use dbus::{
    arg::{AppendAll, ReadAll}, 
    channel::Channel, 
    nonblock::{stdintf::org_freedesktop_dbus::Properties, SyncConnection}, 
    strings::{BusName, Interface, Member}, 
    Path
};
use dbus_tokio::connection::IOResourceError;
use tokio::task::JoinHandle;

/// represents the different ways a system bus method call can fail
#[derive(Debug)]
pub enum DBusError{
    SystemBusLost(IOResourceError),
    MethodCallError(dbus::Error),
    UserMethodCallError(u32, dbus::Error),
    PropertyQueryError(dbus::Error),
    UserPropertyQueryError(u32, dbus::Error),
    FailedToConnectToSystemBus(dbus::Error),
    FailedToGetUserRuntimePath(String, dbus::Error),
    FailedToSetEUID(u32, std::io::Error),
    FailedToOpenChannel(String, dbus::Error),
    FailedToRegisterChannel(String, dbus::Error),
    FailedToConnectToUserBus(String, dbus::Error)
}
impl ToString for DBusError{
    fn to_string(&self) -> String {
        match self{
            DBusError::SystemBusLost(err) => format!("Connection to System Bus was lost: {}", *err),
            DBusError::MethodCallError(err) => format!("Could not call dbus method: {}", *err),
            DBusError::UserMethodCallError(uid, err) => format!("Could not call user dbus method for user {}, with error: {}", *uid, *err),
            DBusError::PropertyQueryError(err) => format!("Failed to Query for property: {}", *err),
            DBusError::UserPropertyQueryError(uid, err) => format!("Failed to Query for user bus property for user {}, with error: {}", *uid, *err),
            DBusError::FailedToConnectToSystemBus(err) => format!("Failed to connect to system bus: {}", *err),
            DBusError::FailedToGetUserRuntimePath(user, err) => format!("Could not get the runtime path from login1 for user: {}, with error: {}", *user, *err),
            DBusError::FailedToSetEUID(uid, err) => format!("Could not set the effective uid {}, err: {}", *uid, *err),
            DBusError::FailedToOpenChannel(addr, err) => format!("Could not open the channel with address {}, with error: {}", *addr, *err),
            DBusError::FailedToRegisterChannel(addr, err) => format!("Could not register channel with address {}, with error: {}", *addr, *err),
            DBusError::FailedToConnectToUserBus(name, err) => format!("Connecting to {}'s bus failed with error: {}", *name, *err)
        }
    }
}

/// Struct to hold all connection resources.
/// System connection dropping is an immediate failure. User connections dropping can be ignored.
pub struct DBusConnection{
    pub system_conn: Arc<SyncConnection>,
    pub system_handle: Box<JoinHandle<IOResourceError>>,
    pub users: HashMap<u32, (Arc<SyncConnection>, JoinHandle<IOResourceError>)>
}
impl DBusConnection{
    /// Connect to the system bus and create a new resource to hold the connection
    pub fn new() -> Result<Self, DBusError> {
        let (resource, system_conn) = dbus_tokio::connection::new_system_sync()
            .map_err(|err| DBusError::FailedToConnectToSystemBus(err))?;
        let system_handle = Box::new(tokio::spawn(resource));
        Ok(Self{ system_conn, system_handle, users: HashMap::new()})
    }
    /// Call a method on the system bus
    pub async fn call_system_method<'a, D, P, I, M, A, R>(&mut self, dest: D, path: P, interface: I, method: M, args: A) -> Result<R, DBusError> 
    where 
        D: Into<BusName<'a>>,
        P: Into<Path<'a>>,
        I: Into<Interface<'a>>,
        M: Into<Member<'a>>,
        A: AppendAll,
        R: ReadAll + 'static
    {
        self.check_system_bus().await?;
        let proxy = dbus::nonblock::Proxy::new(
            dest, 
            path, 
            std::time::Duration::from_secs(2), 
            self.system_conn.clone()
        );
        proxy.method_call(interface, method, args).await.map_err(|err| DBusError::MethodCallError(err))
    }
    /// Get a property on the system bus
    pub async fn get_system_property<'a, D, P, I, N, R>(&mut self, dest: D, path: P, interface: I, property: N) -> Result<R, DBusError> 
    where 
        D: Into<BusName<'a>>,
        P: Into<Path<'a>>,
        I: AsRef<str>,
        N: AsRef<str>,
        R: ReadAll + 'static + for<'b> dbus::arg::Get<'b> 
    {
        self.check_system_bus().await?;
        let proxy = dbus::nonblock::Proxy::new(
            dest, 
            path, 
            std::time::Duration::from_secs(2), 
            self.system_conn.clone()
        );
        proxy.get(interface.as_ref(), property.as_ref()).await
            .map_err(|err| DBusError::PropertyQueryError(err))
    }
    /// Check status of system bus
    pub async fn check_system_bus(&mut self) -> Result<(), DBusError>{
        if self.system_handle.is_finished() {
            Err(DBusError::SystemBusLost(self.system_handle.as_mut().await.unwrap()))
        }else {Ok(())}
    }
    /// Create connections to all known users
    pub async fn connect_users(&mut self) -> Result<(), DBusError>{
        self.check_system_bus().await?;
        // call list users on login1 to find all currently active users
        let users: (Vec<(u32, String, Path)>,) = self.call_system_method(
            "org.freedesktop.login1", 
            "/org/freedesktop/login1", 
            "org.freedesktop.login1.Manager", 
            "ListUsers", 
            ()
        ).await?;
        self.check_users().await;
        for (uid, name, path) in users.0.into_iter() {
            // skip if we have a valid dbus connection already
            if self.users.contains_key(&uid) {continue;}
            let (runtime_path,): (String,) = self.get_system_property("org.freedesktop.login1", path.clone(), "org.freedesktop.login1.User", "RuntimePath").await
                .map_err(|err| match err {
                    DBusError::SystemBusLost(e) => DBusError::SystemBusLost(e),
                    DBusError::PropertyQueryError(e) => DBusError::FailedToGetUserRuntimePath(name.to_owned(), e),
                    _ => panic!("How did we get here?")
                })?;
            //create channel and connect
            let addr = "unix:path=".to_string() + runtime_path.to_string().as_str() + "/bus";
            let channel = Self::open_channel(uid, &addr)?;
            let (resource, conn) = dbus_tokio::connection::from_channel::<dbus::nonblock::SyncConnection>(channel)
                .map_err(|err| DBusError::FailedToConnectToUserBus(name, err))?;
            let handle = tokio::spawn(resource);
            self.users.insert(uid, (conn, handle));
        }
        Ok(())
    }
    /// Call a method on the user busses
    pub async fn call_user_method<'a, D, P, I, M, A, R>(&mut self, dest: D, path: P, interface: I, method: M, args: A) -> Result<HashMap<u32, R>, DBusError> 
    where 
        D: Into<BusName<'a>>,
        P: Into<Path<'a>>,
        I: Into<Interface<'a>>,
        M: Into<Member<'a>>,
        A: AppendAll + Clone,
        R: ReadAll + 'static
    {
        self.check_users().await;
        let mut results = HashMap::new();
        let dest: BusName = dest.into(); let path: Path = path.into(); let interface: Interface = interface.into();
        let method: Member = method.into();
        for (uid, (conn, _)) in self.users.iter(){
            let proxy = dbus::nonblock::Proxy::new(
                dest.clone(), 
                path.clone(), 
                std::time::Duration::from_secs(2), 
                conn.clone()
            );
            results.insert(
                uid.clone(), 
                proxy.method_call(interface.clone(), method.clone(), args.clone()).await
                    .map_err(|err| DBusError::UserMethodCallError(uid.to_owned(), err))?
            );
        }
        Ok(results)
    }
    /// Get a property on the user busses
    pub async fn get_user_property<'a, D, P, I, N, R>(&mut self, dest: D, path: P, interface: I, property: N) -> Result<HashMap<u32, R>, DBusError> 
    where 
        D: Into<BusName<'a>>,
        P: Into<Path<'a>>,
        I: AsRef<str>,
        N: AsRef<str>,
        R: ReadAll + 'static + for<'b> dbus::arg::Get<'b> 
    {
        self.check_users().await;
        let mut results = HashMap::new();
        let dest: BusName = dest.into(); let path: Path = path.into();
        for (uid, (conn, _)) in self.users.iter(){
            let proxy = dbus::nonblock::Proxy::new(
                dest.clone(), 
                path.clone(), 
                std::time::Duration::from_secs(2), 
                conn.clone()
            );
            results.insert(
                uid.clone(), 
                proxy.get(interface.as_ref(), property.as_ref()).await
                    .map_err(|err| DBusError::UserPropertyQueryError(uid.to_owned(), err))?
            );
        }
        Ok(results)
    }
    /// Checks all user connections to see if they are still valid.
    pub async fn check_users(&mut self) {
        let mut errors: HashMap<u32, IOResourceError> = HashMap::new();
        for (name, (_, handle)) in self.users.iter_mut(){
            if handle.is_finished() {
                let err = handle.await.unwrap();
                errors.insert(name.to_owned(), err);
            }
        }
        errors.iter().for_each(|(name, err)| {
            self.users.remove(name);
            println!("User bus: {}, disconnected with error: {}", name, err);
        });
    }
    /// Opens a channel at address with uid
    pub fn open_channel(uid: u32, address: &str) -> Result<Channel, DBusError>{
        users::switch::set_effective_uid(uid).map_err(|err| DBusError::FailedToSetEUID(uid, err))?;
        let channel = match Channel::open_private(address)
            .map(|mut channel| {if let Err(err) = channel.register() {Err(err)} else {Ok(channel)}})
        {
            Ok(Ok(channel)) => Ok(channel),
            Ok(Err(err)) => {
                let _ = users::switch::set_effective_uid(0);
                Err(DBusError::FailedToRegisterChannel(address.to_string(), err))
            },
            Err(err) => {
                let _ = users::switch::set_effective_uid(0);
                Err(DBusError::FailedToOpenChannel(address.to_string(), err))
            }
        }?;
        users::switch::set_effective_uid(0).map_err(|err| DBusError::FailedToSetEUID(0, err))?;
        Ok(channel)
    }
}