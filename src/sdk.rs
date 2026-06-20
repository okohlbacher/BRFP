use std::{
    env, fs,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::pipeline::{BrfpError, BrfpResult};

#[derive(Debug, Clone, Serialize)]
pub struct SdkDiscovery {
    pub root: PathBuf,
    pub platform: String,
    pub library_path: PathBuf,
    pub library_size_bytes: u64,
}

impl SdkDiscovery {
    pub fn discover(explicit_dir: Option<&Path>) -> BrfpResult<Option<Self>> {
        let Some(root) = explicit_dir.map(Path::to_path_buf).or_else(env_sdk_dir) else {
            return Ok(None);
        };
        Self::from_root(root).map(Some)
    }

    pub fn from_root(root: PathBuf) -> BrfpResult<Self> {
        let platform = current_sdk_platform()?;
        let library_path = root.join(platform.relative_library_path());
        if !library_path.exists() {
            return Err(BrfpError::InvalidInput(format!(
                "Bruker SDK library not found at {}",
                library_path.display()
            )));
        }
        let library_size_bytes = fs::metadata(&library_path)?.len();
        Ok(Self {
            root,
            platform: platform.name().to_string(),
            library_path,
            library_size_bytes,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SdkPlatform {
    LinuxX86_64,
    WindowsX86_64,
}

impl SdkPlatform {
    fn name(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "linux-x86_64",
            Self::WindowsX86_64 => "windows-x86_64",
        }
    }

    fn relative_library_path(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "linux64/libtimsdata.so",
            Self::WindowsX86_64 => "win64/timsdata.dll",
        }
    }
}

fn current_sdk_platform() -> BrfpResult<SdkPlatform> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok(SdkPlatform::LinuxX86_64),
        ("windows", "x86_64") => Ok(SdkPlatform::WindowsX86_64),
        (os, arch) => Err(BrfpError::UnsupportedPlatform(format!(
            "Bruker SDK is available only for linux-x86_64 and windows-x86_64, not {os}-{arch}"
        ))),
    }
}

fn env_sdk_dir() -> Option<PathBuf> {
    env::var_os("TIMSDATA_LIB_DIR")
        .map(PathBuf::from)
        .map(|path| normalize_sdk_root(&path))
}

fn normalize_sdk_root(path: &Path) -> PathBuf {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("linux64" | "win64") => path.parent().unwrap_or(path).to_path_buf(),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_platform_subdirectory_to_sdk_root() {
        assert_eq!(
            normalize_sdk_root(Path::new("/sdk/linux64")),
            PathBuf::from("/sdk")
        );
        assert_eq!(
            normalize_sdk_root(Path::new("/sdk/win64")),
            PathBuf::from("/sdk")
        );
        assert_eq!(normalize_sdk_root(Path::new("/sdk")), PathBuf::from("/sdk"));
    }
}
