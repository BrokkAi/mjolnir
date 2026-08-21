//! macOS observation backend for the computer-interaction contract.
//!
//! CoreGraphics provides display geometry and the current display image;
//! ImageIO encodes that image as PNG. This module intentionally does not
//! request Screen Recording access, inject input, start an MCP listener, or
//! make policy decisions.

use std::{
    ffi::{c_char, c_void},
    io::Cursor,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use image::{GenericImageView as _, ImageFormat};
use tokio_util::sync::CancellationToken;

use crate::computer::{
    BackendAction, CaptureRegion, ComputerBackend, ComputerError, CurrentDisplay, DesktopPoint,
    DisplayId, EncodedImage, HostLockState, ImageLimits, Observation, ObservationId,
    ObservationMetadata, ObserveArgs, PermissionReadiness, PermissionState, PixelSize,
    SourceRegion,
};

type CGDirectDisplayID = u32;
type CGError = i32;
type CGImageRef = *const c_void;
type CFDataRef = *const c_void;
type CFMutableDataRef = *mut c_void;
type CFStringRef = *const c_void;
type CGImageDestinationRef = *mut c_void;

const KCG_ERROR_SUCCESS: CGError = 0;

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
    fn CGDisplayPixelsWide(display: CGDirectDisplayID) -> usize;
    fn CGDisplayPixelsHigh(display: CGDirectDisplayID) -> usize;
    fn CGDisplayCreateImage(display: CGDirectDisplayID) -> CGImageRef;
    fn CGPreflightScreenCaptureAccess() -> bool;
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

        let image = CoreGraphicsImage::capture(display)?;
        check_cancelled(cancellation)?;
        let png = image.png_bytes()?;
        check_cancelled(cancellation)?;
        let full = image::load_from_memory_with_format(&png, ImageFormat::Png)
            .map_err(|error| ComputerError::Backend(format!("decode CoreGraphics PNG: {error}")))?;
        if full.dimensions() != (info.pixel_size.width, info.pixel_size.height) {
            return Err(ComputerError::Backend(format!(
                "CoreGraphics returned {}x{} pixels for display declared as {}x{}",
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
        let returned = cropped.thumbnail(max_width, max_height);
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
                display_scale: info.scale,
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
            accessibility: PermissionState::Unsupported,
        })
    }

    async fn host_lock_state(
        &self,
        cancellation: CancellationToken,
    ) -> Result<HostLockState, ComputerError> {
        check_cancelled(&cancellation)?;
        // This observation-only backend does not infer host lock state. The
        // later input policy treats Unknown as a hard denial.
        Ok(HostLockState::Unknown)
    }

    async fn execute(
        &self,
        _action: BackendAction,
        cancellation: CancellationToken,
    ) -> Result<(), ComputerError> {
        check_cancelled(&cancellation)?;
        Err(ComputerError::Backend(
            "macOS observation backend does not implement input".to_string(),
        ))
    }
}

#[derive(Debug, Clone)]
struct DisplayInfo {
    display_id: DisplayId,
    origin: DesktopPoint,
    pixel_size: PixelSize,
    scale: f64,
    point_size: (f64, f64),
}

impl DisplayInfo {
    fn current_display(&self) -> CurrentDisplay {
        CurrentDisplay {
            display_id: self.display_id.clone(),
            origin: self.origin,
            pixel_size: self.pixel_size,
            scale: self.scale,
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
    // SAFETY: `display` is an active id returned by CoreGraphics.
    let width = unsafe { CGDisplayPixelsWide(display) };
    // SAFETY: `display` is an active id returned by CoreGraphics.
    let height = unsafe { CGDisplayPixelsHigh(display) };
    let pixel_size = PixelSize {
        width: u32::try_from(width)
            .map_err(|_| ComputerError::Backend("display width exceeds u32".to_string()))?,
        height: u32::try_from(height)
            .map_err(|_| ComputerError::Backend("display height exceeds u32".to_string()))?,
    };
    if bounds.size.width <= 0.0
        || bounds.size.height <= 0.0
        || !bounds.size.width.is_finite()
        || !bounds.size.height.is_finite()
    {
        return Err(ComputerError::InvalidDisplayScale);
    }
    let scale_x = f64::from(pixel_size.width) / bounds.size.width;
    let scale_y = f64::from(pixel_size.height) / bounds.size.height;
    if !scale_x.is_finite() || !scale_y.is_finite() || (scale_x - scale_y).abs() > f64::EPSILON {
        return Err(ComputerError::InvalidDisplayScale);
    }
    Ok(DisplayInfo {
        display_id: DisplayId(display.to_string()),
        origin: DesktopPoint {
            x: bounds.origin.x.round() as i64,
            y: bounds.origin.y.round() as i64,
        },
        pixel_size,
        scale: scale_x,
        point_size: (bounds.size.width, bounds.size.height),
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
    let x = (left * display.scale).round();
    let y = (top * display.scale).round();
    let right = (right * display.scale).round();
    let bottom = (bottom * display.scale).round();
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

struct CoreGraphicsImage(CGImageRef);

impl CoreGraphicsImage {
    fn capture(display: CGDirectDisplayID) -> Result<Self, ComputerError> {
        // SAFETY: `display` is active and CoreGraphics returns a retained image
        // reference owned by this wrapper.
        let image = unsafe { CGDisplayCreateImage(display) };
        if image.is_null() {
            return Err(ComputerError::Backend(
                "CoreGraphics did not return a display image".to_string(),
            ));
        }
        Ok(Self(image))
    }

    fn png_bytes(&self) -> Result<Vec<u8>, ComputerError> {
        let data = CoreFoundationData::new_mutable()?;
        let png_type = CoreFoundationString::new("public.png")?;
        // SAFETY: the CF data and PNG type remain valid for the destination's
        // lifetime; ImageIO retains neither after finalize/release.
        let destination =
            unsafe { CGImageDestinationCreateWithData(data.0, png_type.0, 1, std::ptr::null()) };
        if destination.is_null() {
            return Err(ComputerError::Backend(
                "create PNG image destination failed".to_string(),
            ));
        }
        let destination = CoreFoundationObject(destination.cast());
        // SAFETY: destination and image are valid retained CoreFoundation
        // objects; no properties are supplied.
        unsafe { CGImageDestinationAddImage(destination.0.cast_mut(), self.0, std::ptr::null()) };
        // SAFETY: destination is valid until the RAII wrapper drops it.
        if !unsafe { CGImageDestinationFinalize(destination.0.cast_mut()) } {
            return Err(ComputerError::Backend(
                "encode CoreGraphics PNG failed".to_string(),
            ));
        }
        data.bytes()
    }
}

impl Drop for CoreGraphicsImage {
    fn drop(&mut self) {
        // SAFETY: CGDisplayCreateImage returned this retained CoreFoundation
        // object and this wrapper is its sole owner.
        unsafe { CFRelease(self.0) };
    }
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
            scale: 2.0,
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
            current.scale.to_bits(),
            observation.metadata.display_scale.to_bits()
        );
    }
}
