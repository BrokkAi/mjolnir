//! macOS observation backend for the computer-interaction contract.
//!
//! CoreGraphics provides display geometry; ScreenCaptureKit supplies display
//! images and ImageIO encodes them as PNG. This module intentionally does not
//! request Screen Recording access, inject input, start an MCP listener, or
//! make policy decisions.

use std::{
    ffi::{c_char, c_void},
    io::Cursor,
    sync::mpsc,
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use block2::RcBlock;
use image::{GenericImageView as _, ImageFormat};
use objc2::{ClassType as _, sel};
use objc2_core_foundation::{CGPoint as ObjcCGPoint, CGRect as ObjcCGRect, CGSize as ObjcCGSize};
use objc2_core_graphics::CGImage;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::SCScreenshotManager;
use tokio_util::sync::CancellationToken;

use crate::computer::{
    BackendAction, CaptureRegion, ComputerBackend, ComputerError, ComputerPermission,
    CurrentDisplay, DesktopPoint, DisplayId, EncodedImage, HostLockState, ImageLimits, KeyModifier,
    NamedKey, Observation, ObservationId, ObservationMetadata, ObserveArgs, PermissionReadiness,
    PermissionState, PixelSize, PointerButton, SourceRegion,
};

type CGDirectDisplayID = u32;
type CGError = i32;
type CGDisplayModeRef = *const c_void;
type CGImageRef = *const c_void;
type CFDataRef = *const c_void;
type CFMutableDataRef = *mut c_void;
type CFStringRef = *const c_void;
type CGImageDestinationRef = *mut c_void;
type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;
type CFDictionaryRef = *const c_void;

/// Layout-only bindings for CoreFoundation's predefined dictionary callback
/// tables. The callbacks themselves remain owned by CoreFoundation.
#[repr(C)]
struct CFDictionaryKeyCallBacks {
    version: isize,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
    hash: *const c_void,
}

#[repr(C)]
struct CFDictionaryValueCallBacks {
    version: isize,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

const KCG_ERROR_SUCCESS: CGError = 0;
const KCG_HID_EVENT_TAP: u32 = 0;
const KCG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE: i32 = 1;
const KCG_SCROLL_EVENT_UNIT_PIXEL: u32 = 1;
const KCG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const KCG_EVENT_LEFT_MOUSE_UP: u32 = 2;
const KCG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const KCG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
const KCG_EVENT_MOUSE_MOVED: u32 = 5;
const KCG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const KCG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const KCG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const KCG_EVENT_OTHER_MOUSE_UP: u32 = 26;
const KCG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;
const KCG_EVENT_FLAG_MASK_SHIFT: u64 = 1 << 17;
const KCG_EVENT_FLAG_MASK_CONTROL: u64 = 1 << 18;
const KCG_EVENT_FLAG_MASK_ALTERNATE: u64 = 1 << 19;
const KCG_EVENT_FLAG_MASK_COMMAND: u64 = 1 << 20;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> CGDirectDisplayID;
    fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> CGError;
    fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    fn CGDisplayCopyDisplayMode(display: CGDirectDisplayID) -> CGDisplayModeRef;
    fn CGDisplayModeGetPixelWidth(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: CGDisplayModeRef) -> usize;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
    fn CGEventSourceCreate(state_id: i32) -> CGEventSourceRef;
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: u16,
        key_down: bool,
    ) -> CGEventRef;
    fn CGEventKeyboardSetUnicodeString(event: CGEventRef, length: u16, string: *const u16);
    fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventSetLocation(event: CGEventRef, location: CGPoint);
    fn CGEventPost(tap: u32, event: CGEventRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataCreateMutable(allocator: *const c_void, capacity: isize) -> CFMutableDataRef;
    fn CFDataGetLength(data: CFDataRef) -> isize;
    fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFRelease(cf: *const c_void);
    fn CFDictionaryGetValue(dictionary: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(boolean: *const c_void) -> bool;
    fn CFDictionaryCreateMutable(
        allocator: *const c_void,
        capacity: isize,
        key_callbacks: *const CFDictionaryKeyCallBacks,
        value_callbacks: *const CFDictionaryValueCallBacks,
    ) -> *mut c_void;
    fn CFDictionarySetValue(dictionary: *mut c_void, key: *const c_void, value: *const c_void);
    static kCFBooleanTrue: *const c_void;
    static kCFTypeDictionaryKeyCallBacks: CFDictionaryKeyCallBacks;
    static kCFTypeDictionaryValueCallBacks: CFDictionaryValueCallBacks;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageDestinationCreateWithData(
        data: CFMutableDataRef,
        type_identifier: CFStringRef,
        image_count: usize,
        options: *const c_void,
    ) -> CGImageDestinationRef;
    fn CGImageDestinationAddImage(
        destination: CGImageDestinationRef,
        image: CGImageRef,
        properties: *const c_void,
    );
    fn CGImageDestinationFinalize(destination: CGImageDestinationRef) -> bool;
}

/// A CoreGraphics-backed observation implementation for macOS.
#[derive(Debug, Clone, Copy)]
pub struct MacosComputerBackend {
    limits: ImageLimits,
}

impl Default for MacosComputerBackend {
    fn default() -> Self {
        Self::new(ImageLimits::DEFAULT)
    }
}

impl MacosComputerBackend {
    pub const fn new(limits: ImageLimits) -> Self {
        Self { limits }
    }

    /// Returns current geometry for the display identifier used in an
    /// observation. Callers compare this to `ObservationMetadata` before
    /// emitting input, which makes display changes fail closed.
    pub fn current_display(&self, display_id: &DisplayId) -> Result<CurrentDisplay, ComputerError> {
        let display = parse_display_id(display_id)?;
        display_info(display).map(|info| info.current_display())
    }

    fn capture(
        &self,
        request: ObserveArgs,
        cancellation: &CancellationToken,
    ) -> Result<Observation, ComputerError> {
        request.validate(self.limits)?;
        check_cancelled(cancellation)?;
        if !screen_recording_granted() {
            return Err(ComputerError::ScreenRecordingPermission(
                PermissionState::NotGranted,
            ));
        }

        let display = match request.display_id.as_ref() {
            Some(id) => parse_display_id(id)?,
            None => main_display_id(),
        };
        let info = display_info(display)?;
        check_cancelled(cancellation)?;

        let png = screen_capture_kit_png(display_rect(&info), cancellation)?;
        check_cancelled(cancellation)?;
        let full =
            image::load_from_memory_with_format(&png, ImageFormat::Png).map_err(|error| {
                ComputerError::Backend(format!("decode ScreenCaptureKit PNG: {error}"))
            })?;
        if full.dimensions() != (info.pixel_size.width, info.pixel_size.height) {
            return Err(ComputerError::Backend(format!(
                "ScreenCaptureKit returned {}x{} pixels for display declared as {}x{}",
                full.width(),
                full.height(),
                info.pixel_size.width,
                info.pixel_size.height
            )));
        }

        let source_region = source_region(request.region, &info)?;
        let cropped = full.crop_imm(
            source_region.x,
            source_region.y,
            source_region.width,
            source_region.height,
        );
        let max_width = request.max_image_width.unwrap_or(self.limits.max_width);
        let max_height = request.max_image_height.unwrap_or(self.limits.max_height);
        let returned = downscale_to_limits(cropped, max_width, max_height);
        let returned_size = PixelSize {
            width: returned.width(),
            height: returned.height(),
        };
        self.limits.validate_image(returned_size, 0)?;
        check_cancelled(cancellation)?;

        let mut encoded = Vec::new();
        returned
            .write_to(&mut Cursor::new(&mut encoded), ImageFormat::Png)
            .map_err(|error| ComputerError::Backend(format!("encode observation PNG: {error}")))?;
        check_cancelled(cancellation)?;
        let observation = Observation {
            metadata: ObservationMetadata {
                observation_id: new_observation_id()?,
                display_id: info.display_id,
                display_origin: info.origin,
                display_pixel_size: info.pixel_size,
                display_scale_x: info.scale_x,
                display_scale_y: info.scale_y,
                source_region,
                returned_image_size: returned_size,
                mime_type: "image/png".to_string(),
                created_at_unix_ms: unix_millis()?,
                expires_at_unix_ms: unix_millis()?.saturating_add(30_000),
            },
            image: EncodedImage::from_bytes(&encoded, self.limits)?,
        };
        observation.validate(self.limits)?;
        Ok(observation)
    }
}

#[async_trait]
impl ComputerBackend for MacosComputerBackend {
    async fn observe(
        &self,
        request: ObserveArgs,
        cancellation: CancellationToken,
    ) -> Result<Observation, ComputerError> {
        let backend = *self;
        tokio::task::spawn_blocking(move || backend.capture(request, &cancellation))
            .await
            .map_err(|error| ComputerError::Backend(format!("capture task failed: {error}")))?
    }

    async fn permission_readiness(
        &self,
        cancellation: CancellationToken,
    ) -> Result<PermissionReadiness, ComputerError> {
        check_cancelled(&cancellation)?;
        let screen_recording = if screen_recording_granted() {
            PermissionState::Granted
        } else {
            PermissionState::NotGranted
        };
        Ok(PermissionReadiness {
            screen_recording,
            accessibility: if accessibility_granted() {
                PermissionState::Granted
            } else {
                PermissionState::NotGranted
            },
        })
    }

    async fn request_permission(
        &self,
        permission: ComputerPermission,
        cancellation: CancellationToken,
    ) -> Result<PermissionReadiness, ComputerError> {
        check_cancelled(&cancellation)?;
        tokio::task::spawn_blocking(move || {
            check_cancelled(&cancellation)?;
            match permission {
                ComputerPermission::ScreenRecording => request_screen_recording_permission(),
                ComputerPermission::Accessibility => request_accessibility_permission(),
            }
            check_cancelled(&cancellation)?;
            Ok(PermissionReadiness {
                screen_recording: if screen_recording_granted() {
                    PermissionState::Granted
                } else {
                    PermissionState::NotGranted
                },
                accessibility: if accessibility_granted() {
                    PermissionState::Granted
                } else {
                    PermissionState::NotGranted
                },
            })
        })
        .await
        .map_err(|error| {
            ComputerError::Backend(format!("permission request task failed: {error}"))
        })?
    }

    async fn current_display(
        &self,
        display_id: DisplayId,
        cancellation: CancellationToken,
    ) -> Result<CurrentDisplay, ComputerError> {
        check_cancelled(&cancellation)?;
        self.current_display(&display_id)
    }

    async fn host_lock_state(
        &self,
        cancellation: CancellationToken,
    ) -> Result<HostLockState, ComputerError> {
        check_cancelled(&cancellation)?;
        Ok(host_lock_state())
    }

    async fn execute(
        &self,
        action: BackendAction,
        cancellation: CancellationToken,
    ) -> Result<(), ComputerError> {
        let backend = *self;
        tokio::task::spawn_blocking(move || backend.execute_action(action, &cancellation))
            .await
            .map_err(|error| ComputerError::Backend(format!("input task failed: {error}")))?
    }
}

impl MacosComputerBackend {
    fn execute_action(
        &self,
        action: BackendAction,
        cancellation: &CancellationToken,
    ) -> Result<(), ComputerError> {
        check_cancelled(cancellation)?;
        if !accessibility_granted() {
            return Err(ComputerError::AccessibilityPermission(
                PermissionState::NotGranted,
            ));
        }
        match host_lock_state() {
            HostLockState::Unlocked => {}
            HostLockState::Locked => return Err(ComputerError::HostLocked),
            HostLockState::Unknown => return Err(ComputerError::HostLockStateUnknown),
        }
        let source = EventSource::new()?;
        match action {
            BackendAction::Verify => {
                let point = Event::current_pointer_location(&source)?;
                post_mouse(
                    &source,
                    KCG_EVENT_MOUSE_MOVED,
                    point.x,
                    point.y,
                    PointerButton::Left,
                )?;
            }
            BackendAction::Move { x, y } => {
                post_mouse(&source, KCG_EVENT_MOUSE_MOVED, x, y, PointerButton::Left)?;
            }
            BackendAction::Click { x, y, button } => {
                post_mouse(&source, mouse_down_event(button), x, y, button)?;
                check_cancelled(cancellation)?;
                post_mouse(&source, mouse_up_event(button), x, y, button)?;
            }
            BackendAction::DoubleClick { x, y, button } => {
                for _ in 0..2 {
                    post_mouse(&source, mouse_down_event(button), x, y, button)?;
                    check_cancelled(cancellation)?;
                    post_mouse(&source, mouse_up_event(button), x, y, button)?;
                    check_cancelled(cancellation)?;
                }
            }
            BackendAction::Drag { from, to, button } => {
                post_mouse(&source, KCG_EVENT_MOUSE_MOVED, from.0, from.1, button)?;
                check_cancelled(cancellation)?;
                post_mouse(&source, mouse_down_event(button), from.0, from.1, button)?;
                check_cancelled(cancellation)?;
                post_mouse(&source, mouse_drag_event(button), to.0, to.1, button)?;
                check_cancelled(cancellation)?;
                post_mouse(&source, mouse_up_event(button), to.0, to.1, button)?;
            }
            BackendAction::TypeText { text } => {
                for character in text.chars() {
                    check_cancelled(cancellation)?;
                    post_text(&source, character)?;
                    check_cancelled(cancellation)?;
                }
            }
            BackendAction::Key { key, modifiers } => {
                let flags = key_modifier_flags(&modifiers);
                post_key(&source, key_code(key), true, flags)?;
                check_cancelled(cancellation)?;
                post_key(&source, key_code(key), false, flags)?;
            }
            BackendAction::Scroll {
                x,
                y,
                delta_x,
                delta_y,
            } => {
                post_scroll(&source, x, y, delta_x, delta_y)?;
            }
        }
        check_cancelled(cancellation)
    }
}

#[derive(Debug, Clone)]
struct DisplayInfo {
    display_id: DisplayId,
    origin: DesktopPoint,
    pixel_size: PixelSize,
    scale_x: f64,
    scale_y: f64,
    point_size: (f64, f64),
}

impl DisplayInfo {
    fn current_display(&self) -> CurrentDisplay {
        CurrentDisplay {
            display_id: self.display_id.clone(),
            origin: self.origin,
            pixel_size: self.pixel_size,
            scale_x: self.scale_x,
            scale_y: self.scale_y,
        }
    }
}

fn main_display_id() -> CGDirectDisplayID {
    // SAFETY: CoreGraphics returns the currently configured main display id.
    unsafe { CGMainDisplayID() }
}

fn screen_recording_granted() -> bool {
    // SAFETY: This read-only CoreGraphics check does not show a permission
    // prompt and has no ownership requirements.
    unsafe { CGPreflightScreenCaptureAccess() }
}

fn accessibility_granted() -> bool {
    // SAFETY: AXIsProcessTrusted only queries the host app's Accessibility
    // grant. It neither opens a prompt nor injects any input.
    unsafe { AXIsProcessTrusted() }
}

fn request_screen_recording_permission() {
    // SAFETY: this is called only by Mjolnir Computer.app through its
    // authenticated host IPC. macOS owns whether a prompt is shown.
    let _ = unsafe { CGRequestScreenCaptureAccess() };
}

fn request_accessibility_permission() {
    let Ok(options) = accessibility_prompt_options() else {
        return;
    };
    // SAFETY: options remains valid throughout this synchronous permission
    // request; no ownership transfers to Accessibility.
    let _ = unsafe { AXIsProcessTrustedWithOptions(options.0) };
}

fn accessibility_prompt_options() -> Result<CoreFoundationObject, ComputerError> {
    let key = CoreFoundationString::new("AXTrustedCheckOptionPrompt")?;
    // SAFETY: the CoreFoundation callback tables retain the CFString key and
    // Boolean value while the dictionary exists. Null callbacks use pointer
    // equality, so this fresh key would not match Accessibility's constant.
    let options = unsafe {
        CFDictionaryCreateMutable(
            std::ptr::null(),
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    };
    if options.is_null() {
        return Err(ComputerError::Backend(
            "allocate Accessibility permission options failed".to_string(),
        ));
    }
    let options = CoreFoundationObject(options.cast());
    // SAFETY: `options` is a valid mutable CFDictionary and `key` remains
    // alive through insertion. `kCFBooleanTrue` is process-lifetime.
    unsafe { CFDictionarySetValue(options.0.cast_mut(), key.0, kCFBooleanTrue) };
    Ok(options)
}

fn host_lock_state() -> HostLockState {
    // SAFETY: CoreGraphics returns a retained snapshot of the current login
    // session dictionary. The wrapper releases it before returning.
    let dictionary = unsafe { CGSessionCopyCurrentDictionary() };
    if dictionary.is_null() {
        return HostLockState::Unknown;
    }
    let dictionary = CoreFoundationObject(dictionary);
    host_lock_state_from_session_flags(
        session_boolean(dictionary.0, "kCGSSessionOnConsoleKey"),
        session_boolean(dictionary.0, "CGSSessionScreenIsLocked"),
    )
}

fn session_boolean(dictionary: CFDictionaryRef, key_name: &str) -> Option<bool> {
    let key = CoreFoundationString::new(key_name).ok()?;
    // SAFETY: both CoreFoundation objects remain valid for this lookup.
    let value = unsafe { CFDictionaryGetValue(dictionary, key.0) };
    if value.is_null()
        // SAFETY: CoreFoundation type-id queries are valid for a non-null
        // object pointer and do not transfer ownership.
        || unsafe { CFGetTypeID(value) } != unsafe { CFBooleanGetTypeID() }
    {
        return None;
    }
    // SAFETY: the type check above proves the dictionary value is a CFBoolean.
    Some(unsafe { CFBooleanGetValue(value) })
}

fn host_lock_state_from_session_flags(
    on_console: Option<bool>,
    screen_is_locked: Option<bool>,
) -> HostLockState {
    match (on_console, screen_is_locked) {
        // The screen-lock key is authoritative: the console can stay active
        // while macOS presents the lock screen.
        (_, Some(true)) => HostLockState::Locked,
        (Some(true), Some(false)) => HostLockState::Unlocked,
        // A session that is not on the console cannot safely receive input.
        (Some(false), Some(false)) => HostLockState::Locked,
        // Missing or malformed state is fail-closed at the policy boundary.
        _ => HostLockState::Unknown,
    }
}

struct EventSource(CGEventSourceRef);

impl EventSource {
    fn new() -> Result<Self, ComputerError> {
        // SAFETY: CoreGraphics returns a retained event source owned by this
        // wrapper, or null on allocation failure.
        let source = unsafe { CGEventSourceCreate(KCG_EVENT_SOURCE_STATE_HID_SYSTEM_STATE) };
        if source.is_null() {
            return Err(ComputerError::Backend(
                "create CoreGraphics event source failed".to_string(),
            ));
        }
        Ok(Self(source))
    }
}

impl Drop for EventSource {
    fn drop(&mut self) {
        // SAFETY: EventSource::new created a retained CoreFoundation object.
        unsafe { CFRelease(self.0) };
    }
}

struct Event(CGEventRef);

impl Event {
    fn current_pointer_location(source: &EventSource) -> Result<CGPoint, ComputerError> {
        // SAFETY: `source` is a valid retained event source. CoreGraphics
        // returns a retained snapshot event whose location is the current
        // pointer position; `Event` releases it after the read.
        let event = Self::from_raw(unsafe { CGEventCreate(source.0) }, "read pointer location")?;
        // SAFETY: `event` owns a valid CoreGraphics event.
        let point = unsafe { CGEventGetLocation(event.0) };
        validate_point(point.x, point.y)?;
        Ok(point)
    }

    fn mouse(
        source: &EventSource,
        event_type: u32,
        x: f64,
        y: f64,
        button: PointerButton,
    ) -> Result<Self, ComputerError> {
        validate_point(x, y)?;
        // SAFETY: source is a valid retained event source and the scalar
        // values are finite desktop coordinates checked above.
        let event = unsafe {
            CGEventCreateMouseEvent(source.0, event_type, CGPoint { x, y }, mouse_button(button))
        };
        Self::from_raw(event, "create mouse event")
    }

    fn keyboard(
        source: &EventSource,
        virtual_key: u16,
        key_down: bool,
    ) -> Result<Self, ComputerError> {
        // SAFETY: source is a valid retained event source and virtual key is
        // one of the named-key constants selected below.
        let event = unsafe { CGEventCreateKeyboardEvent(source.0, virtual_key, key_down) };
        Self::from_raw(event, "create keyboard event")
    }

    fn scroll(source: &EventSource, delta_x: i32, delta_y: i32) -> Result<Self, ComputerError> {
        // SAFETY: source is valid; a two-axis pixel scroll has the supplied
        // bounded signed deltas and no borrowed pointers.
        let event = unsafe {
            CGEventCreateScrollWheelEvent2(
                source.0,
                KCG_SCROLL_EVENT_UNIT_PIXEL,
                2,
                delta_y,
                delta_x,
                0,
            )
        };
        Self::from_raw(event, "create scroll event")
    }

    fn from_raw(event: CGEventRef, operation: &str) -> Result<Self, ComputerError> {
        if event.is_null() {
            Err(ComputerError::Backend(format!("{operation} failed")))
        } else {
            Ok(Self(event))
        }
    }

    fn set_flags(&self, flags: u64) {
        // SAFETY: self.0 is a valid event owned by this wrapper.
        unsafe { CGEventSetFlags(self.0, flags) };
    }

    fn set_location(&self, x: f64, y: f64) -> Result<(), ComputerError> {
        validate_point(x, y)?;
        // SAFETY: self.0 is valid and the location uses checked finite values.
        unsafe { CGEventSetLocation(self.0, CGPoint { x, y }) };
        Ok(())
    }

    fn set_unicode(&self, units: &[u16]) -> Result<(), ComputerError> {
        let length = u16::try_from(units.len()).map_err(|_| {
            ComputerError::Backend("unicode key event exceeds u16 length".to_string())
        })?;
        // SAFETY: self.0 is valid and `units` remains alive for the call.
        unsafe { CGEventKeyboardSetUnicodeString(self.0, length, units.as_ptr()) };
        Ok(())
    }

    fn post(&self) {
        // SAFETY: self.0 is a valid event. Posting is intentionally confined
        // to the Mjolnir Computer app after readiness and lock checks.
        unsafe { CGEventPost(KCG_HID_EVENT_TAP, self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: Event::from_raw accepts only a retained CGEventRef.
        unsafe { CFRelease(self.0) };
    }
}

fn post_mouse(
    source: &EventSource,
    event_type: u32,
    x: f64,
    y: f64,
    button: PointerButton,
) -> Result<(), ComputerError> {
    let event = Event::mouse(source, event_type, x, y, button)?;
    event.post();
    Ok(())
}

fn post_text(source: &EventSource, character: char) -> Result<(), ComputerError> {
    let mut units = [0_u16; 2];
    let units = character.encode_utf16(&mut units);
    let key_down = Event::keyboard(source, 0, true)?;
    key_down.set_unicode(units)?;
    key_down.post();
    let key_up = Event::keyboard(source, 0, false)?;
    key_up.set_unicode(units)?;
    key_up.post();
    Ok(())
}

fn post_key(
    source: &EventSource,
    virtual_key: u16,
    key_down: bool,
    flags: u64,
) -> Result<(), ComputerError> {
    let event = Event::keyboard(source, virtual_key, key_down)?;
    event.set_flags(flags);
    event.post();
    Ok(())
}

fn post_scroll(
    source: &EventSource,
    x: f64,
    y: f64,
    delta_x: f64,
    delta_y: f64,
) -> Result<(), ComputerError> {
    validate_point(x, y)?;
    let delta_x = scroll_delta(delta_x)?;
    let delta_y = scroll_delta(delta_y)?;
    let event = Event::scroll(source, delta_x, delta_y)?;
    event.set_location(x, y)?;
    event.post();
    Ok(())
}

fn validate_point(x: f64, y: f64) -> Result<(), ComputerError> {
    if x.is_finite() && y.is_finite() {
        Ok(())
    } else {
        Err(ComputerError::InvalidCoordinate)
    }
}

fn scroll_delta(value: f64) -> Result<i32, ComputerError> {
    if !value.is_finite() || value < f64::from(i32::MIN) || value > f64::from(i32::MAX) {
        return Err(ComputerError::InvalidCoordinate);
    }
    Ok(value.round() as i32)
}

fn mouse_button(button: PointerButton) -> u32 {
    match button {
        PointerButton::Left => 0,
        PointerButton::Right => 1,
        PointerButton::Middle => 2,
    }
}

fn mouse_down_event(button: PointerButton) -> u32 {
    match button {
        PointerButton::Left => KCG_EVENT_LEFT_MOUSE_DOWN,
        PointerButton::Right => KCG_EVENT_RIGHT_MOUSE_DOWN,
        PointerButton::Middle => KCG_EVENT_OTHER_MOUSE_DOWN,
    }
}

fn mouse_up_event(button: PointerButton) -> u32 {
    match button {
        PointerButton::Left => KCG_EVENT_LEFT_MOUSE_UP,
        PointerButton::Right => KCG_EVENT_RIGHT_MOUSE_UP,
        PointerButton::Middle => KCG_EVENT_OTHER_MOUSE_UP,
    }
}

fn mouse_drag_event(button: PointerButton) -> u32 {
    match button {
        PointerButton::Left => KCG_EVENT_LEFT_MOUSE_DRAGGED,
        PointerButton::Right => KCG_EVENT_RIGHT_MOUSE_DRAGGED,
        PointerButton::Middle => KCG_EVENT_OTHER_MOUSE_DRAGGED,
    }
}

fn key_modifier_flags(modifiers: &[KeyModifier]) -> u64 {
    modifiers.iter().fold(0, |flags, modifier| {
        flags
            | match modifier {
                KeyModifier::Alt => KCG_EVENT_FLAG_MASK_ALTERNATE,
                KeyModifier::Control => KCG_EVENT_FLAG_MASK_CONTROL,
                KeyModifier::Meta => KCG_EVENT_FLAG_MASK_COMMAND,
                KeyModifier::Shift => KCG_EVENT_FLAG_MASK_SHIFT,
            }
    })
}

fn key_code(key: NamedKey) -> u16 {
    match key {
        NamedKey::ArrowDown => 125,
        NamedKey::ArrowLeft => 123,
        NamedKey::ArrowRight => 124,
        NamedKey::ArrowUp => 126,
        NamedKey::Backspace => 51,
        NamedKey::Delete => 117,
        NamedKey::End => 119,
        NamedKey::Enter => 36,
        NamedKey::Escape => 53,
        NamedKey::Home => 115,
        NamedKey::PageDown => 121,
        NamedKey::PageUp => 116,
        NamedKey::Space => 49,
        NamedKey::Tab => 48,
    }
}

fn active_displays() -> Result<Vec<CGDirectDisplayID>, ComputerError> {
    let mut count = 0;
    // SAFETY: Null output is allowed when asking CoreGraphics for the active
    // display count; `count` is a valid mutable pointer.
    let status = unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) };
    if status != KCG_ERROR_SUCCESS {
        return Err(ComputerError::Backend(format!(
            "list active displays failed with CoreGraphics status {status}"
        )));
    }
    let mut displays = vec![0; count as usize];
    // SAFETY: `displays` has room for the requested count and CoreGraphics
    // writes no more than `max_displays` identifiers.
    let status = unsafe { CGGetActiveDisplayList(count, displays.as_mut_ptr(), &mut count) };
    if status != KCG_ERROR_SUCCESS {
        return Err(ComputerError::Backend(format!(
            "read active displays failed with CoreGraphics status {status}"
        )));
    }
    displays.truncate(count as usize);
    Ok(displays)
}

fn parse_display_id(display_id: &DisplayId) -> Result<CGDirectDisplayID, ComputerError> {
    display_id
        .0
        .parse()
        .map_err(|_| ComputerError::DisplayNotFound)
}

fn display_info(display: CGDirectDisplayID) -> Result<DisplayInfo, ComputerError> {
    if !active_displays()?.contains(&display) {
        return Err(ComputerError::DisplayNotFound);
    }
    // SAFETY: `display` is an active id returned by CoreGraphics.
    let bounds = unsafe { CGDisplayBounds(display) };
    let pixel_size = display_mode_pixel_size(display)?;
    display_info_from_geometry(display, bounds, pixel_size)
}

fn display_info_from_geometry(
    display: CGDirectDisplayID,
    bounds: CGRect,
    pixel_size: PixelSize,
) -> Result<DisplayInfo, ComputerError> {
    if bounds.size.width <= 0.0
        || bounds.size.height <= 0.0
        || !bounds.size.width.is_finite()
        || !bounds.size.height.is_finite()
    {
        return Err(ComputerError::InvalidDisplayScale);
    }
    let scale_x = f64::from(pixel_size.width) / bounds.size.width;
    let scale_y = f64::from(pixel_size.height) / bounds.size.height;
    if !scale_x.is_finite() || !scale_y.is_finite() || scale_x <= 0.0 || scale_y <= 0.0 {
        return Err(ComputerError::InvalidDisplayScale);
    }
    Ok(DisplayInfo {
        display_id: DisplayId(display.to_string()),
        origin: DesktopPoint {
            x: bounds.origin.x.round() as i64,
            y: bounds.origin.y.round() as i64,
        },
        pixel_size,
        scale_x,
        scale_y,
        point_size: (bounds.size.width, bounds.size.height),
    })
}

fn display_mode_pixel_size(display: CGDirectDisplayID) -> Result<PixelSize, ComputerError> {
    // SAFETY: `display` is an active id and CoreGraphics returns a retained
    // display-mode object owned by this wrapper.
    let mode = unsafe { CGDisplayCopyDisplayMode(display) };
    if mode.is_null() {
        return Err(ComputerError::Backend(
            "CoreGraphics did not return a display mode".to_string(),
        ));
    }
    let mode = CoreFoundationObject(mode);
    // SAFETY: `mode` is the valid retained display-mode object above.
    let width = unsafe { CGDisplayModeGetPixelWidth(mode.0) };
    // SAFETY: `mode` is the valid retained display-mode object above.
    let height = unsafe { CGDisplayModeGetPixelHeight(mode.0) };
    Ok(PixelSize {
        width: u32::try_from(width)
            .map_err(|_| ComputerError::Backend("display width exceeds u32".to_string()))?,
        height: u32::try_from(height)
            .map_err(|_| ComputerError::Backend("display height exceeds u32".to_string()))?,
    })
}

fn source_region(
    request: Option<CaptureRegion>,
    display: &DisplayInfo,
) -> Result<SourceRegion, ComputerError> {
    let Some(request) = request else {
        return Ok(SourceRegion {
            x: 0,
            y: 0,
            width: display.pixel_size.width,
            height: display.pixel_size.height,
        });
    };
    let left = request.x as f64 - display.origin.x as f64;
    let top = request.y as f64 - display.origin.y as f64;
    let right = left + request.width as f64;
    let bottom = top + request.height as f64;
    if left < 0.0 || top < 0.0 || right > display.point_size.0 || bottom > display.point_size.1 {
        return Err(ComputerError::InvalidCaptureRegion);
    }
    let x = (left * display.scale_x).round();
    let y = (top * display.scale_y).round();
    let right = (right * display.scale_x).round();
    let bottom = (bottom * display.scale_y).round();
    let region = SourceRegion {
        x: x as u32,
        y: y as u32,
        width: (right - x) as u32,
        height: (bottom - y) as u32,
    };
    if region.width == 0
        || region.height == 0
        || region
            .x
            .checked_add(region.width)
            .is_none_or(|edge| edge > display.pixel_size.width)
        || region
            .y
            .checked_add(region.height)
            .is_none_or(|edge| edge > display.pixel_size.height)
    {
        return Err(ComputerError::InvalidCaptureRegion);
    }
    Ok(region)
}

fn downscale_to_limits(
    image: image::DynamicImage,
    max_width: u32,
    max_height: u32,
) -> image::DynamicImage {
    image.thumbnail(max_width.min(image.width()), max_height.min(image.height()))
}

fn check_cancelled(cancellation: &CancellationToken) -> Result<(), ComputerError> {
    if cancellation.is_cancelled() {
        Err(ComputerError::Cancelled)
    } else {
        Ok(())
    }
}

fn new_observation_id() -> Result<ObservationId, ComputerError> {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes)
        .map_err(|error| ComputerError::Backend(format!("generate observation id: {error}")))?;
    Ok(ObservationId(URL_SAFE_NO_PAD.encode(bytes)))
}

fn unix_millis() -> Result<u64, ComputerError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| ComputerError::Backend(format!("read system clock: {error}")))
        .map(|duration| duration.as_millis() as u64)
}

