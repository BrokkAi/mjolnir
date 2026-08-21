//! Platform-neutral contract for opt-in computer interaction.
//!
//! This module deliberately contains no MCP listener, product policy, or OS
//! APIs.  It is the shared boundary for those later layers: model-facing tool
//! arguments, bounded observations, coordinate transforms, and the native
//! backend interface.

use std::{error::Error, fmt};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Hard limits enforced by the future MCP service, not selected by an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLimits {
    pub max_width: u32,
    pub max_height: u32,
    pub max_encoded_bytes: usize,
}

impl ImageLimits {
    pub const DEFAULT: Self = Self {
        max_width: 2_048,
        max_height: 2_048,
        max_encoded_bytes: 4 * 1024 * 1024,
    };

    pub fn validate_requested(
        self,
        requested_width: Option<u32>,
        requested_height: Option<u32>,
    ) -> Result<(), ComputerError> {
        if requested_width.is_some_and(|width| width == 0 || width > self.max_width) {
            return Err(ComputerError::ImageLimitExceeded {
                field: "max_image_width",
            });
        }
        if requested_height.is_some_and(|height| height == 0 || height > self.max_height) {
            return Err(ComputerError::ImageLimitExceeded {
                field: "max_image_height",
            });
        }
        Ok(())
    }

    pub fn validate_image(
        self,
        size: PixelSize,
        encoded_bytes: usize,
    ) -> Result<(), ComputerError> {
        if size.width == 0 || size.width > self.max_width {
            return Err(ComputerError::ImageLimitExceeded {
                field: "image width",
            });
        }
        if size.height == 0 || size.height > self.max_height {
            return Err(ComputerError::ImageLimitExceeded {
                field: "image height",
            });
        }
        if encoded_bytes > self.max_encoded_bytes {
            return Err(ComputerError::ImageLimitExceeded {
                field: "encoded image bytes",
            });
        }
        Ok(())
    }
}

/// A stable platform display identifier. It is opaque to models.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct DisplayId(pub String);

/// An opaque observation token issued by the service.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ObservationId(pub String);

/// A location in the desktop coordinate space. Negative origins are valid for
/// displays positioned left of or above the primary display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DesktopPoint {
    pub x: i64,
    pub y: i64,
}

/// A location in the returned observation image's pixel space.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ImagePoint {
    pub x: f64,
    pub y: f64,
}

/// Pixel dimensions for a source capture or returned image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PixelSize {
    pub width: u32,
    pub height: u32,
}

/// A requested desktop capture area. When absent, the selected display is
/// captured. Coordinates are desktop points, not returned-image pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CaptureRegion {
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
}

impl CaptureRegion {
    fn validate(self) -> Result<(), ComputerError> {
        if self.width == 0 || self.height == 0 {
            return Err(ComputerError::InvalidCaptureRegion);
        }
        Ok(())
    }
}

/// The physical-pixel crop inside the source display capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SourceRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The display geometry currently reported by a backend. A later policy layer
/// compares it to an observation before allowing an input action.
#[derive(Debug, Clone, PartialEq)]
pub struct CurrentDisplay {
    pub display_id: DisplayId,
    pub origin: DesktopPoint,
    pub pixel_size: PixelSize,
    pub scale_x: f64,
    pub scale_y: f64,
}

/// Geometry reported with every observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ObservationMetadata {
    pub observation_id: ObservationId,
    pub display_id: DisplayId,
    /// Top-left of the display in desktop coordinate space.
    pub display_origin: DesktopPoint,
    /// Full physical-pixel dimensions of the display at capture time.
    pub display_pixel_size: PixelSize,
    /// Physical pixels per desktop point on each axis. The values can differ
    /// slightly in a scaled display mode because its point dimensions round.
    pub display_scale_x: f64,
    pub display_scale_y: f64,
    /// Physical-pixel crop taken from the display before any downscaling.
    pub source_region: SourceRegion,
    /// Dimensions of the image supplied to the agent after downscaling.
    pub returned_image_size: PixelSize,
    pub mime_type: String,
    /// Unix milliseconds when this observation was captured.
    pub created_at_unix_ms: u64,
    /// Unix milliseconds after which targeting this observation is rejected.
    pub expires_at_unix_ms: u64,
}

