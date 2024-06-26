use std::{fs::{File, OpenOptions}, num::ParseIntError, os::{fd::OwnedFd, unix::fs::OpenOptionsExt}, path::Path};
use evdev::{uinput::{VirtualDevice, VirtualDeviceBuilder}, AttributeSet, Device, EventStream, EventType, InputEvent, InputEventKind, Key, RelativeAxisType, Synchronization};
use input::{event::{pointer::{ButtonState, PointerScrollEvent}, PointerEvent}, Event, Libinput, LibinputInterface};
use nix::libc::{O_RDONLY, O_RDWR, O_WRONLY};
use tokio::task::JoinHandle;

/// Interface used by Libinput.
pub struct Interface;
impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        OpenOptions::new()
            .custom_flags(flags)
            .read((flags & O_RDONLY != 0) | (flags & O_RDWR != 0))
            .write((flags & O_WRONLY != 0) | (flags & O_RDWR != 0))
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(File::from(fd));
    }
}

#[derive(Debug)]
pub enum MouseError{
    FailedToAddPathToLibinput,
    FailedToStripEventFromSysname,
    FailedToParseSysnameForID(ParseIntError),
    FailedToOpenMouseDevice(std::io::Error),
    FailedToTurnDeviceIntoStream(std::io::Error),
    FailedToCreateDeviceBuilder(std::io::Error),
    FailedToAddRelativeAxes(std::io::Error),
    FailedToAddKeys(std::io::Error),
    FailedToBuildVirtualMouse(std::io::Error),
    FailedToGetOutputSyspath(std::io::Error),
    FailedToReadOutputSyspath(std::io::Error),
    FailedToFindEventFileInOutputSyspath,
    FailedToParseEventFileForOutputID(ParseIntError),
    FailedToGetEventFromTestStream(std::io::Error),
    FailedToDispatchLibinput(std::io::Error),
    FailedToEmitMouseEvents(std::io::Error)
}
impl ToString for MouseError{
    fn to_string(&self) -> String {
        match self{
            MouseError::FailedToAddPathToLibinput => format!("Could not add mouse input path to libinput context"),
            MouseError::FailedToStripEventFromSysname => format!("Stripping event prefix from input sysname failed"),
            MouseError::FailedToParseSysnameForID(err) => format!("Could not parse stripped sysname for input id: {}", *err),
            MouseError::FailedToOpenMouseDevice(err) => format!("Failed to open mouse input as a device: {}", *err),
            MouseError::FailedToTurnDeviceIntoStream(err) => format!("Failed to convert mouse input device to event stream: {}", *err),
            MouseError::FailedToCreateDeviceBuilder(err) => format!("Could not create VirtualDeviceBuilder: {}", *err),
            MouseError::FailedToAddRelativeAxes(err) => format!("Ccould not add relative axes to virtual device builder: {}", *err),
            MouseError::FailedToAddKeys(err) => format!("Could not add keys to virtual device builder: {}", *err),
            MouseError::FailedToBuildVirtualMouse(err) => format!("Could not build virtual device: {}", *err),
            MouseError::FailedToGetOutputSyspath(err) => format!("Failed to get syspath of mouse output: {}", *err),
            MouseError::FailedToReadOutputSyspath(err) => format!("Could not read syspath dir of mouse output: {}", *err),
            MouseError::FailedToFindEventFileInOutputSyspath => format!("Could not find event file/folder in syspath of mouse output"),
            MouseError::FailedToParseEventFileForOutputID(err) => format!("Could not parse event file of mouse output syspath for output id: {}", *err),
            MouseError::FailedToGetEventFromTestStream(err) => format!("Failed to get an event from the test stream: {}", *err),
            MouseError::FailedToDispatchLibinput(err) => format!("Could not dispath libinput context: {}", *err),
            MouseError::FailedToEmitMouseEvents(err) => format!("Could not emit mouse events on virtual device: {}", *err),
        }
    }
}