fn display_rect(display: &DisplayInfo) -> ObjcCGRect {
    ObjcCGRect::new(
        ObjcCGPoint::new(display.origin.x as f64, display.origin.y as f64),
        ObjcCGSize::new(display.point_size.0, display.point_size.1),
    )
}

/// Captures one display-space rectangle with the supported ScreenCaptureKit
/// API. The callback encodes while its borrowed `CGImage` is alive, so no
/// CoreFoundation object escapes the callback without a retain.
fn screen_capture_kit_png(
    rect: ObjcCGRect,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, ComputerError> {
    if !SCScreenshotManager::class().responds_to(sel!(captureImageInRect:completionHandler:)) {
        return Err(ComputerError::Backend(
            "ScreenCaptureKit display capture requires macOS 15.2 or newer".to_string(),
        ));
    }
    let (tx, rx) = mpsc::sync_channel(1);
    let callback = RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
        let result = if image.is_null() {
            let reason = if error.is_null() {
                "ScreenCaptureKit returned neither image nor error"
            } else {
                "ScreenCaptureKit did not capture the requested display"
            };
            Err(ComputerError::Backend(reason.to_string()))
        } else {
            png_bytes_from_image(image.cast())
        };
        let _ = tx.send(result);
    });
    // SAFETY: `rect` is a valid display-space rectangle and `callback` remains
    // retained until the capture has completed or this function returns.
    unsafe {
        SCScreenshotManager::captureImageInRect_completionHandler(rect, Some(&callback));
    }
    loop {
        check_cancelled(cancellation)?;
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(ComputerError::Backend(
                    "ScreenCaptureKit completion handler disconnected".to_string(),
                ));
            }
        }
    }
}

