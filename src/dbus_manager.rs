// This module is responsible for creating and managing dbus server connections
// It also tracks sessions and opens user connections as needed
use std::{collections::{HashMap, HashSet}, fs::File, process::Stdio, sync::{Arc, Mutex}, task::{Poll, Waker}, time::Duration};
use dbus::{
    arg::{AppendAll, ReadAll}, channel::Channel, message::MatchRule, nonblock::{stdintf::org_freedesktop_dbus::Properties, MsgMatch, Proxy, SyncConnection}, strings::{BusName, Interface, Member}, Path
};
use dbus_tokio::connection::IOResourceError;
use futures::Future;
use tokio::{process::Child, task::JoinHandle};

use crate::LaunchConfig;

/// represents the different ways a system bus method call can fail
#[derive(Debug)]
pub enum DBusError{
    FailedToConnectToSystemBus(dbus::Error),
    SystemBusLost(IOResourceError),
    UserBusLost(u32, IOResourceError),
    MethodCallError(dbus::Error),
    UserMethodCallError(u32, dbus::Error),
    PropertyQueryError(dbus::Error),
    UserPropertyQueryError(u32, dbus::Error),
    FailedToSetEUID(u32, std::io::Error),
    FailedToOpenChannel(String, dbus::Error),
    FailedToRegisterChannel(String, dbus::Error),
    FailedToConnectToUserBus(u32, dbus::Error),
    FailedToGetSessions(dbus::Error),
    FailedToAqcuireConnectionsLock(String),
    FailedToGetSessionClass(String, dbus::Error),
    FailedToGetSessionDisplay(String, dbus::Error),
    FailedToGetSessionUser(String, dbus::Error),
    FailedToGetUserRuntimePath(String, dbus::Error),
    FailedToGetSystemdEnvironment(u32, dbus::Error),
    FailedToAddSignalMatchToSystemBus(dbus::Error)
}
impl ToString for DBusError{
    fn to_string(&self) -> String {
        match self{
            DBusError::FailedToConnectToSystemBus(err) => format!("Failed to connect to system bus: {}", *err),
            DBusError::SystemBusLost(err) => format!("Connection to System Bus was lost: {}", *err),
            DBusError::UserBusLost(uid, err) => format!("Connection to the user {}'s, bus was lost: {}", *uid, *err),
            DBusError::MethodCallError(err) => format!("Could not call dbus method: {}", *err),
            DBusError::UserMethodCallError(uid, err) => format!("Could not call user dbus method for user {}, with error: {}", *uid, *err),
            DBusError::PropertyQueryError(err) => format!("Failed to Query for property: {}", *err),
            DBusError::UserPropertyQueryError(uid, err) => format!("Failed to Query for user bus property for user {}, with error: {}", *uid, *err),
            DBusError::FailedToSetEUID(uid, err) => format!("Could not set the effective uid {}, err: {}", *uid, *err),
            DBusError::FailedToOpenChannel(addr, err) => format!("Could not open the channel with address {}, with error: {}", *addr, *err),
            DBusError::FailedToRegisterChannel(addr, err) => format!("Could not register channel with address {}, with error: {}", *addr, *err),
            DBusError::FailedToConnectToUserBus(uid, err) => format!("Connecting to user {}'s bus failed with error: {}", *uid, *err),
            DBusError::FailedToGetSessions(err) => format!("Call to login1 of ListSessions failed: {}", *err),
            DBusError::FailedToAqcuireConnectionsLock(err) => format!("Session handler failed to lock the dbus connections mutex: {}", *err),
            DBusError::FailedToGetSessionClass(path, err) => format!("Could not get class from session: {}, with err: {}", *path, *err),
            DBusError::FailedToGetSessionDisplay(path, err) => format!("Could not get display from session: {}, with err: {}", *path, *err),
            DBusError::FailedToGetSessionUser(path, err) => format!("Could not get user path from session: {}, with err: {}", *path, *err),
            DBusError::FailedToGetUserRuntimePath(user, err) => format!("Could not get the runtime path from login1 for user: {}, with error: {}", *user, *err),
            DBusError::FailedToGetSystemdEnvironment(uid, err) => format!("Could not get environment property from user {}'s, systemd dbus service with err: {}", *uid, *err),
            DBusError::FailedToAddSignalMatchToSystemBus(err) => format!("Failed to add signal match to the system bus: {}", *err)
        }
    }
}

