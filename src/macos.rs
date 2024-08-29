#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

use std::{
  ffi::{c_int, c_void},
  marker::{PhantomData, PhantomPinned},
  ptr::null_mut,
  sync::{mpsc, Arc, Barrier},
  thread::JoinHandle,
};

use tracing::{debug, trace, warn};

use crate::{Error, EventHandler, SleepQueryResponse};

pub struct Naptime {
  // The run loop id so we can ask it to stop
  run_loop: Option<CFRunLoopRefWrapper>,

  // We create a new macos run loop on a new thread so we can receive the power events
  run_loop_thread: Option<JoinHandle<()>>,
}

impl Naptime {
  pub fn new<E>(event_handler: E) -> Result<Self, Error>
  where
    E: EventHandler,
  {
    let event_handler = Box::new(event_handler);

    // spawn the thread that will subscribe to the power events
    let (tx, rx) = mpsc::channel();
    let barrier = Arc::new(Barrier::new(2));
    let thread_barrier = barrier.clone();
    let run_loop_thread =
      std::thread::spawn(move || run_loop_proc(event_handler, tx, thread_barrier));

    // wait for the thread to finish initializing
    let run_loop = rx.recv().unwrap()?;
    // SAFETY: retain the run loop so it doesn't get freed on us. We must release it when dropped
    unsafe {
      CFRetain(run_loop.0 as *const c_void);
    }
    barrier.wait();

    Ok(Self {
      run_loop: Some(run_loop),
      run_loop_thread: Some(run_loop_thread),
    })
  }
}

// SAFETY: Once the listener thread starts, we want to be able to stop it. This is a pointer to the
// run loop and the docs suggest the API is thread safe:
// <https://developer.apple.com/library/archive/documentation/Cocoa/Conceptual/Multithreading/RunLoopManagement/RunLoopManagement.html#//apple_ref/doc/uid/10000057i-CH16-SW26>
struct CFRunLoopRefWrapper(CFRunLoopRef);
unsafe impl Send for CFRunLoopRefWrapper {}
unsafe impl Sync for CFRunLoopRefWrapper {}

struct ThreadState {
  event_handler: Box<dyn EventHandler>,
  root_port: io_connect_t,
}

fn run_loop_proc(
  event_handler: Box<dyn EventHandler>,
  tx: mpsc::Sender<Result<CFRunLoopRefWrapper, Error>>,
  barrier: Arc<Barrier>,
) {
  // capture this thread's run loop
  // SAFETY: we need to retain run_loop and release it later when Notifier drops. This is probably
  // pedantic since we're in the thread that owns the run loop, but whatever
  let run_loop = unsafe {
    let run_loop = CFRunLoopGetCurrent();
    CFRetain(run_loop as *const c_void);
    run_loop
  };

  // create the state
  let mut state = Box::new(ThreadState {
    event_handler,
    root_port: 0,
  });
  let state_ptr = &mut *state as *mut ThreadState;

  // register for event notifications for power changes
  // SAFETY: AFAICT the callback cannot be invoked until we call CFRunLoopAddSource, so it should be
  // safe to capture the root_port and then assign it to the state after the call returns
  let mut notify_port_ref: IONotificationPortRef = null_mut();
  let mut notifier_object: io_object_t = 0;
  let root_port = unsafe {
    IORegisterForSystemPower(
      state_ptr as *mut c_void,
      &mut notify_port_ref,
      system_power_event_handler,
      &mut notifier_object,
    )
  };
  if root_port == 0 {
    // SAFETY: make sure we release our interest in the run loop here
    unsafe { CFRelease(run_loop as *const c_void) };

    tx.send(Err(Error(format!(
      "IORegisterForSystemPower failed. code={:08x}",
      root_port
    ))))
    .unwrap();
    return;
  }
  state.root_port = root_port;

  // tell Notifier about the run loop and wait for it to receive it
  tx.send(Ok(CFRunLoopRefWrapper(run_loop))).unwrap();
  drop(tx);
  barrier.wait();
  drop(barrier);

  unsafe {
    // tell the run loop we want to get notifications about power
    CFRunLoopAddSource(
      run_loop,
      IONotificationPortGetRunLoopSource(notify_port_ref),
      kCFRunLoopCommonModes,
    );

    // drive the run loop, this won't return until CFRunLoopStop() is called when Notifier is dropped
    println!("starting run loop");
    CFRunLoopRun();
    println!("loop done!");

    // unregister for power events
    CFRunLoopRemoveSource(
      run_loop,
      IONotificationPortGetRunLoopSource(notify_port_ref),
      kCFRunLoopCommonModes,
    );

    // cleanup
    IODeregisterForSystemPower(&mut notifier_object);
    IOServiceClose(root_port);
    IONotificationPortDestroy(notify_port_ref);

    // release the run loop
    CFRelease(run_loop as *const c_void);
  }

  trace!("run loop thread exiting");
}