fn png_bytes_from_image(image: CGImageRef) -> Result<Vec<u8>, ComputerError> {
    let data = CoreFoundationData::new_mutable()?;
    let png_type = CoreFoundationString::new("public.png")?;
    // SAFETY: the CF data, PNG type, and callback-owned image remain valid
    // until ImageIO finishes encoding below.
    let destination =
        unsafe { CGImageDestinationCreateWithData(data.0, png_type.0, 1, std::ptr::null()) };
    if destination.is_null() {
        return Err(ComputerError::Backend(
            "create PNG image destination failed".to_string(),
        ));
    }
    let destination = CoreFoundationObject(destination.cast());
    // SAFETY: destination and image are valid CoreFoundation objects; no
    // properties are supplied.
    unsafe { CGImageDestinationAddImage(destination.0.cast_mut(), image, std::ptr::null()) };
    // SAFETY: destination is valid until the RAII wrapper drops it.
    if !unsafe { CGImageDestinationFinalize(destination.0.cast_mut()) } {
        return Err(ComputerError::Backend(
            "encode ScreenCaptureKit PNG failed".to_string(),
        ));
    }
    data.bytes()
}

struct CoreFoundationObject(*const c_void);

impl Drop for CoreFoundationObject {
    fn drop(&mut self) {
        // SAFETY: every constructor stores a retained CoreFoundation object.
        unsafe { CFRelease(self.0) };
    }
}