/// Represents a connection to a dbus server
#[derive(Clone)]
pub struct Connection{
    pub conn: Arc<SyncConnection>
}
impl Connection{
    pub fn new_system() -> Result<Self, DBusError>{
        let (r, conn) = dbus_tokio::connection::new_system_sync()
            .map_err(|err| DBusError::FailedToConnectToSystemBus(err))?;
        tokio::spawn(r);
        Ok(Self{conn})
    }
    pub async fn new_channel<A: AsRef<str>>(uid: u32, addr: A) -> Result<Self, DBusError> {
        users::switch::set_effective_uid(uid).map_err(|err| DBusError::FailedToSetEUID(uid, err))?;
        let channel = match Channel::open_private(addr.as_ref())
            .map(|mut channel| {if let Err(err) = channel.register() {Err(err)} else {Ok(channel)}})
        {
            Ok(Ok(channel)) => Ok(channel),
            Ok(Err(err)) => {
                let _ = users::switch::set_effective_uid(0);
                Err(DBusError::FailedToRegisterChannel(addr.as_ref().to_string(), err))
            },
            Err(err) => {
                let _ = users::switch::set_effective_uid(0);
                Err(DBusError::FailedToOpenChannel(addr.as_ref().to_string(), err))
            }
        }?;
        users::switch::set_effective_uid(0).map_err(|err| DBusError::FailedToSetEUID(0, err))?;
        let (r, conn) = dbus_tokio::connection::from_channel(channel)
            .map_err(|err| DBusError::FailedToConnectToUserBus(uid, err))?;
        tokio::spawn(r);
        Ok(Self{conn})
    }
    pub async fn method_call<'a, D, P, I, M, A, R>(&self, dest: D, path: P, interface: I, method: M, args: A) -> Result<R, dbus::Error> 
    where 
        D: Into<BusName<'a>>,
        P: Into<Path<'a>>,
        I: Into<Interface<'a>>,
        M: Into<Member<'a>>,
        A: AppendAll,
        R: ReadAll + 'static
    {
        let proxy = dbus::nonblock::Proxy::new(
            dest, 
            path, 
            std::time::Duration::from_secs(2), 
            self.conn.clone()
        );
        proxy.method_call(interface, method, args).await
    }
    pub async fn property_get<'a, D, P, I, N, R>(&self, dest: D, path: P, interface: I, property: N) -> Result<R, dbus::Error> 
    where 
        D: Into<BusName<'a>>,
        P: Into<Path<'a>>,
        I: AsRef<str>,
        N: AsRef<str>,
        R: for<'b> dbus::arg::Get<'b> + 'static
    {
        let proxy = dbus::nonblock::Proxy::new(
            dest, 
            path, 
            std::time::Duration::from_secs(2), 
            self.conn.clone()
        );
        proxy.get::<R>(interface.as_ref(), property.as_ref()).await
    }
}

/// Data representing a session
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct Session{
    pub path: String,
    pub display: String,
    pub xauthority_path: String,
    pub uid: u32
}