extern "C" fn system_power_event_handler(
  refCon: *mut c_void,
  _service: io_service_t,
  messageType: u32,
  messageArgument: *mut c_void,
) {
  // SAFETY: the user data into this function is the ThreadState object whose lifetime is governed
  // by run_loop_proc(). Also, Box::from_raw() will take ownership of the pointer, so we're going to
  // need to take it before leaving the function
  let state_ptr = refCon as *mut ThreadState;
  let mut state = unsafe { Box::from_raw(state_ptr) };

  trace!("message: {:08x}", messageType);
  match messageType {
    // This is a notification that the system wants to sleep, but we have a chance to deny it. We
    // are required to acknowledge the message with either IOAllowPowerChange() or
    // IOCancelPowerChange() and the OS will wait 30 seconds for us to do so before giving up.
    // See: https://developer.apple.com/documentation/iokit/1557114-ioregisterforsystempower?language=objc
    kIOMessageCanSystemSleep => {
      trace!("kIOMessageCanSystemSleep");

      let response = state.event_handler.sleep_query();
      match response {
        SleepQueryResponse::Allow => {
          let ret = unsafe { IOAllowPowerChange(state.root_port, messageArgument) };
          if ret != kIOReturnSuccess {
            warn!("IOAllowPowerChange failed. ret={:08x}", ret);
          }
        }
        SleepQueryResponse::Deny => {
          let ret = unsafe { IOCancelPowerChange(state.root_port, messageArgument) };
          if ret != kIOReturnSuccess {
            warn!("IOCancelPowerChange failed. ret={:08x}", ret);
          }
        }
      }
    }

    // This is a notification that the system is definitely going to sleep. We are required to
    // acknowledge the message with IOAllowPowerChange() and the OS will wait 30 seconds for us to
    // do so before giving up.
    // See: https://developer.apple.com/documentation/iokit/1557114-ioregisterforsystempower?language=objc
    kIOMessageSystemWillSleep => {
      trace!("kIOMessageSystemWillSleep");
      state.event_handler.sleep();
      let ret = unsafe { IOAllowPowerChange(state.root_port, messageArgument) };
      if ret != kIOReturnSuccess {
        warn!("IOAllowPowerChange failed. ret={:08x}", ret);
      }
    }
    kIOMessageSystemWillNotSleep => {
      trace!("kIOMessageSystemWillNotSleep");
      state.event_handler.sleep_failed();
    }
    kIOMessageSystemWillPowerOn => {
      trace!("kIOMessageSystemWillPowerOn");
    }
    kIOMessageSystemHasPoweredOn => {
      trace!("kIOMessageSystemHasPoweredOn");
      state.event_handler.wake();
    }
    _ => {
      debug!("unknown message type");
    }
  }

  // SAFETY: We don't want the Box to drop our state pointer. The run loop thread will do that on exit
  Box::leak(state);
}

impl Drop for Naptime {
  fn drop(&mut self) {
    // tell the thread to stop
    // SAFETY: this is where we release our interest in the run loop
    if let Some(CFRunLoopRefWrapper(run_loop)) = self.run_loop.take() {
      unsafe {
        CFRunLoopStop(run_loop);
        CFRelease(run_loop as *const c_void);
      }
    }

    // wait for the thread to finish
    if let Some(thread_handle) = self.run_loop_thread.take() {
      thread_handle.join().unwrap();
    }
  }
}