struct CoreFoundationData(CFMutableDataRef);

impl CoreFoundationData {
    fn new_mutable() -> Result<Self, ComputerError> {
        // SAFETY: null allocator selects the default allocator; the returned
        // retained data is owned by this wrapper.
        let data = unsafe { CFDataCreateMutable(std::ptr::null(), 0) };
        if data.is_null() {
            return Err(ComputerError::Backend(
                "allocate CoreFoundation data failed".to_string(),
            ));
        }
        Ok(Self(data))
    }

    fn bytes(&self) -> Result<Vec<u8>, ComputerError> {
        // SAFETY: `self.0` is a valid CFData object for the wrapper lifetime.
        let length = unsafe { CFDataGetLength(self.0) };
        if length < 0 {
            return Err(ComputerError::Backend(
                "CoreFoundation data has negative length".to_string(),
            ));
        }
        // SAFETY: `self.0` is valid and ImageIO has finished writing to it.
        let bytes = unsafe { CFDataGetBytePtr(self.0) };
        if bytes.is_null() && length != 0 {
            return Err(ComputerError::Backend(
                "CoreFoundation data has no bytes".to_string(),
            ));
        }
        // SAFETY: CoreFoundation owns `length` initialized bytes at `bytes`.
        Ok(unsafe { std::slice::from_raw_parts(bytes, length as usize) }.to_vec())
    }
}