/// Struct to hold all connection resources.
/// System connection dropping is an immediate failure. User connections dropping can be ignored
pub struct DBusConnections{
    pub system: Connection,
    /// Stores uid -> (Vec<Session Path Strings>, DBus Connection)
    pub users: HashMap<u32, (HashSet<String>, Connection)>,
    /// Stores Session Path Strings of sessions with a display
    pub displays: HashSet<String>,
    /// Stores Session Path String -> Session,
    pub sessions: HashMap<String, Session>,
    /// List of wakers to call when the number of displays changes
    pub display_change_wakers: Vec<Waker>,
    signal_handles: Option<(MsgMatch, MsgMatch)>
}
impl DBusConnections{
    /// Create new struct with a connection to the system bus
    pub fn new() -> Result<Self, DBusError> {
        Ok(Self{system: Connection::new_system()?, users: HashMap::new(), displays: HashSet::new(), sessions: HashMap::new(), display_change_wakers: vec![], signal_handles: None})
    }
    /// Async fn which continuosly handles new sessions forever
    pub async fn create_session_handler(data: Arc<Mutex<Self>>) -> Result<JoinHandle<()>, DBusError> {
        // sessions currently handled by the data struct
        let mut known_sessions: HashSet<(String, u32)> = HashSet::new();
        let mut invalid_sessions: HashSet<(String, u32)> = HashSet::new();
        let mut valid_sessions: HashSet<(String, u32)> = HashSet::new();

        let system_connection = data.lock().unwrap().system.conn.clone();
        let proxy = dbus::nonblock::Proxy::new(
            "org.freedesktop.login1", 
            "/org/freedesktop/login1", 
            Duration::from_secs(2), 
            system_connection.clone()
        );

        let mr = MatchRule::new_signal("org.freedesktop.login1.Manager", "SessionNew");
        let mr2 = MatchRule::new_signal("org.freedesktop.login1.Manager", "SessionRemoved");
        let waker: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));
        let waker_copy = waker.clone();
        let incoming_signal = system_connection.add_match(mr).await
            .map_err(|err| DBusError::FailedToAddSignalMatchToSystemBus(err))?
            .cb(move |_, (_, _): (String, dbus::Path)| {
                if let Ok(Some(waker)) = waker_copy.lock().map(|mut guard| guard.take()) {waker.wake();}
                true
            });
        let waker_copy = waker.clone();
        let incoming_signal2 = system_connection.add_match(mr2).await
            .map_err(|err| DBusError::FailedToAddSignalMatchToSystemBus(err))?
            .cb(move |_, (_, _): (String, dbus::Path)| {
                if let Ok(Some(waker)) = waker_copy.lock().map(|mut guard| guard.take()) {waker.wake();}
                true
            });
        data.lock().unwrap().signal_handles = Some((incoming_signal, incoming_signal2));
        let mut new_sessions: HashSet<(String, u32)> = HashSet::new();
        let mut new_session_info: HashMap<String, (String, dbus::Path)> = HashMap::new();
        let handle = tokio::task::spawn(async move {loop{
            // wait either 5 seconds, or until a signal is sent on login
            println!("Waiting for new sessions, invalid: {:?}, valid: {:?}", invalid_sessions, valid_sessions);
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(5)) => {println!("Waited, 5 seconds, getting sessions")},
                _ = NewSignalFuture::new(waker.clone()) => {println!("recieved signal, getting sessions")}
            }
            // get the current sessions from login1 as HashSet(object-path, uid)
            let current_sessions = HashSet::from_iter(proxy.method_call::<(Vec<(String, u32, String, String, dbus::Path)>,), _, _, _>(
                "org.freedesktop.login1.Manager", 
                "ListSessions", 
                ()
            ).await.map_err(|err| DBusError::FailedToGetSessions(err)).unwrap().0.into_iter().map(|(_, uid, _, _, path)| (path.to_string(), uid.to_owned())));
            println!("Current Sessions: {:?}", current_sessions);
            // if state hasnt changed, continue
            if current_sessions == known_sessions {continue;}
            println!("Found session discrepency");
            // go through the new sessions, and find out whether they are valid or not
            new_sessions.clear(); new_session_info.clear();
            for (path, uid) in current_sessions.difference(&known_sessions.clone()){
                println!("Looking at session {:?}", path);
                // determine if session is user session
                let temp_proxy = dbus::nonblock::Proxy::new(
                    "org.freedesktop.login1", 
                    path.clone(), 
                    Duration::from_secs(2), 
                    system_connection.clone()
                );
                let class = temp_proxy.get::<String>("org.freedesktop.login1.Session", "Class").await
                    .map_err(|err| DBusError::FailedToGetSessionClass(path.to_string(), err)).unwrap();
                if class == "user" {
                    new_sessions.insert((path.to_string(), uid.to_owned()));
                    let display = temp_proxy.get::<String>("org.freedesktop.login1.Session", "Display").await
                        .map_err(|err| DBusError::FailedToGetSessionDisplay(path.to_string(), err)).unwrap();
                    let (_, user_path) = temp_proxy.get::<(u32, dbus::Path)>("org.freedesktop.login1.Session", "User").await
                        .map_err(|err| DBusError::FailedToGetSessionUser(path.to_string(), err)).unwrap();
                    new_session_info.insert(path.to_string(), (display, user_path));
                    valid_sessions.insert((path.to_string(), uid.to_owned()));
                }else {
                    invalid_sessions.insert((path.to_string(), uid.to_owned()));
                }
                known_sessions.insert((path.to_string(), uid.to_owned()));
            }
            let old_sessions = valid_sessions.difference(&current_sessions).cloned().collect::<HashSet<(String, u32)>>();
            println!("Found old sessions: {:?}, and new sessions: {:?} info: {:?}", old_sessions, new_sessions, new_session_info);
            // if there is no work to be done on data, continue
            if new_sessions.len() == 0 && old_sessions.len() == 0 {continue;}
            
            let mut wake = false;
            // create new sessions
            for (path, uid) in new_sessions.iter(){
                // grab display from login1
                let (display, user_path) = new_session_info.get(path).unwrap().to_owned();
                // connect user bus if needed
                if !data.lock().unwrap().users.contains_key(&uid) {
                    let temp_proxy = dbus::nonblock::Proxy::new(
                        "org.freedesktop.login1", 
                        user_path, 
                        Duration::from_secs(2), 
                        system_connection.clone()
                    );
                    let runtime_path = temp_proxy.get::<String>("org.freedesktop.login1.User", "RuntimePath").await
                        .map_err(|err| DBusError::FailedToGetUserRuntimePath(path.to_string(), err)).unwrap();
                    let addr = "unix:path=".to_string() + &runtime_path + "/bus";
                    let connection = Connection::new_channel(uid.clone(), addr).await.unwrap();
                    data.lock().unwrap().users.insert(uid.clone(), (HashSet::from_iter([path.clone()]), connection));
                }
                // get xauthority if display exists
                let mut xauthority_path = "".to_string();
                if display != "" {
                    let connection = data.lock().unwrap().users.get_mut(&uid).unwrap().1.clone();
                    let env: Vec<String> = connection.property_get(
                        "org.freedesktop.systemd1", 
                        "/org/freedesktop/systemd1", 
                        "org.freedesktop.systemd1.Manager", 
                        "Environment"
                    ).await.map_err(|err| DBusError::FailedToGetSystemdEnvironment(uid.to_owned(), err)).unwrap();
                    if let Some(var) = env.iter().find(|var| var.starts_with("XAUTHORITY=")) {
                        xauthority_path = var.strip_prefix("XAUTHORITY=").unwrap().to_string();
                    }
                    data.lock().unwrap().displays.insert(path.to_owned());
                    wake = true;
                }
                // place everything in the object
                let session = Session{path: path.to_owned(), display, xauthority_path, uid: uid.clone()};
                println!("Found new session at path {:?}, : {:?}", path, session);
                data.lock().unwrap().sessions.insert(path.to_owned(), session);
            }
            // kill old sessions
            for (path, uid) in old_sessions.into_iter() {
                data.lock().unwrap().sessions.remove(&path);
                if data.lock().unwrap().displays.remove(&path) {
                    wake = true;
                }
                let mut delete_user = false;
                if let Some(user) = data.lock().unwrap().users.get_mut(&uid) {
                    user.0.remove(&path);
                    if user.0.is_empty() {
                        delete_user = true;
                    }
                }
                if delete_user {data.lock().unwrap().users.remove(&uid);}
                let session = (path, uid);
                invalid_sessions.remove(&session);
                valid_sessions.remove(&session);
                known_sessions.remove(&session);
            }
            // wake up display change futures
            if wake {data.lock().unwrap().display_change_wakers.drain(..).for_each(|waker| waker.wake());}
        }});
        return Ok(handle);
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
        self.system.method_call(dest, path, interface, method, args).await.map_err(|err| DBusError::MethodCallError(err))
    }
    /// Get a property on the system bus
    pub async fn get_system_property<'a, D, P, I, N, R>(&mut self, dest: D, path: P, interface: I, property: N) -> Result<R, DBusError> 
    where 
        D: Into<BusName<'a>>,
        P: Into<Path<'a>>,
        I: AsRef<str>,
        N: AsRef<str>,
        R: for<'b> dbus::arg::Get<'b> + 'static
    {
        self.system.property_get(dest, path, interface, property).await.map_err(|err| DBusError::PropertyQueryError(err))
    }
    /// Call a method on the user busses
    pub async fn call_user_method<'a, D, P, I, M, A, R>(&mut self, dest: D, path: P, interface: I, method: M, args: A) -> Result<HashMap<u32, R>, DBusError> 
    where 
        D: Into<BusName<'a>> + Clone,
        P: Into<Path<'a>> + Clone,
        I: Into<Interface<'a>> + Clone,
        M: Into<Member<'a>> + Clone,
        A: AppendAll + Clone,
        R: ReadAll + 'static
    {
        let mut results = HashMap::new();
        for (uid, (_, connection)) in self.users.iter() {
            let result = connection.method_call(dest.clone(), path.clone(), interface.clone(), method.clone(), args.clone()).await
                .map_err(|err| DBusError::UserMethodCallError(*uid, err))?;
            results.insert(*uid, result);
        }
        Ok(results)
    }
    /// Get a property on the user busses
    pub async fn get_user_property<'a, D, P, I, N, R>(&mut self, dest: D, path: P, interface: I, property: N) -> Result<HashMap<u32, R>, DBusError> 
    where 
        D: Into<BusName<'a>> + Clone,
        P: Into<Path<'a>> + Clone,
        I: AsRef<str>,
        N: AsRef<str>,
        R: for<'b> dbus::arg::Get<'b> + 'static
    {
        let mut results = HashMap::new();
        for (uid, (_, connection)) in self.users.iter(){
            let result = connection.property_get(dest.clone(), path.clone(), interface.as_ref(), property.as_ref()).await
                .map_err(|err| DBusError::UserPropertyQueryError(*uid, err))?;
            results.insert(*uid, result);
        }
        Ok(results)
    }
}