impl ObservationMetadata {
    pub fn validate(&self, limits: ImageLimits) -> Result<(), ComputerError> {
        if !self.display_scale_x.is_finite()
            || !self.display_scale_y.is_finite()
            || self.display_scale_x <= 0.0
            || self.display_scale_y <= 0.0
        {
            return Err(ComputerError::InvalidDisplayScale);
        }
        if self.expires_at_unix_ms <= self.created_at_unix_ms {
            return Err(ComputerError::InvalidObservationExpiry);
        }
        let max_x = self
            .source_region
            .x
            .checked_add(self.source_region.width)
            .ok_or(ComputerError::InvalidSourceRegion)?;
        let max_y = self
            .source_region
            .y
            .checked_add(self.source_region.height)
            .ok_or(ComputerError::InvalidSourceRegion)?;
        if self.source_region.width == 0
            || self.source_region.height == 0
            || max_x > self.display_pixel_size.width
            || max_y > self.display_pixel_size.height
        {
            return Err(ComputerError::InvalidSourceRegion);
        }
        limits.validate_image(self.returned_image_size, 0)
    }

    /// Converts a point from the returned image to a desktop coordinate.
    /// Callers must retain the fractional result until the platform backend
    /// applies its documented rounding rule.
    pub fn map_image_point(&self, point: ImagePoint) -> Result<(f64, f64), ComputerError> {
        if !point.x.is_finite()
            || !point.y.is_finite()
            || point.x < 0.0
            || point.y < 0.0
            || point.x >= f64::from(self.returned_image_size.width)
            || point.y >= f64::from(self.returned_image_size.height)
        {
            return Err(ComputerError::InvalidCoordinate);
        }

        let source_x = f64::from(self.source_region.x)
            + point.x * f64::from(self.source_region.width)
                / f64::from(self.returned_image_size.width);
        let source_y = f64::from(self.source_region.y)
            + point.y * f64::from(self.source_region.height)
                / f64::from(self.returned_image_size.height);
        Ok((
            self.display_origin.x as f64 + source_x / self.display_scale_x,
            self.display_origin.y as f64 + source_y / self.display_scale_y,
        ))
    }

    /// Rejects a stale or invalidated observation before mapping an image point
    /// to an input coordinate. This belongs above the platform backend, so no
    /// backend can accidentally emit input for a changed display.
    pub fn resolve_target(
        &self,
        point: ImagePoint,
        now_unix_ms: u64,
        current_display: &CurrentDisplay,
    ) -> Result<(f64, f64), ComputerError> {
        if now_unix_ms >= self.expires_at_unix_ms {
            return Err(ComputerError::ObservationExpired);
        }
        if current_display.display_id != self.display_id
            || current_display.origin != self.display_origin
            || current_display.pixel_size != self.display_pixel_size
            || current_display.scale_x.to_bits() != self.display_scale_x.to_bits()
            || current_display.scale_y.to_bits() != self.display_scale_y.to_bits()
        {
            return Err(ComputerError::DisplayChanged);
        }
        self.map_image_point(point)
    }
}

/// Bounded image payload returned with an observation. The data is base64 so
/// it remains JSON/MCP transport-safe; handlers can turn it into MCP image
/// content without changing the observation contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EncodedImage {
    pub data_base64: String,
}

impl EncodedImage {
    pub fn from_bytes(bytes: &[u8], limits: ImageLimits) -> Result<Self, ComputerError> {
        let data_base64 = STANDARD.encode(bytes);
        if data_base64.len() > limits.max_encoded_bytes {
            return Err(ComputerError::ImageLimitExceeded {
                field: "encoded image bytes",
            });
        }
        Ok(Self { data_base64 })
    }
}

/// Result of `observe` before the MCP transport renders the image content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Observation {
    pub metadata: ObservationMetadata,
    pub image: EncodedImage,
}

