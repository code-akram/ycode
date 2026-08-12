use crate::RequirementsLayerEntry;
use crate::config_requirements::RequirementSource;
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation::string::CFStringRef;
use std::ffi::c_void;
use std::io;
use tokio::task;

const MANAGED_PREFERENCES_APPLICATION_ID: &str = "com.openai.codex";
const MANAGED_PREFERENCES_REQUIREMENTS_KEY: &str = "requirements_toml_base64";

pub(super) fn managed_preferences_requirements_source() -> RequirementSource {
    RequirementSource::MdmManagedPreferences {
        domain: MANAGED_PREFERENCES_APPLICATION_ID.to_string(),
        key: MANAGED_PREFERENCES_REQUIREMENTS_KEY.to_string(),
    }
}

pub(crate) async fn load_managed_admin_requirements_layer(
    override_base64: Option<&str>,
) -> io::Result<Option<RequirementsLayerEntry>> {
    if let Some(encoded) = override_base64 {
        let trimmed = encoded.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }

        return parse_managed_requirements_base64(trimmed).map(|contents| {
            Some(RequirementsLayerEntry::from_toml(
                managed_preferences_requirements_source(),
                contents,
            ))
        });
    }

    match task::spawn_blocking(load_managed_admin_requirements).await {
        Ok(result) => Ok(result?.map(|contents| {
            RequirementsLayerEntry::from_toml(managed_preferences_requirements_source(), contents)
        })),
        Err(join_err) => {
            if join_err.is_cancelled() {
                tracing::error!("Managed requirements load task was cancelled");
            } else {
                tracing::error!("Managed requirements load task failed: {join_err}");
            }
            Err(io::Error::other("Failed to load managed requirements"))
        }
    }
}

fn load_managed_admin_requirements() -> io::Result<Option<String>> {
    load_managed_preference(MANAGED_PREFERENCES_REQUIREMENTS_KEY)?
        .as_deref()
        .map(str::trim)
        .map(parse_managed_requirements_base64)
        .transpose()
}

fn load_managed_preference(key_name: &str) -> io::Result<Option<String>> {
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFPreferencesCopyAppValue(key: CFStringRef, application_id: CFStringRef) -> *mut c_void;
    }

    let value_ref = unsafe {
        CFPreferencesCopyAppValue(
            CFString::new(key_name).as_concrete_TypeRef(),
            CFString::new(MANAGED_PREFERENCES_APPLICATION_ID).as_concrete_TypeRef(),
        )
    };

    if value_ref.is_null() {
        tracing::debug!(
            "Managed preferences for {MANAGED_PREFERENCES_APPLICATION_ID} key {key_name} not found",
        );
        return Ok(None);
    }

    let value = unsafe { CFString::wrap_under_create_rule(value_ref as _) }.to_string();
    Ok(Some(value))
}

fn parse_managed_requirements_base64(encoded: &str) -> io::Result<String> {
    decode_managed_preferences_base64(encoded)
}

fn decode_managed_preferences_base64(encoded: &str) -> io::Result<String> {
    String::from_utf8(BASE64_STANDARD.decode(encoded.as_bytes()).map_err(|err| {
        tracing::error!("Failed to decode managed value as base64: {err}",);
        io::Error::new(io::ErrorKind::InvalidData, err)
    })?)
    .map_err(|err| {
        tracing::error!("Managed value base64 contents were not valid UTF-8: {err}",);
        io::Error::new(io::ErrorKind::InvalidData, err)
    })
}