pub trait DBusManager{
    /// Call a method on the system bus
    #[allow(async_fn_in_trait)]
    async fn call_system_method<'a, D, P, I, M, A, R>(&self, dest: D, path: P, interface: I, method: M, args: A) -> Result<R, DBusError> 
    where 
        D: Into<BusName<'a>>,
        P: Into<Path<'a>>,
        I: Into<Interface<'a>>,
        M: Into<Member<'a>>,
        A: AppendAll,
        R: ReadAll + 'static;
    /// Get a property on the system bus
    #[allow(async_fn_in_trait)]
    async fn get_system_property<'a, D, P, I, N, R>(&self, dest: D, path: P, interface: I, property: N) -> Result<R, DBusError> 
    where 
        D: Into<BusName<'a>>,
        P: Into<Path<'a>>,
        I: AsRef<str>,
        N: AsRef<str>,
        R: for<'b> dbus::arg::Get<'b> + 'static;
    /// Call a method on the user busses
    #[allow(async_fn_in_trait)]
    async fn call_user_method<'a, D, P, I, M, A, R>(&self, dest: D, path: P, interface: I, method: M, args: A) -> Result<HashMap<u32, R>, DBusError> 
    where 
        D: Into<BusName<'a>> + Clone,
        P: Into<Path<'a>> + Clone,
        I: Into<Interface<'a>> + Clone,
        M: Into<Member<'a>> + Clone,
        A: AppendAll + Clone,
        R: ReadAll + 'static;
    /// Get a property on the user busses
    #[allow(async_fn_in_trait)]
    async fn get_user_property<'a, D, P, I, N, R>(&self, dest: D, path: P, interface: I, property: N) -> Result<HashMap<u32, R>, DBusError> 
    where 
        D: Into<BusName<'a>> + Clone,
        P: Into<Path<'a>> + Clone,
        I: AsRef<str>,
        N: AsRef<str>,
        R: for<'b> dbus::arg::Get<'b> + 'static; 
}
impl DBusManager for Arc<Mutex<DBusConnections>> {
    /// Call a method on the system bus
    async fn call_system_method<'a, D, P, I, M, A, R>(&self, dest: D, path: P, interface: I, method: M, args: A) -> Result<R, DBusError> 
    where 
        D: Into<BusName<'a>>,
        P: Into<Path<'a>>,
        I: Into<Interface<'a>>,
        M: Into<Member<'a>>,
        A: AppendAll,
        R: ReadAll + 'static
    {
        let system = self.lock().unwrap().system.clone();
        system.method_call(dest, path, interface, method, args).await.map_err(|err| DBusError::MethodCallError(err))
    }
    /// Get a property on the system bus
    async fn get_system_property<'a, D, P, I, N, R>(&self, dest: D, path: P, interface: I, property: N) -> Result<R, DBusError> 
    where 
        D: Into<BusName<'a>>,
        P: Into<Path<'a>>,
        I: AsRef<str>,
        N: AsRef<str>,
        R: for<'b> dbus::arg::Get<'b> + 'static
    {
        let system = self.lock().unwrap().system.clone();
        system.property_get(dest, path, interface, property).await.map_err(|err| DBusError::PropertyQueryError(err))
    }
    /// Call a method on the user busses
    async fn call_user_method<'a, D, P, I, M, A, R>(&self, dest: D, path: P, interface: I, method: M, args: A) -> Result<HashMap<u32, R>, DBusError> 
    where 
        D: Into<BusName<'a>> + Clone,
        P: Into<Path<'a>> + Clone,
        I: Into<Interface<'a>> + Clone,
        M: Into<Member<'a>> + Clone,
        A: AppendAll + Clone,
        R: ReadAll + 'static
    {
        let users = self.lock().unwrap().users.clone();
        let mut results = HashMap::new();
        for (uid, (_, connection)) in users.into_iter() {
            let result = connection.method_call(dest.clone(), path.clone(), interface.clone(), method.clone(), args.clone()).await
                .map_err(|err| DBusError::UserMethodCallError(uid, err))?;
            results.insert(uid, result);
        }
        Ok(results)
    }
    /// Get a property on the user busses
    async fn get_user_property<'a, D, P, I, N, R>(&self, dest: D, path: P, interface: I, property: N) -> Result<HashMap<u32, R>, DBusError> 
    where 
        D: Into<BusName<'a>> + Clone,
        P: Into<Path<'a>> + Clone,
        I: AsRef<str>,
        N: AsRef<str>,
        R: for<'b> dbus::arg::Get<'b> + 'static
    {
        let users = self.lock().unwrap().users.clone();
        let mut results = HashMap::new();
        for (uid, (_, connection)) in users.into_iter(){
            let result = connection.property_get(dest.clone(), path.clone(), interface.as_ref(), property.as_ref()).await
                .map_err(|err| DBusError::UserPropertyQueryError(uid, err))?;
            results.insert(uid, result);
        }
        Ok(results)
    }
}