/// Struct containing mouse information
pub struct MouseManager{
    pub input_id: u32,
    pub output_id: u32,
    pub test_source: EventStream,
    pub data_source: Libinput,
    pub output: VirtualDevice,
    pub movement: MouseMovement
}
impl MouseManager{
    pub fn new(input_event_path: &str) -> Result<Self, MouseError>{
        // Get Libinput setup
        let mut data_source = Libinput::new_from_path(Interface);
        let device = data_source.path_add_device(&input_event_path).ok_or(MouseError::FailedToAddPathToLibinput)?;
        // Get the input event id
        let input_id = device.sysname().to_string().strip_prefix("event")
            .ok_or(MouseError::FailedToStripEventFromSysname)?.parse::<u32>()
            .map_err(|err| MouseError::FailedToParseSysnameForID(err))?;
        // Get evdev test source setup
        let test_source = Device::open(input_event_path).map_err(|err| MouseError::FailedToOpenMouseDevice(err))?
            .into_event_stream().map_err(|err| MouseError::FailedToTurnDeviceIntoStream(err))?;
        // Create the virtual mouse device
        let mut output = VirtualDeviceBuilder::new().map_err(|err| MouseError::FailedToCreateDeviceBuilder(err))?
            .name("Windows VM Mouse")
            .with_relative_axes(&AttributeSet::from_iter([
                RelativeAxisType::REL_X,
                RelativeAxisType::REL_Y,
                RelativeAxisType::REL_WHEEL,
                RelativeAxisType::REL_WHEEL_HI_RES,
                RelativeAxisType::REL_HWHEEL,
                RelativeAxisType::REL_HWHEEL_HI_RES
            ])).map_err(|err| MouseError::FailedToAddRelativeAxes(err))?
            .with_keys(&AttributeSet::from_iter([
                Key::BTN_LEFT,
                Key::BTN_RIGHT,
                Key::BTN_MIDDLE
            ])).map_err(|err| MouseError::FailedToAddKeys(err))?
            .build().map_err(|err| MouseError::FailedToBuildVirtualMouse(err))?;
        // Get the output event id
        let output_id = output.get_syspath().map_err(|err| MouseError::FailedToGetOutputSyspath(err))?
            .read_dir().map_err(|err| MouseError::FailedToReadOutputSyspath(err))?
            .flatten().filter_map(|entry| {
                match entry.file_name().into_string() {
                    Ok(name) => {
                        name.strip_prefix("event").map(|id| id.to_string())
                    },
                    Err(_) => {None}
                }
            }).next().ok_or(MouseError::FailedToFindEventFileInOutputSyspath)?
            .parse::<u32>().map_err(|err| MouseError::FailedToParseEventFileForOutputID(err))?;
        Ok(Self{
            input_id,
            output_id,
            test_source,
            data_source,
            output,
            movement: MouseMovement::default()
        })
    }
    /// Asynchronously waits for the next syn report to happen for the trackpad input device
    pub async fn await_sync_event(&mut self) -> Result<(), MouseError>{
        loop{
            if self.test_source.next_event().await.map_err(|err| MouseError::FailedToGetEventFromTestStream(err))?.kind() ==  
                InputEventKind::Synchronization(Synchronization::SYN_REPORT) {return Ok(());}
        }
    }
    /// Poll function to update the mouse endlessly until it errors out
    pub async fn update_loop(&mut self) -> Result<(), MouseError>{
        loop{
            self.await_sync_event().await?;

            self.data_source.dispatch().map_err(|err| MouseError::FailedToDispatchLibinput(err))?;

            let events: Vec<Event> = self.data_source.by_ref().collect();
            for event in events{
                self.movement.process_event(event);
            }
            // emit mouse events
            let events = self.movement.get_output_events();
            if events.len() > 0 {
                self.output.emit(&events).map_err(|err| MouseError::FailedToEmitMouseEvents(err))?;
            }
        }
    }
}

/// Spawns a tokio task to automatically update the virtual mouse asynchronously
pub fn spawn_mouse_update_loop(mut manager: MouseManager) -> JoinHandle<Result<(), MouseError>>{
    tokio::task::spawn_local(async move {
        loop{
            manager.update_loop().await?;
        }
    })
}

