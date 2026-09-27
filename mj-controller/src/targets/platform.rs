use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOs {
    Linux,
    Darwin,
}

/// Platform of the execution boundary, which may differ from both the
/// controller and the host of a Linux container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetPlatform {
    pub os: TargetOs,
    pub architecture: &'static str,
}

impl TargetPlatform {
    pub fn parse(uname: &str) -> Result<Self> {
        let mut fields = uname.split_whitespace();
        let os = match fields.next() {
            Some("Linux") => TargetOs::Linux,
            Some("Darwin") => TargetOs::Darwin,
            other => bail!("unsupported target operating system {other:?}"),
        };
        let architecture = normalize_architecture(fields.next().unwrap_or_default())?;
        ensure!(
            fields.next().is_none(),
            "invalid target platform response {uname:?}"
        );
        Ok(Self { os, architecture })
    }
}

pub fn normalize_architecture(architecture: &str) -> Result<&'static str> {
    match architecture {
        "x86_64" | "amd64" => Ok("x86_64"),
        "aarch64" | "arm64" => Ok("aarch64"),
        other => bail!("unsupported target architecture {other:?}"),
    }
}

pub fn platform_probe(locator: &TargetLocator) -> CommandSpec {
    locator_command(locator, vec!["uname".into(), "-sm".into()]).purpose("detect target platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_platforms_and_rejects_unknown_responses() {
        for (text, os, architecture) in [
            ("Darwin arm64\n", TargetOs::Darwin, "aarch64"),
            ("Darwin x86_64\n", TargetOs::Darwin, "x86_64"),
            ("Linux aarch64\n", TargetOs::Linux, "aarch64"),
            ("Linux x86_64\n", TargetOs::Linux, "x86_64"),
        ] {
            assert_eq!(
                TargetPlatform::parse(text).unwrap(),
                TargetPlatform { os, architecture }
            );
        }
        for text in [
            "FreeBSD arm64",
            "Linux riscv64",
            "Darwin",
            "",
            "Linux x86_64 junk",
        ] {
            assert!(TargetPlatform::parse(text).is_err(), "{text}");
        }
    }
}
