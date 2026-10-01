//! Editable container sizing and the EC2 instance selector.
use super::*;
use mj_chat::text_input::TextInput;

const GIB: u64 = 1 << 30;

#[derive(Debug, Clone, Copy)]
pub(crate) enum ResourcePicker<'a> {
    Container(&'a ResourceEditor),
    Ec2 {
        editor: &'a ResourceEditor,
        options: &'a [SessionResourceAllocation],
        selected: usize,
        has_selection: bool,
        loading: bool,
    },
}

pub(crate) fn target_resources<'a, W: WizardDraft>(
    dashboard: &DashboardState,
    wizard: &'a W,
) -> Option<ResourcePicker<'a>> {
    if wizard.step() != WizardStep::Target {
        return None;
    }
    let id = nth_key(&dashboard.config.targets, wizard.target());
    if wizard.target_rejection(dashboard, &id).is_some() {
        return None;
    }
    let target = &dashboard.config.targets[&id];
    if mj_core::config::is_container_target(target) {
        Some(ResourcePicker::Container(wizard.resource_editor()))
    } else if matches!(target, TargetTemplate::AwsEc2 { .. }) {
        let options = wizard.aws_options().get(&id).map_or(&[][..], Vec::as_slice);
        let selected = options
            .iter()
            .position(|option| Some(option) == wizard.resource_allocation())
            .unwrap_or(0);
        Some(ResourcePicker::Ec2 {
            editor: wizard.resource_editor(),
            options,
            selected,
            has_selection: wizard.resource_allocation().is_some(),
            loading: !wizard.aws_options().contains_key(&id) && wizard.sizing_error().is_none(),
        })
    } else {
        None
    }
}

