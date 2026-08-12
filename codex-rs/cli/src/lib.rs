mod exit_status;
pub(crate) mod login;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("ycode supports only aarch64-apple-darwin");

pub use login::run_login_status;
pub use login::run_login_with_chatgpt;
pub use login::run_login_with_device_code;
pub use login::run_login_with_device_code_fallback_to_browser;
pub use login::run_logout;