/// Struct containing Mouse tracking data
#[derive(Default, Debug, Clone)]
pub struct MouseMovement{
    /// Delta x of mouse pointer location since last event was sent
    relx: f64,
    /// Delta y of mouse pointer location since last event was sent
    rely: f64,
    /// Delta scroll of the mouse since the last event was sent
    rel_scroll: f64,
    /// Delta scroll of the mouse with high resolution (normal*120) since the last event was sent
    rel_scroll_hr: f64,
    /// Delta horizontal scroll fo the mouse since the last event was sent
    rel_hscroll: f64,
    /// Delta horizontal scroll of the mouse with high resolution (normal*120) since the last event was sent
    rel_hscroll_hr: f64,
    /// 0 if the left click has been released, 1 if pressed, none otherwise
    left_button_event: Option<i32>,
    /// 0 if the right click has been released, 1 if pressed, none otherwise
    right_button_event: Option<i32>,
    /// 0 if the middle click has been released, 1 if pressed, none otherwise
    middle_button_event: Option<i32>,
}
impl MouseMovement{
    /// Reads in an event, and updates the movement values accordingly
    pub fn process_event(&mut self, event: Event) {
        match event{
            Event::Pointer(PointerEvent::Motion(ev)) => {
                self.relx += ev.dx();
                self.rely += ev.dy();
            },
            Event::Pointer(PointerEvent::Button(ev)) => {
                match ev.button() {
                    272 => {self.left_button_event = Some(match ev.button_state() {ButtonState::Pressed => 1, ButtonState::Released => 0});}
                    273 => {self.right_button_event = Some(match ev.button_state() {ButtonState::Pressed => 1, ButtonState::Released => 0});}
                    274 => {self.middle_button_event = Some(match ev.button_state() {ButtonState::Pressed => 1, ButtonState::Released => 0});}
                    _ => {}
                };
            },
            Event::Pointer(PointerEvent::ScrollFinger(ev)) => {
                if ev.has_axis(input::event::pointer::Axis::Vertical) {
                    self.rel_scroll += ev.scroll_value(input::event::pointer::Axis::Vertical)*-0.05;
                    self.rel_scroll_hr += ev.scroll_value(input::event::pointer::Axis::Vertical)*120.0*-0.05;
                }
                if ev.has_axis(input::event::pointer::Axis::Horizontal) {
                    self.rel_hscroll += ev.scroll_value(input::event::pointer::Axis::Horizontal)*-0.05;
                    self.rel_hscroll_hr += ev.scroll_value(input::event::pointer::Axis::Horizontal)*120.0*-0.05;
                }
            },
            _ => {}
        };
    }
    /// reduce delta changes of the mouse, returning the list of input event containing the reduction
    pub fn get_output_events(&mut self) -> Vec<InputEvent>{
        let mut event_storage = Vec::with_capacity(8);
        if let Some(val) = self.left_button_event.take(){
            event_storage.push(InputEvent::new(EventType::KEY, Key::BTN_LEFT.code(), val));
        }
        if let Some(val) = self.right_button_event.take(){
            event_storage.push(InputEvent::new(EventType::KEY, Key::BTN_RIGHT.code(), val));
        }
        if let Some(val) = self.middle_button_event.take(){
            event_storage.push(InputEvent::new(EventType::KEY, Key::BTN_MIDDLE.code(), val));
        }
        if self.rel_scroll.abs() >= 1.0 {
            event_storage.push(InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_WHEEL.0, self.rel_scroll.trunc() as i32));
            self.rel_scroll = self.rel_scroll.fract();
        }
        if self.rel_scroll_hr.abs() >= 1.0 {
            event_storage.push(InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_WHEEL_HI_RES.0, self.rel_scroll_hr.trunc() as i32));
            self.rel_scroll_hr = self.rel_scroll_hr.fract();
        }
        if self.rel_hscroll.abs() >= 1.0 {
            event_storage.push(InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_HWHEEL.0, self.rel_hscroll.trunc() as i32));
            self.rel_hscroll = self.rel_hscroll.fract();
        }
        if self.rel_hscroll_hr.abs() >= 1.0 {
            event_storage.push(InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_HWHEEL_HI_RES.0, self.rel_hscroll_hr.trunc() as i32));
            self.rel_hscroll_hr = self.rel_hscroll_hr.fract();
        }
        if self.relx.abs() >= 1.0 {
            event_storage.push(InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_X.0, self.relx.trunc() as i32));
            self.relx = self.relx.fract();
        }
        if self.rely.abs() >= 1.0 {
            event_storage.push(InputEvent::new(EventType::RELATIVE, RelativeAxisType::REL_Y.0, self.rely.trunc() as i32));
            self.rely = self.rely.fract();
        }
        return event_storage;
    }
}