impl Drop for CoreFoundationData {
    fn drop(&mut self) {
        // SAFETY: CFDataCreateMutable returned this retained object.
        unsafe { CFRelease(self.0) };
    }
}

struct CoreFoundationString(CFStringRef);

impl CoreFoundationString {
    fn new(value: &str) -> Result<Self, ComputerError> {
        let value = std::ffi::CString::new(value).map_err(|error| {
            ComputerError::Backend(format!("make CoreFoundation string: {error}"))
        })?;
        // kCFStringEncodingUTF8.
        const UTF8: u32 = 0x0800_0100;
        // SAFETY: `value` is NUL terminated and remains alive until the call
        // returns; CoreFoundation returns a retained string object.
        let string = unsafe { CFStringCreateWithCString(std::ptr::null(), value.as_ptr(), UTF8) };
        if string.is_null() {
            return Err(ComputerError::Backend(
                "allocate CoreFoundation string failed".to_string(),
            ));
        }
        Ok(Self(string))
    }
}

impl Drop for CoreFoundationString {
    fn drop(&mut self) {
        // SAFETY: CFStringCreateWithCString returned this retained object.
        unsafe { CFRelease(self.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;

    fn display() -> DisplayInfo {
        DisplayInfo {
            display_id: DisplayId("1".to_string()),
            origin: DesktopPoint { x: -1_920, y: 0 },
            pixel_size: PixelSize {
                width: 3_840,
                height: 2_160,
            },
            scale_x: 2.0,
            scale_y: 2.0,
            point_size: (1_920.0, 1_080.0),
        }
    }

    #[test]
    fn capture_region_converts_negative_display_origin_to_physical_pixels() {
        assert_eq!(
            source_region(
                Some(CaptureRegion {
                    x: -1_820,
                    y: 50,
                    width: 400,
                    height: 300,
                }),
                &display(),
            )
            .unwrap(),
            SourceRegion {
                x: 200,
                y: 100,
                width: 800,
                height: 600,
            }
        );
    }

    #[test]
    fn capture_region_rejects_rectangles_outside_display() {
        assert_eq!(
            source_region(
                Some(CaptureRegion {
                    x: -1_921,
                    y: 0,
                    width: 1,
                    height: 1,
                }),
                &display(),
            ),
            Err(ComputerError::InvalidCaptureRegion)
        );
    }

    #[test]
    fn image_limits_never_upscale_a_small_capture() {
        let image = image::DynamicImage::new_rgba8(800, 600);
        let returned = downscale_to_limits(image, 2_048, 2_048);
        assert_eq!(returned.dimensions(), (800, 600));
    }

    #[test]
    fn named_keys_and_modifiers_use_the_documented_macos_virtual_keys() {
        assert_eq!(key_code(NamedKey::Enter), 36);
        assert_eq!(key_code(NamedKey::Escape), 53);
        assert_eq!(key_code(NamedKey::ArrowLeft), 123);
        assert_eq!(
            key_modifier_flags(&[KeyModifier::Meta, KeyModifier::Shift]),
            KCG_EVENT_FLAG_MASK_COMMAND | KCG_EVENT_FLAG_MASK_SHIFT
        );
    }

    #[test]
    fn mouse_button_variants_select_matching_event_families() {
        assert_eq!(mouse_button(PointerButton::Left), 0);
        assert_eq!(mouse_button(PointerButton::Right), 1);
        assert_eq!(mouse_button(PointerButton::Middle), 2);
        assert_eq!(
            mouse_down_event(PointerButton::Middle),
            KCG_EVENT_OTHER_MOUSE_DOWN
        );
        assert_eq!(
            mouse_drag_event(PointerButton::Right),
            KCG_EVENT_RIGHT_MOUSE_DRAGGED
        );
        assert_eq!(mouse_up_event(PointerButton::Left), KCG_EVENT_LEFT_MOUSE_UP);
    }

    #[test]
    fn host_lock_state_uses_the_screen_lock_signal_not_console_presence() {
        assert_eq!(
            host_lock_state_from_session_flags(Some(true), Some(false)),
            HostLockState::Unlocked
        );
        assert_eq!(
            host_lock_state_from_session_flags(Some(true), Some(true)),
            HostLockState::Locked
        );
        assert_eq!(
            host_lock_state_from_session_flags(Some(false), Some(false)),
            HostLockState::Locked
        );
        assert_eq!(
            host_lock_state_from_session_flags(None, Some(true)),
            HostLockState::Locked
        );
        assert_eq!(
            host_lock_state_from_session_flags(Some(true), None),
            HostLockState::Unknown
        );
    }

    #[test]
    fn accessibility_prompt_options_hold_a_typed_true_value() {
        let options = accessibility_prompt_options().expect("create Accessibility prompt options");
        let key = CoreFoundationString::new("AXTrustedCheckOptionPrompt").unwrap();
        // SAFETY: `options` is a live CFDictionary and `key` is a live
        // CFString for this lookup. The stored value must be a CFBoolean.
        let value = unsafe { CFDictionaryGetValue(options.0, key.0) };
        assert!(!value.is_null());
        // SAFETY: the non-null value came from the CoreFoundation dictionary.
        assert_eq!(unsafe { CFGetTypeID(value) }, unsafe {
            CFBooleanGetTypeID()
        });
        // SAFETY: the type assertion above establishes that `value` is a CFBoolean.
        assert!(unsafe { CFBooleanGetValue(value) });
    }

    #[test]
    fn scroll_deltas_reject_non_finite_or_unrepresentable_values() {
        assert_eq!(scroll_delta(2.6), Ok(3));
        assert_eq!(scroll_delta(-2.6), Ok(-3));
        assert_eq!(
            scroll_delta(f64::NAN),
            Err(ComputerError::InvalidCoordinate)
        );
        assert_eq!(
            scroll_delta(f64::from(i32::MAX) + 1.0),
            Err(ComputerError::InvalidCoordinate)
        );
    }

    #[test]
    fn input_coordinates_must_be_finite() {
        assert_eq!(validate_point(0.0, -1.0), Ok(()));
        assert_eq!(
            validate_point(f64::INFINITY, 0.0),
            Err(ComputerError::InvalidCoordinate)
        );
    }

    #[test]
    fn main_display_uses_display_mode_pixel_dimensions_without_screen_recording_permission() {
        let display = main_display_id();
        let mode_pixels = match display_mode_pixel_size(display) {
            Ok(mode_pixels) => mode_pixels,
            // A headless macOS test runner can have a main display ID while
            // CoreGraphics exposes no display mode. There is no geometry to
            // compare in that environment; interactive Macs still exercise
            // the assertions below without Screen Recording permission.
            Err(ComputerError::Backend(message))
                if message == "CoreGraphics did not return a display mode" =>
            {
                eprintln!("skipping display-mode geometry check: no CoreGraphics display mode");
                return;
            }
            Err(error) => panic!("read main display mode: {error}"),
        };
        // SAFETY: the main display ID is valid for CoreGraphics geometry
        // queries; unlike display capture, this does not require TCC consent.
        let bounds = unsafe { CGDisplayBounds(display) };
        let info = display_info_from_geometry(display, bounds, mode_pixels)
            .expect("construct main display geometry");

        assert_eq!(info.pixel_size, mode_pixels);
        assert_eq!(
            info.scale_x,
            f64::from(mode_pixels.width) / info.point_size.0
        );
        assert_eq!(
            info.scale_y,
            f64::from(mode_pixels.height) / info.point_size.1
        );
    }

    #[tokio::test]
    async fn cancelled_capture_stops_before_permission_or_native_capture() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = MacosComputerBackend::default()
            .observe(
                ObserveArgs {
                    display_id: None,
                    region: None,
                    max_image_width: None,
                    max_image_height: None,
                },
                cancellation,
            )
            .await
            .expect_err("cancelled capture must not reach CoreGraphics");
        assert_eq!(error, ComputerError::Cancelled);
    }

    #[tokio::test]
    #[ignore = "requires Screen Recording permission for the process running cargo test"]
    async fn native_capture_matches_declared_display_geometry() {
        let backend = MacosComputerBackend::default();
        let observation = backend
            .observe(
                ObserveArgs {
                    display_id: None,
                    region: None,
                    max_image_width: Some(640),
                    max_image_height: Some(640),
                },
                CancellationToken::new(),
            )
            .await
            .expect("Screen Recording permission is required for this native test");
        let bytes = STANDARD
            .decode(&observation.image.data_base64)
            .expect("observation image is base64");
        let image = image::load_from_memory_with_format(&bytes, ImageFormat::Png)
            .expect("observation image is PNG");
        assert_eq!(
            image.dimensions(),
            (
                observation.metadata.returned_image_size.width,
                observation.metadata.returned_image_size.height
            )
        );
        assert!(image.width() <= 640);
        assert!(image.height() <= 640);

        let current = backend
            .current_display(&observation.metadata.display_id)
            .expect("captured display remains present");
        assert_eq!(current.display_id, observation.metadata.display_id);
        assert_eq!(current.pixel_size, observation.metadata.display_pixel_size);
        assert_eq!(current.origin, observation.metadata.display_origin);
        assert_eq!(
            current.scale_x.to_bits(),
            observation.metadata.display_scale_x.to_bits()
        );
        assert_eq!(
            current.scale_y.to_bits(),
            observation.metadata.display_scale_y.to_bits()
        );
    }
}
