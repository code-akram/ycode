use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use codex_utils_absolute_path::AbsolutePathBuf;

const BIN_DIRNAME: &str = "bin";
const CODE_MODE_HOST_EXECUTABLE_NAME: &str = if cfg!(windows) {
    "codex-code-mode-host.exe"
} else {
    "codex-code-mode-host"
};
const PACKAGE_METADATA_FILENAME: &str = "codex-package.json";
const PATH_DIRNAME: &str = "codex-path";
const RESOURCES_DIRNAME: &str = "codex-resources";
const ZSH_DIRNAME: &str = "zsh";
static INSTALL_CONTEXT: OnceLock<InstallContext> = OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexPackageLayout {
    /// The package root that contains the metadata file and layout directories.
    pub package_dir: AbsolutePathBuf,
    /// Directory containing the Codex entrypoint executable.
    pub bin_dir: AbsolutePathBuf,
    /// Directory containing bundled helper binaries and data files, when present.
    pub resources_dir: Option<AbsolutePathBuf>,
    /// Folder that should be prepended to PATH, when present.
    pub path_dir: Option<AbsolutePathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallContext {
    pub package_layout: Option<CodexPackageLayout>,
}

impl InstallContext {
    pub fn from_exe(current_exe: Option<&Path>) -> Self {
        Self {
            package_layout: current_exe.and_then(CodexPackageLayout::from_exe),
        }
    }

    pub fn current() -> &'static Self {
        INSTALL_CONTEXT.get_or_init(|| Self::from_exe(std::env::current_exe().ok().as_deref()))
    }

    pub fn rg_command(&self) -> PathBuf {
        if let Some(path_dir) = self
            .package_layout
            .as_ref()
            .and_then(|layout| layout.path_dir.as_ref())
        {
            let bundled_rg = path_dir.join(default_rg_command());
            if bundled_rg.is_file() {
                return bundled_rg.into_path_buf();
            }
        }
        default_rg_command()
    }

    pub fn code_mode_host_program(&self) -> PathBuf {
        self.bundled_resource(CODE_MODE_HOST_EXECUTABLE_NAME)
            .map_or_else(
                || self.code_mode_host_program_from_exe(std::env::current_exe().ok().as_deref()),
                AbsolutePathBuf::into_path_buf,
            )
    }

    fn code_mode_host_program_from_exe(&self, current_exe: Option<&Path>) -> PathBuf {
        let executable_dir = self
            .package_layout
            .as_ref()
            .map(|layout| layout.bin_dir.clone())
            .or_else(|| {
                current_exe
                    .and_then(Path::parent)
                    .and_then(canonical_absolute_path)
            });
        if let Some(executable_dir) = executable_dir {
            let executable = executable_dir.join(CODE_MODE_HOST_EXECUTABLE_NAME);
            if executable.is_file() {
                return executable.into_path_buf();
            }
        }

        current_exe
            .and_then(Path::parent)
            .map(|parent| parent.join(CODE_MODE_HOST_EXECUTABLE_NAME))
            .unwrap_or_else(|| PathBuf::from(CODE_MODE_HOST_EXECUTABLE_NAME))
    }

    pub fn bundled_resource(&self, file_name: impl AsRef<Path>) -> Option<AbsolutePathBuf> {
        let resources_dir = self.package_layout.as_ref()?.resources_dir.as_ref()?;
        let resource = resources_dir.join(file_name);
        resource.is_file().then_some(resource)
    }

    pub fn bundled_zsh_path(&self) -> Option<AbsolutePathBuf> {
        if cfg!(windows) {
            None
        } else {
            self.bundled_resource(zsh_resource_path())
        }
    }

    pub fn bundled_zsh_bin_dir(&self) -> Option<AbsolutePathBuf> {
        self.bundled_zsh_path()?.parent()
    }
}