/// future which waits for any signal to happen
pub struct NewSignalFuture{
    waker: Arc<Mutex<Option<Waker>>>,
    polled_once: bool
}
impl NewSignalFuture{
    pub fn new(waker: Arc<Mutex<Option<Waker>>>) -> Self{
        Self{waker, polled_once: false}
    }
}
impl Future for NewSignalFuture{
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        if self.polled_once {Poll::Ready(())} else {
            let future = self.get_mut();
            future.polled_once = true;
            let _ = future.waker.lock().unwrap().insert(cx.waker().to_owned());
            Poll::Pending
        }
    }
}

pub struct AnyDisplayFuture{
    dbus: Arc<Mutex<DBusConnections>>
}
impl AnyDisplayFuture{
    pub fn new(dbus: Arc<Mutex<DBusConnections>>) -> Self{Self{dbus}}
}
impl Future for AnyDisplayFuture{
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let mut dbus = self.dbus.lock().unwrap(); 
        if dbus.displays.is_empty() {
            dbus.display_change_wakers.push(cx.waker().clone());
            Poll::Pending
        }else {Poll::Ready(())}
    }
}

/// future which waits for any new displays to be recognized
pub struct DisplaySessionChangeFuture{
    pub dbus: Arc<Mutex<DBusConnections>>,
    pub known_displays: HashSet<String>
}
impl Future for DisplaySessionChangeFuture{
    type Output = HashSet<Session>;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let future = self.get_mut();
        let mut dbus = future.dbus.lock().unwrap();
        if future.known_displays == dbus.displays {
            dbus.display_change_wakers.push(cx.waker().to_owned());
            return Poll::Pending;
        }else {
            let sessions = dbus.sessions.values().filter(|sess| dbus.displays.contains(&sess.path)).cloned().collect::<HashSet<Session>>();
            return Poll::Ready(sessions);
        }
    }
}