pub(crate) fn declare_resource_controls(
    form: &mut Dialog<WizardControl>,
    resources: Option<ResourcePicker<'_>>,
) {
    match resources {
        Some(ResourcePicker::Container(_)) => {
            form.declare_with_enabled(WizardControl::ResourceCpu, ControlKind::TextField, true);
            form.declare_with_enabled(WizardControl::ResourceMemory, ControlKind::TextField, true);
        }
        Some(ResourcePicker::Ec2 {
            editor,
            options,
            selected,
            ..
        }) => {
            form.declare_with_enabled(
                WizardControl::ResourceInstance,
                ControlKind::ComboBox {
                    len: options.len(),
                    selected: editor
                        .instances
                        .selection(WizardControl::ResourceInstance, selected),
                    expanded: editor.instances.is_open(WizardControl::ResourceInstance),
                },
                !options.is_empty(),
            );
        }
        None => {}
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResourceEditor {
    pub(crate) target_id: Option<String>,
    pub(crate) cpu: TextInput,
    pub(crate) memory: TextInput,
    pub(crate) instances: ComboBoxState<WizardControl>,
    pub(crate) instance_type: Option<String>,
}

impl ResourceEditor {
    pub(crate) fn draft_values(&self) -> Vec<String> {
        vec![
            self.cpu.to_string(),
            self.memory.to_string(),
            self.instance_type.clone().unwrap_or_default(),
        ]
    }

    pub(crate) fn reset(&mut self, allocation: Option<&SessionResourceAllocation>) {
        self.instances = ComboBoxState::default();
        self.instance_type = match allocation {
            Some(SessionResourceAllocation::AwsEc2 { instance_type, .. }) => {
                Some(instance_type.clone())
            }
            _ => None,
        };
        if let Some(SessionResourceAllocation::Container { cpus, memory_bytes }) = allocation {
            self.cpu = cpus.to_string().into();
            self.memory = memory_gib_text(*memory_bytes).into();
        } else {
            self.cpu.clear();
            self.memory.clear();
        }
    }

    pub(crate) fn allocation(
        &self,
        limits: Option<(u64, u64)>,
    ) -> Result<SessionResourceAllocation, String> {
        let cpu = self.cpu.trim();
        let cpus = cpu
            .parse::<u64>()
            .ok()
            .filter(|cpus| *cpus > 0 && cpu.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or_else(|| "CPU must be a positive whole number.".to_owned())?;
        let memory_bytes = parse_memory_gib(self.memory.trim())
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| {
                "MEM must be a positive number of GiB (up to 30 decimal places).".to_owned()
            })?;
        if let Some((max_cpus, max_memory)) = limits {
            if cpus > max_cpus {
                return Err(format!("CPU exceeds this host's {max_cpus} CPUs."));
            }
            if memory_bytes > max_memory {
                return Err(format!(
                    "MEM exceeds this host's {} GiB.",
                    host_limit_gib_text(max_memory)
                ));
            }
        }
        Ok(SessionResourceAllocation::Container { cpus, memory_bytes })
    }
}

/// A host limit for a message: rounded down to one decimal, so the number
/// shown is itself accepted when typed back.
fn host_limit_gib_text(bytes: u64) -> String {
    let tenths = (u128::from(bytes) * 10 / u128::from(GIB)) as u64;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// Exact decimal GiB: even a host limit ending in a partial GiB round-trips.
pub(crate) fn memory_gib_text(bytes: u64) -> String {
    let mut text = (bytes / GIB).to_string();
    let mut remainder = bytes % GIB;
    if remainder != 0 {
        text.push('.');
        while remainder != 0 {
            remainder *= 10;
            text.push(char::from(b'0' + (remainder / GIB) as u8));
            remainder %= GIB;
        }
    }
    text
}

fn parse_memory_gib(text: &str) -> Option<u64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if (whole.is_empty() && fraction.is_empty())
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 30
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let whole = if whole.is_empty() { "0" } else { whole };
    let whole = whole.parse::<u64>().ok()?.checked_mul(GIB)?;
    if fraction.is_empty() {
        return Some(whole);
    }
    // Cancel powers of two before multiplication to avoid overflow for
    // the exact 30-place decimal representation of a single byte.
    let digits = fraction.len() as u32;
    let denominator = 5_u128.pow(digits);
    let numerator = fraction.parse::<u128>().ok()? * (1_u128 << (30 - digits));
    let bytes = (numerator + denominator / 2) / denominator;
    whole.checked_add(u64::try_from(bytes).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_memory_round_trips_bytes_and_checks_invalid_sizes() {
        for bytes in [1, GIB - 1, GIB, 32 * GIB, 48 * GIB + 123456, u64::MAX] {
            assert_eq!(parse_memory_gib(&memory_gib_text(bytes)), Some(bytes));
        }
        assert_eq!(parse_memory_gib("0.1"), Some(107374182));
        assert_eq!(parse_memory_gib("1.5"), Some(GIB + GIB / 2));
        assert_eq!(parse_memory_gib(".5"), Some(GIB / 2));
        for text in ["", "-1", "NaN", "1GiB", "1.2.3", "18446744073709551615"] {
            assert_eq!(parse_memory_gib(text), None, "{text}");
        }
    }

    #[test]
    fn host_memory_limit_message_is_readable() {
        let editor = ResourceEditor {
            cpu: "1".into(),
            memory: "999".into(),
            ..ResourceEditor::default()
        };
        let limit = 98 * GIB + GIB / 5 + 12345;
        let message = editor.allocation(Some((8, limit))).unwrap_err();
        assert_eq!(message, "MEM exceeds this host's 98.2 GiB.");
    }

    #[test]
    fn container_fields_validate_independently_without_clamping() {
        let mut editor = ResourceEditor::default();
        editor.reset(Some(&SessionResourceAllocation::Container {
            cpus: 8,
            memory_bytes: 32 * GIB,
        }));
        editor.cpu = "3".into();
        editor.memory = "1.5".into();
        assert_eq!(
            editor.allocation(None),
            Ok(SessionResourceAllocation::Container {
                cpus: 3,
                memory_bytes: GIB + GIB / 2,
            })
        );
        assert!(
            editor
                .allocation(Some((2, 64 * GIB)))
                .unwrap_err()
                .contains("CPU exceeds")
        );
        assert!(
            editor
                .allocation(Some((8, GIB)))
                .unwrap_err()
                .contains("MEM exceeds")
        );
        for text in ["", "0", "1.5", "-1", "18446744073709551616"] {
            editor.cpu = text.into();
            assert!(editor.allocation(None).is_err(), "{text}");
        }
    }
}