impl CodexPackageLayout {
    fn from_exe(exe_path: &Path) -> Option<Self> {
        let canonical_exe = canonical_absolute_path(exe_path)?;
        let exe_dir = canonical_exe.parent()?;
        match exe_dir.file_name() {
            Some(name) if name == OsStr::new(BIN_DIRNAME) => Self::from_package_bin_dir(exe_dir),
            Some(_) | None => None,
        }
    }

    fn from_package_bin_dir(bin_dir: AbsolutePathBuf) -> Option<Self> {
        let package_dir = bin_dir.parent()?;
        if !package_dir.join(PACKAGE_METADATA_FILENAME).is_file() {
            return None;
        }

        Some(Self {
            resources_dir: existing_dir(package_dir.join(RESOURCES_DIRNAME)),
            path_dir: existing_dir(package_dir.join(PATH_DIRNAME)),
            package_dir,
            bin_dir,
        })
    }
}

fn canonical_absolute_path(path: &Path) -> Option<AbsolutePathBuf> {
    let canonical_path = std::fs::canonicalize(path).ok()?;
    AbsolutePathBuf::from_absolute_path(canonical_path).ok()
}

fn existing_dir(path: AbsolutePathBuf) -> Option<AbsolutePathBuf> {
    path.is_dir().then_some(path)
}

fn default_rg_command() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("rg.exe")
    } else {
        PathBuf::from("rg")
    }
}

fn zsh_resource_path() -> PathBuf {
    PathBuf::from(ZSH_DIRNAME).join(BIN_DIRNAME).join("zsh")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::fs;

    #[test]
    fn package_layout_exposes_bundled_resources_and_search() -> std::io::Result<()> {
        let package_dir = tempfile::tempdir()?;
        let bin_dir = package_dir.path().join(BIN_DIRNAME);
        let resources_dir = package_dir.path().join(RESOURCES_DIRNAME);
        let path_dir = package_dir.path().join(PATH_DIRNAME);
        fs::create_dir_all(&bin_dir)?;
        fs::create_dir_all(&resources_dir)?;
        fs::create_dir_all(&path_dir)?;
        fs::write(package_dir.path().join(PACKAGE_METADATA_FILENAME), "{}")?;
        let exe_path = bin_dir.join(if cfg!(windows) { "codex.exe" } else { "codex" });
        let rg_path = path_dir.join(default_rg_command());
        let host_path = resources_dir.join(CODE_MODE_HOST_EXECUTABLE_NAME);
        fs::write(&exe_path, "")?;
        fs::write(&rg_path, "")?;
        fs::write(&host_path, "")?;

        let context = InstallContext::from_exe(Some(&exe_path));

        assert!(context.package_layout.is_some());
        assert_eq!(context.rg_command(), rg_path.canonicalize()?);
        assert_eq!(context.code_mode_host_program(), host_path.canonicalize()?);
        Ok(())
    }

    #[test]
    fn arbitrary_binary_uses_path_search() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let exe_path = dir.path().join("codex");
        fs::write(&exe_path, "")?;

        let context = InstallContext::from_exe(Some(&exe_path));

        assert_eq!(context.package_layout, None);
        assert_eq!(context.rg_command(), default_rg_command());
        Ok(())
    }

    #[test]
    fn standalone_codex_resolves_adjacent_code_mode_host() -> std::io::Result<()> {
        let bin_dir = tempfile::tempdir()?;
        let exe_path = bin_dir
            .path()
            .join(if cfg!(windows) { "codex.exe" } else { "codex" });
        let host_path = bin_dir.path().join(CODE_MODE_HOST_EXECUTABLE_NAME);
        fs::write(&exe_path, "")?;
        fs::write(&host_path, "")?;

        let context = InstallContext::from_exe(Some(&exe_path));

        assert_eq!(context.package_layout, None);
        assert_eq!(
            context.code_mode_host_program_from_exe(Some(&exe_path)),
            host_path.canonicalize()?
        );
        Ok(())
    }
}