const fn err_system(x: u32) -> u32 {
  (x & 0x3f) << 26
}
const fn err_sub(x: u32) -> u32 {
  (x & 0xfff) << 14
}

type natural_t = u32;
type mach_port_t = natural_t;
type io_object_t = mach_port_t;
type io_connect_t = io_object_t;
type io_service_t = io_object_t;
type kern_return_t = c_int;

//
// Core Foundation
//

type CFTypeRef = *const c_void;

#[repr(C)]
struct __CFString(c_void);
type CFStringRef = *const __CFString;

#[repr(C)]
struct __CFRunLoop {
  _data: [u8; 0],
  _marker: PhantomData<(*mut u8, PhantomPinned)>,
}
type CFRunLoopRef = *mut __CFRunLoop;

#[repr(C)]
struct __CFRunLoopSource {
  _data: [u8; 0],
  _marker: PhantomData<(*mut u8, PhantomPinned)>,
}
type CFRunLoopSourceRef = *mut __CFRunLoopSource;

type CFRunLoopMode = CFStringRef;

#[cfg_attr(target_os = "macos", link(name = "CoreFoundation", kind = "framework"))]
extern "C" {
  static kCFRunLoopCommonModes: CFStringRef;

  fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFRunLoopMode);
  fn CFRunLoopRemoveSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFRunLoopMode);
  fn CFRunLoopGetCurrent() -> CFRunLoopRef;
  fn CFRunLoopRun();
  fn CFRunLoopStop(rl: CFRunLoopRef);

  fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
  fn CFRelease(cf: CFTypeRef);
}

//
// IOKit
//

const fn sys_iokit() -> u32 {
  err_system(0x38)
}
const fn sub_iokit_common() -> u32 {
  err_sub(0)
}
const fn iokit_common_msg(message: u32) -> u32 {
  sys_iokit() | sub_iokit_common() | message
}

const kIOMessageCanSystemSleep: u32 = iokit_common_msg(0x270);
const kIOMessageSystemWillSleep: u32 = iokit_common_msg(0x280);
const kIOMessageSystemWillNotSleep: u32 = iokit_common_msg(0x290);

const kIOMessageSystemHasPoweredOn: u32 = iokit_common_msg(0x300);
const kIOMessageSystemWillPowerOn: u32 = iokit_common_msg(0x320);

const kIOReturnSuccess: i32 = 0;

type IOReturn = kern_return_t;

#[repr(C)]
struct IONotificationPort {
  _data: [u8; 0],
  _marker: PhantomData<(*mut u8, PhantomPinned)>,
}
type IONotificationPortRef = *mut IONotificationPort;

type IOServiceInterestCallback = unsafe extern "C" fn(
  refcon: *mut c_void,
  service: io_service_t,
  messageType: u32,
  messageArgument: *mut c_void,
);

#[cfg_attr(target_os = "macos", link(name = "IOKit", kind = "framework"))]
extern "C" {
  fn IORegisterForSystemPower(
    refcon: *mut c_void,
    thePortRef: *mut IONotificationPortRef,
    callback: IOServiceInterestCallback,
    notifier: *mut io_object_t,
  ) -> io_connect_t;
  fn IODeregisterForSystemPower(notifier: *mut io_object_t) -> IOReturn;

  fn IONotificationPortGetRunLoopSource(notify: IONotificationPortRef) -> CFRunLoopSourceRef;
  fn IONotificationPortDestroy(notify: IONotificationPortRef);

  fn IOAllowPowerChange(kernelPort: io_connect_t, notificationID: *const c_void) -> IOReturn;
  fn IOCancelPowerChange(kernelPort: io_connect_t, notificationID: *const c_void) -> IOReturn;

  fn IOServiceClose(connect: io_connect_t) -> kern_return_t;
}
