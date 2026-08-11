use std::ffi::OsString;

use std::os::unix::ffi::OsStrExt;

/// This is designed to be called pre-main() (using `#[ctor::ctor]`) to perform
/// various process hardening steps, such as
/// - disabling core dumps
/// - disabling ptrace attach on macOS.
/// - removing dangerous `DYLD_*` environment variables
pub fn pre_main_hardening() {
    pre_main_hardening_macos();
}

const PTRACE_DENY_ATTACH_FAILED_EXIT_CODE: i32 = 6;

const SET_RLIMIT_CORE_FAILED_EXIT_CODE: i32 = 7;

pub(crate) fn pre_main_hardening_macos() {
    // Prevent debuggers from attaching to this process.
    let ret_code = unsafe { libc::ptrace(libc::PT_DENY_ATTACH, 0, std::ptr::null_mut(), 0) };
    if ret_code == -1 {
        eprintln!(
            "ERROR: ptrace(PT_DENY_ATTACH) failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(PTRACE_DENY_ATTACH_FAILED_EXIT_CODE);
    }

    // Set the core file size limit to 0 to prevent core dumps.
    set_core_file_size_limit_to_zero();

    // Remove all DYLD_ environment variables, which can be used to subvert
    // library loading.
    remove_env_vars_with_prefix(b"DYLD_");
}

fn set_core_file_size_limit_to_zero() {
    let rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    let ret_code = unsafe { libc::setrlimit(libc::RLIMIT_CORE, &rlim) };
    if ret_code != 0 {
        eprintln!(
            "ERROR: setrlimit(RLIMIT_CORE) failed: {}",
            std::io::Error::last_os_error()
        );
        std::process::exit(SET_RLIMIT_CORE_FAILED_EXIT_CODE);
    }
}

fn remove_env_vars_with_prefix(prefix: &[u8]) {
    for key in env_keys_with_prefix(std::env::vars_os(), prefix) {
        unsafe {
            std::env::remove_var(key);
        }
    }
}

fn env_keys_with_prefix<I>(vars: I, prefix: &[u8]) -> Vec<OsString>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    vars.into_iter()
        .filter_map(|(key, _)| {
            key.as_os_str()
                .as_bytes()
                .starts_with(prefix)
                .then_some(key)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn env_keys_with_prefix_handles_non_utf8_entries() {
        // RÖDBURK
        let non_utf8_key1 = OsStr::from_bytes(b"R\xD6DBURK").to_os_string();
        assert!(non_utf8_key1.clone().into_string().is_err());
        let non_utf8_key2 = OsString::from_vec(vec![b'D', b'Y', b'L', b'D', b'_', 0xF0]);
        assert!(non_utf8_key2.clone().into_string().is_err());

        let non_utf8_value = OsString::from_vec(vec![0xF0, 0x9F, 0x92, 0xA9]);

        let keys = env_keys_with_prefix(
            vec![
                (non_utf8_key1, non_utf8_value.clone()),
                (non_utf8_key2.clone(), non_utf8_value),
            ],
            b"DYLD_",
        );
        assert_eq!(
            keys,
            vec![non_utf8_key2],
            "non-UTF-8 env entries with DYLD_ prefix should be retained"
        );
    }

    #[test]
    fn env_keys_with_prefix_filters_only_matching_keys() {
        let dyld_test_var = OsStr::from_bytes(b"DYLD_TEST");
        let vars = vec![
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (dyld_test_var.to_os_string(), OsString::from("1")),
            (OsString::from("LD_FOO"), OsString::from("bar")),
        ];

        let keys = env_keys_with_prefix(vars, b"DYLD_");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].as_os_str(), dyld_test_var);
    }
}
