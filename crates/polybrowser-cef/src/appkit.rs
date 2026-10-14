//! The parts of the backend that talk to AppKit and the dynamic loader.

use std::{
    ffi::CString,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
};

use objc2::{
    ffi, msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, AnyProtocol, Bool, ClassBuilder, Sel},
    sel, MainThreadMarker,
};
use objc2_app_kit::{NSApplication, NSEvent, NSView};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use polybrowser_core::Rect;

const FRAMEWORK: &str = "Chromium Embedded Framework.framework/Chromium Embedded Framework";

/// Which executable of the application bundle is running.
#[derive(Clone, Copy)]
pub(crate) enum Role {
    /// `<App>.app/Contents/MacOS/<app>`
    Main,
    /// `<App>.app/Contents/Frameworks/<app> Helper.app/Contents/MacOS/<app> Helper`
    Helper,
}

/// Loads Chromium from the application bundle this executable is part of.
pub(crate) fn load_framework(role: Role) -> Result<(), String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("cannot locate the running executable: {error}"))?;
    let frameworks: PathBuf = match role {
        Role::Main => "../Frameworks".into(),
        Role::Helper => "../../..".into(),
    };
    // Relative to the executable as it was started, so that a symlinked
    // executable finds the bundle it was started from.
    let path = executable
        .parent()
        .unwrap_or(&executable)
        .join(frameworks)
        .join(FRAMEWORK);
    let Ok(path) = path.canonicalize() else {
        return Err(format!(
            "Chromium was not found at {}. On macOS an application using the Chromium engine has \
             to run from an .app bundle that contains it; `polybrowser-bundle` creates one.",
            path.display()
        ));
    };
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "the path to the application bundle contains a NUL byte".to_owned())?;
    // SAFETY: a valid NUL-terminated string that outlives the call.
    if cef::load_library(Some(unsafe { &*c_path.as_ptr() })) != 1 {
        return Err(format!("could not load Chromium from {}", path.display()));
    }
    Ok(())
}

static HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);
static ORIGINAL_APP_CLASS: OnceLock<&'static AnyClass> = OnceLock::new();

/// Makes the running `NSApplication` implement `CefAppProtocol`.
///
/// Chromium asks the application object whether an event is currently being
/// dispatched (`isHandlingSendEvent`), and crashes if it cannot answer. A
/// Chromium application answers by subclassing `NSApplication`, but here the
/// application object belongs to whatever toolkit the host uses and already
/// exists. So the existing object is moved to a subclass of its own class,
/// created on the spot, that adds the three methods. This is the technique
/// Cocoa uses for key-value observing, and what JCEF does for the same reason.
pub(crate) fn adopt_cef_app_protocol(mtm: MainThreadMarker) -> Result<(), String> {
    let app = NSApplication::sharedApplication(mtm);
    let app: &AnyObject = &app;
    let original = app.class();
    if original.responds_to(sel!(isHandlingSendEvent)) {
        // The host is a Chromium application already.
        return Ok(());
    }
    let _ = ORIGINAL_APP_CLASS.set(original);

    let mut builder = ClassBuilder::new(c"PolybrowserChromiumApplication", original)
        .ok_or("the application class was already extended for Chromium")?;
    // SAFETY: the signatures match the Objective-C declarations in CEF's
    // `cef_application_mac.h` and AppKit's `-[NSApplication sendEvent:]`.
    unsafe {
        builder.add_method(
            sel!(isHandlingSendEvent),
            is_handling_send_event as extern "C-unwind" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(setHandlingSendEvent:),
            set_handling_send_event as extern "C-unwind" fn(_, _, _),
        );
        builder.add_method(
            sel!(sendEvent:),
            send_event as extern "C-unwind" fn(_, _, _),
        );
    }
    for name in [c"CrAppProtocol", c"CrAppControlProtocol", c"CefAppProtocol"] {
        if let Some(protocol) = AnyProtocol::get(name) {
            builder.add_protocol(protocol);
        }
    }
    let class = builder.register();

    // SAFETY: the new class is a subclass of the object's class that adds
    // methods and no instance variables, so the object's layout is unchanged.
    unsafe { ffi::object_setClass(std::ptr::from_ref(app).cast_mut(), class) };
    Ok(())
}

extern "C-unwind" fn is_handling_send_event(_this: &AnyObject, _cmd: Sel) -> Bool {
    Bool::new(HANDLING_SEND_EVENT.load(Ordering::Relaxed))
}

extern "C-unwind" fn set_handling_send_event(_this: &AnyObject, _cmd: Sel, handling: Bool) {
    HANDLING_SEND_EVENT.store(handling.as_bool(), Ordering::Relaxed);
}

extern "C-unwind" fn send_event(this: &AnyObject, _cmd: Sel, event: &NSEvent) {
    let was_handling = HANDLING_SEND_EVENT.swap(true, Ordering::Relaxed);
    if let Some(original) = ORIGINAL_APP_CLASS.get() {
        // SAFETY: `original` is the superclass of the class this method was
        // added to, and implements `sendEvent:` with this signature.
        unsafe {
            let _: () = msg_send![super(this, *original), sendEvent: event];
        }
    }
    HANDLING_SEND_EVENT.store(was_handling, Ordering::Relaxed);
}

/// Borrows the `NSView` behind a pointer handed out by a window handle or by
/// Chromium.
///
/// # Safety
/// `pointer` must be null or point to a live `NSView`, and this must be the
/// main thread.
pub(crate) unsafe fn view_from_pointer(pointer: *mut std::ffi::c_void) -> Option<Retained<NSView>> {
    unsafe { Retained::retain(pointer.cast::<NSView>()) }
}

/// Converts a rectangle measured from the top-left corner of `parent` into
/// the coordinates `parent` uses for its subviews. AppKit views put the
/// origin at the bottom-left unless they declare themselves flipped.
pub(crate) fn frame_in(parent: &NSView, rect: Rect) -> NSRect {
    let (width, height) = (rect.width.max(0.0), rect.height.max(0.0));
    let y = if parent.isFlipped() {
        rect.y
    } else {
        parent.bounds().size.height - rect.y - height
    };
    NSRect::new(NSPoint::new(rect.x, y), NSSize::new(width, height))
}

pub(crate) fn set_frame(view: &NSView, rect: Rect) {
    // SAFETY: plain AppKit calls on the main thread.
    unsafe {
        if let Some(parent) = view.superview() {
            view.setFrame(frame_in(&parent, rect));
        }
    }
}

pub(crate) fn remove_from_parent(view: &NSView) {
    view.removeFromSuperview();
}

pub(crate) fn set_hidden(view: &NSView, hidden: bool) {
    view.setHidden(hidden);
}

pub(crate) fn make_first_responder(view: &NSView) {
    if let Some(window) = view.window() {
        window.makeFirstResponder(Some(view));
    }
}
