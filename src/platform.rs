//! Platform names: `<os>-<arch>` with os `macos`, `linux` or `windows` and arch `aarch64` or
//! `x86_64`. `windows` is accepted in files and refused when chosen.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Os {
    Macos,
    Linux,
    Windows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Arch {
    Aarch64,
    X86_64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

impl Platform {
    pub fn parse(s: &str) -> Option<Platform> {
        let (os, arch) = s.split_once('-')?;
        let os = match os {
            "macos" => Os::Macos,
            "linux" => Os::Linux,
            "windows" => Os::Windows,
            _ => return None,
        };
        let arch = match arch {
            "aarch64" => Arch::Aarch64,
            "x86_64" => Arch::X86_64,
            _ => return None,
        };
        Some(Platform { os, arch })
    }

    /// The platform this program was built for, if it is one atelier names.
    pub fn current() -> Option<Platform> {
        let os = match std::env::consts::OS {
            "macos" => "macos",
            "linux" => "linux",
            "windows" => "windows",
            _ => return None,
        };
        Platform::parse(&format!("{os}-{}", std::env::consts::ARCH))
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let os = match self.os {
            Os::Macos => "macos",
            Os::Linux => "linux",
            Os::Windows => "windows",
        };
        let arch = match self.arch {
            Arch::Aarch64 => "aarch64",
            Arch::X86_64 => "x86_64",
        };
        write!(f, "{os}-{arch}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_six_names_and_nothing_else() {
        for s in [
            "macos-aarch64",
            "macos-x86_64",
            "linux-aarch64",
            "linux-x86_64",
            "windows-aarch64",
            "windows-x86_64",
        ] {
            assert_eq!(Platform::parse(s).unwrap().to_string(), s);
        }
        for s in [
            "macos",
            "linux-arm64",
            "darwin-aarch64",
            "macos-aarch64-x",
            "",
            "-",
        ] {
            assert!(Platform::parse(s).is_none(), "{s}");
        }
    }
}