pub fn setup_viewer_session_handler(dbus: Arc<Mutex<DBusConnections>>, vm_type: LaunchConfig) -> Result<JoinHandle<()>, DBusError> {
    let mut known_sessions: HashSet<Session> = HashSet::new();
    let mut children: HashMap<String, Result<Child, std::io::Error>> = HashMap::new();
    Ok(tokio::task::spawn(async move {loop{
        let known_displays = known_sessions.iter().map(|sess| sess.path.clone()).collect::<HashSet<String>>();
        let sessions = DisplaySessionChangeFuture{dbus: dbus.clone(), known_displays: known_displays}.await;
        let new_sessions = sessions.difference(&known_sessions).collect::<Vec<&Session>>();
        let old_sessions = known_sessions.difference(&sessions).collect::<Vec<&Session>>();
        for display in old_sessions{
            if let Some(Ok(mut child)) = children.remove(&display.path) {
                let _ = child.start_kill();
            }
        }
        for session in new_sessions{
            println!("Launching viewer for session: {:?}", session);
            let mut xauth = session.xauthority_path.clone();
            let user = dbus.lock().unwrap().users.get(&session.uid).cloned();
            if let Some(user) = user{
                let proxy = Proxy::new("org.freedesktop.systemd1", "/org/freedesktop/systemd1", Duration::from_secs(2), user.1.conn.clone());
                let env = proxy.get::<Vec<String>>("org.freedesktop.systemd1.Manager", "Environment").await.unwrap();
                for var in env {
                    if var.starts_with("XAUTHORITY=") {xauth = var.strip_prefix("XAUTHORITY=").unwrap().to_string();}
                }
                println!("Refound xauth: {}", xauth);
            }
            let _ = users::switch::set_effective_uid(session.uid.to_owned());
            let child = match vm_type {
                LaunchConfig::LG => {
                    let (log, err_log): (Stdio, Stdio) = if let Ok(file) = File::create("/tmp/user_".to_string() + session.uid.to_string().as_str() + "_lg_log.txt") {
                        (file.try_clone().unwrap().into(), file.into())
                    } else {(Stdio::null(), Stdio::null())};
                    tokio::process::Command::new("looking-glass-client")
                        .args(["-T", "-s", "input:captureOnFocus"])
                        .envs([("DISPLAY", session.display.to_owned()), ("XAUTHORITY", xauth)])
                        .uid(session.uid.to_owned())
                        .stdout(log).stderr(err_log).spawn()
                },
                LaunchConfig::Spice => {
                    let (log, err_log): (Stdio, Stdio) = if let Ok(file) = File::create("/tmp/user_".to_string() + session.uid.to_string().as_str() + "_virtviewer_log.txt") {
                        (file.try_clone().unwrap().into(), file.into())
                    } else {(Stdio::null(), Stdio::null())};
                    tokio::process::Command::new("virt-viewer")
                        .args(["--connect", "qemu:///system", "windows"])
                        .envs([("DISPLAY", session.display.to_owned()), ("XAUTHORITY", xauth)])
                        .uid(session.uid.to_owned())
                        .stdout(log).stderr(err_log).spawn()
                },
                _ => panic!("How did we get here?")
            };
            children.insert(session.path.to_owned(), child);
        }
        known_sessions = sessions;
    }}))
}