impl Observation {
    pub fn validate(&self, limits: ImageLimits) -> Result<(), ComputerError> {
        self.metadata.validate(limits)?;
        limits.validate_image(
            self.metadata.returned_image_size,
            self.image.data_base64.len(),
        )
    }
}

/// Model-facing arguments for an observation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObserveArgs {
    /// Omit to select the host's default display.
    pub display_id: Option<DisplayId>,
    /// Omit to capture the selected display.
    pub region: Option<CaptureRegion>,
    /// A preferred bound; the service rejects values above its hard cap.
    pub max_image_width: Option<u32>,
    /// A preferred bound; the service rejects values above its hard cap.
    pub max_image_height: Option<u32>,
}

impl ObserveArgs {
    pub fn validate(&self, limits: ImageLimits) -> Result<(), ComputerError> {
        if self
            .display_id
            .as_ref()
            .is_some_and(|id| id.0.trim().is_empty())
        {
            return Err(ComputerError::InvalidDisplayId);
        }
        if let Some(region) = self.region {
            region.validate()?;
        }
        limits.validate_requested(self.max_image_width, self.max_image_height)
    }
}

/// Input actions are always tied to the exact observation used for targeting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetedPointArgs {
    pub observation_id: ObservationId,
    pub point: ImagePoint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClickArgs {
    #[serde(flatten)]
    pub target: TargetedPointArgs,
    #[serde(default)]
    pub button: PointerButton,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DoubleClickArgs {
    #[serde(flatten)]
    pub target: TargetedPointArgs,
    #[serde(default)]
    pub button: PointerButton,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoveArgs {
    #[serde(flatten)]
    pub target: TargetedPointArgs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DragArgs {
    pub observation_id: ObservationId,
    pub from: ImagePoint,
    pub to: ImagePoint,
    #[serde(default)]
    pub button: PointerButton,
}

/// Text is intentionally separate from key input: it represents literal text,
/// never a platform shortcut or a sequence of key names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TypeTextArgs {
    pub observation_id: ObservationId,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyArgs {
    pub observation_id: ObservationId,
    pub key: NamedKey,
    #[serde(default)]
    pub modifiers: Vec<KeyModifier>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScrollArgs {
    pub observation_id: ObservationId,
    pub point: ImagePoint,
    pub delta_x: f64,
    pub delta_y: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitArgs {
    pub observation_id: ObservationId,
    /// Capped by the service before it waits.
    pub milliseconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum PointerButton {
    #[default]
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeyModifier {
    Alt,
    Control,
    Meta,
    Shift,
}

/// Deliberately named keys only. Printable input belongs in `type_text`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NamedKey {
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    Backspace,
    Delete,
    End,
    Enter,
    Escape,
    Home,
    PageDown,
    PageUp,
    Space,
    Tab,
}

/// Native input after service-side observation validation and coordinate
/// transformation. Policy and MCP transport remain outside the backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BackendAction {
    Move {
        x: f64,
        y: f64,
    },
    Click {
        x: f64,
        y: f64,
        button: PointerButton,
    },
    DoubleClick {
        x: f64,
        y: f64,
        button: PointerButton,
    },
    Drag {
        from: (f64, f64),
        to: (f64, f64),
        button: PointerButton,
    },
    TypeText {
        text: String,
    },
    Key {
        key: NamedKey,
        modifiers: Vec<KeyModifier>,
    },
    Scroll {
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    Granted,
    /// The OS did not grant the permission; CoreGraphics cannot distinguish a
    /// first-run prompt from a prior denial without showing a system prompt.
    NotGranted,
    Denied,
    NotDetermined,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionReadiness {
    pub screen_recording: PermissionState,
    pub accessibility: PermissionState,
}

/// A permission prompt owned by the dedicated platform host. The terminal and
/// MCP service may request this through the authenticated host channel, but
/// never call the OS permission API themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerPermission {
    ScreenRecording,
    Accessibility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostLockState {
    Unlocked,
    Locked,
    /// Backends must use this instead of guessing; the policy layer fails
    /// closed before it emits input.
    Unknown,
}

/// OS adapter boundary. Platform implementations receive only portable types
/// and a cancellation token for capture and multi-event input operations.
#[async_trait]
pub trait ComputerBackend: Send + Sync {
    async fn observe(
        &self,
        request: ObserveArgs,
        cancellation: CancellationToken,
    ) -> Result<Observation, ComputerError>;

    async fn permission_readiness(
        &self,
        cancellation: CancellationToken,
    ) -> Result<PermissionReadiness, ComputerError>;

    async fn request_permission(
        &self,
        _permission: ComputerPermission,
        cancellation: CancellationToken,
    ) -> Result<PermissionReadiness, ComputerError> {
        if cancellation.is_cancelled() {
            return Err(ComputerError::Cancelled);
        }
        Err(ComputerError::Backend(
            "this computer backend cannot request OS permission".to_string(),
        ))
    }

    async fn host_lock_state(
        &self,
        cancellation: CancellationToken,
    ) -> Result<HostLockState, ComputerError>;

    async fn execute(
        &self,
        action: BackendAction,
        cancellation: CancellationToken,
    ) -> Result<(), ComputerError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputerError {
    ImageLimitExceeded { field: &'static str },
    InvalidCaptureRegion,
    InvalidCoordinate,
    InvalidDisplayId,
    InvalidDisplayScale,
    InvalidObservationExpiry,
    InvalidSourceRegion,
    DisplayNotFound,
    ObservationExpired,
    DisplayChanged,
    ObservationNotFound,
    ScreenRecordingPermission(PermissionState),
    AccessibilityPermission(PermissionState),
    HostLocked,
    HostLockStateUnknown,
    Cancelled,
    Backend(String),
}

impl fmt::Display for ComputerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ImageLimitExceeded { field } => {
                write!(f, "computer image limit exceeded: {field}")
            }
            Self::InvalidCaptureRegion => f.write_str("computer capture region is invalid"),
            Self::InvalidCoordinate => {
                f.write_str("computer coordinate is outside the observation")
            }
            Self::InvalidDisplayId => f.write_str("computer display id is empty"),
            Self::InvalidDisplayScale => f.write_str("computer display scale is invalid"),
            Self::InvalidObservationExpiry => f.write_str("computer observation expiry is invalid"),
            Self::InvalidSourceRegion => f.write_str("computer source region is invalid"),
            Self::DisplayNotFound => f.write_str("computer display was not found"),
            Self::ObservationExpired => f.write_str("computer observation has expired"),
            Self::DisplayChanged => f.write_str("computer display changed since observation"),
            Self::ObservationNotFound => f.write_str("computer observation was not found"),
            Self::ScreenRecordingPermission(state) => {
                write!(f, "screen recording permission is not ready: {state:?}")
            }
            Self::AccessibilityPermission(state) => {
                write!(f, "accessibility permission is not ready: {state:?}")
            }
            Self::HostLocked => f.write_str("computer input is blocked while the host is locked"),
            Self::HostLockStateUnknown => {
                f.write_str("computer input is blocked because host lock state is unknown")
            }
            Self::Cancelled => f.write_str("computer operation was cancelled"),
            Self::Backend(message) => write!(f, "computer backend error: {message}"),
        }
    }
}

impl Error for ComputerError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> ObservationMetadata {
        ObservationMetadata {
            observation_id: ObservationId("observation-1".to_string()),
            display_id: DisplayId("display-1".to_string()),
            display_origin: DesktopPoint { x: -1_920, y: 0 },
            display_pixel_size: PixelSize {
                width: 3_840,
                height: 2_160,
            },
            display_scale_x: 2.0,
            display_scale_y: 2.0,
            source_region: SourceRegion {
                x: 640,
                y: 360,
                width: 1_920,
                height: 1_080,
            },
            returned_image_size: PixelSize {
                width: 960,
                height: 540,
            },
            mime_type: "image/png".to_string(),
            created_at_unix_ms: 100,
            expires_at_unix_ms: 200,
        }
    }

    #[test]
    fn maps_downscaled_retina_crop_on_negative_origin_display() {
        let point = metadata()
            .map_image_point(ImagePoint { x: 480.0, y: 270.0 })
            .expect("valid point");
        assert_eq!(point, (-1_120.0, 450.0));
    }

    #[test]
    fn rejects_non_finite_or_out_of_bounds_coordinates() {
        let metadata = metadata();
        for point in [
            ImagePoint { x: -0.1, y: 0.0 },
            ImagePoint { x: 960.0, y: 0.0 },
            ImagePoint {
                x: f64::NAN,
                y: 0.0,
            },
            ImagePoint {
                x: 0.0,
                y: f64::INFINITY,
            },
        ] {
            assert_eq!(
                metadata.map_image_point(point),
                Err(ComputerError::InvalidCoordinate)
            );
        }
    }

    #[test]
    fn resolve_target_distinguishes_expired_and_changed_displays_before_mapping() {
        let metadata = metadata();
        let current = CurrentDisplay {
            display_id: metadata.display_id.clone(),
            origin: metadata.display_origin,
            pixel_size: metadata.display_pixel_size,
            scale_x: metadata.display_scale_x,
            scale_y: metadata.display_scale_y,
        };
        assert_eq!(
            metadata.resolve_target(ImagePoint { x: 0.0, y: 0.0 }, 200, &current),
            Err(ComputerError::ObservationExpired)
        );

        let changed = CurrentDisplay {
            pixel_size: PixelSize {
                width: 3_000,
                height: 2_160,
            },
            ..current
        };
        assert_eq!(
            metadata.resolve_target(ImagePoint { x: 0.0, y: 0.0 }, 150, &changed),
            Err(ComputerError::DisplayChanged)
        );
    }

    #[test]
    fn observation_validation_rejects_invalid_transform_and_expiry() {
        let mut invalid = metadata();
        invalid.source_region.width = 0;
        assert_eq!(
            invalid.validate(ImageLimits::DEFAULT),
            Err(ComputerError::InvalidSourceRegion)
        );

        let mut invalid = metadata();
        invalid.expires_at_unix_ms = invalid.created_at_unix_ms;
        assert_eq!(
            invalid.validate(ImageLimits::DEFAULT),
            Err(ComputerError::InvalidObservationExpiry)
        );
    }

    #[test]
    fn image_limits_bound_agent_requested_and_returned_images() {
        let limits = ImageLimits {
            max_width: 100,
            max_height: 50,
            max_encoded_bytes: 8,
        };
        assert_eq!(
            limits.validate_requested(Some(101), None),
            Err(ComputerError::ImageLimitExceeded {
                field: "max_image_width"
            })
        );
        assert_eq!(
            limits.validate_image(
                PixelSize {
                    width: 100,
                    height: 51
                },
                0
            ),
            Err(ComputerError::ImageLimitExceeded {
                field: "image height"
            })
        );
        assert_eq!(
            EncodedImage::from_bytes(b"1234567", limits),
            Err(ComputerError::ImageLimitExceeded {
                field: "encoded image bytes"
            })
        );
    }

    #[test]
    fn tool_arguments_round_trip_and_generate_json_schema() {
        let args = TypeTextArgs {
            observation_id: ObservationId("observation-1".to_string()),
            text: "literal: ⌘Q".to_string(),
        };
        let json = serde_json::to_value(&args).expect("serialize tool args");
        assert_eq!(
            serde_json::from_value::<TypeTextArgs>(json).expect("deserialize tool args"),
            args
        );
        let schema = schemars::schema_for!(DragArgs);
        let schema = serde_json::to_value(schema).expect("serialize tool schema");
        assert!(schema["properties"].get("observation_id").is_some());
        assert!(schema["properties"].get("from").is_some());
    }

    #[test]
    fn literal_text_and_named_keys_are_separate_schemas() {
        let text = serde_json::to_value(schemars::schema_for!(TypeTextArgs)).unwrap();
        let key = serde_json::to_value(schemars::schema_for!(KeyArgs)).unwrap();
        assert!(text["properties"].get("text").is_some());
        assert!(key["properties"].get("key").is_some());
        assert!(key["properties"].get("text").is_none());
    }
}
