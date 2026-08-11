use serde_json::Value as JsonValue;

use codex_code_mode_protocol::DEFAULT_IMAGE_DETAIL;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::ImageDetail;

const IMAGE_HELPER_EXPECTS_MESSAGE: &str =
    "image expects a non-empty image URL string or an object with image_url and optional detail";
const AUDIO_HELPER_EXPECTS_MESSAGE: &str =
    "audio expects a non-empty audio URL string or an object with audio_url";
const REMOTE_IMAGE_URL_ERROR: &str = "Tool call failed: remote image URLs are not supported in tool outputs. Pass a base64 data URI instead";
const INVALID_IMAGE_URL_ERROR: &str =
    "Tool call failed: invalid image output. Pass a base64 data URI instead";
const INVALID_AUDIO_URL_ERROR: &str =
    "Tool call failed: invalid audio output. Pass a base64 data URI instead";

pub(super) fn serialize_output_text(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<String, String> {
    if value.is_undefined()
        || value.is_null()
        || value.is_boolean()
        || value.is_number()
        || value.is_big_int()
        || value.is_string()
    {
        return Ok(value.to_rust_string_lossy(scope));
    }

    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    if let Some(stringified) = v8::json::stringify(&tc, value) {
        return Ok(stringified.to_rust_string_lossy(&tc));
    }
    if tc.has_caught() {
        return Err(tc
            .exception()
            .map(|exception| value_to_error_text(&mut tc, exception))
            .unwrap_or_else(|| "unknown code mode exception".to_string()));
    }
    Ok(value.to_rust_string_lossy(&tc))
}

pub(super) fn normalize_output_image(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
    detail_override: Option<String>,
) -> Result<FunctionCallOutputContentItem, ()> {
    let result = (|| -> Result<FunctionCallOutputContentItem, String> {
        let (image_url, detail) = if value.is_string() {
            (value.to_rust_string_lossy(scope), None)
        } else if value.is_object() && !value.is_array() {
            let object = v8::Local::<v8::Object>::try_from(value)
                .map_err(|_| IMAGE_HELPER_EXPECTS_MESSAGE.to_string())?;
            parse_output_image(scope, object)?
        } else {
            return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
        };

        if image_url.is_empty() {
            return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
        }
        let Some((scheme, _)) = image_url.split_once(':') else {
            return Err(INVALID_IMAGE_URL_ERROR.to_string());
        };
        if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            return Err(REMOTE_IMAGE_URL_ERROR.to_string());
        }
        if !scheme.eq_ignore_ascii_case("data") {
            return Err(INVALID_IMAGE_URL_ERROR.to_string());
        }

        let detail = detail_override.or(detail);
        let detail = match detail {
            Some(detail) => {
                let normalized = detail.to_ascii_lowercase();
                Some(match normalized.as_str() {
                    "auto" => ImageDetail::Auto,
                    "low" => ImageDetail::Low,
                    "high" => ImageDetail::High,
                    "original" => ImageDetail::Original,
                    _ => {
                        return Err(
                            "image detail must be one of: auto, low, high, original".to_string()
                        );
                    }
                })
            }
            None => Some(DEFAULT_IMAGE_DETAIL),
        };

        Ok(FunctionCallOutputContentItem::InputImage { image_url, detail })
    })();

    match result {
        Ok(item) => Ok(item),
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            Err(())
        }
    }
}

fn parse_output_image(
    scope: &mut v8::PinScope<'_, '_>,
    object: v8::Local<'_, v8::Object>,
) -> Result<(String, Option<String>), String> {
    let image_url_key = v8::String::new(scope, "image_url")
        .ok_or_else(|| "failed to allocate image helper keys".to_string())?;
    let Some(image_url) = object.get(scope, image_url_key.into()) else {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
    };
    if image_url.is_undefined() {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
    }
    if !image_url.is_string() {
        return Err(IMAGE_HELPER_EXPECTS_MESSAGE.to_string());
    }
    let detail_key = v8::String::new(scope, "detail")
        .ok_or_else(|| "failed to allocate image helper keys".to_string())?;
    let detail = parse_image_detail_value(scope, object.get(scope, detail_key.into()))?;
    Ok((image_url.to_rust_string_lossy(scope), detail))
}

fn parse_image_detail_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: Option<v8::Local<'s, v8::Value>>,
) -> Result<Option<String>, String> {
    match value {
        Some(value) if value.is_string() => Ok(Some(value.to_rust_string_lossy(scope))),
        Some(value) if value.is_null() || value.is_undefined() => Ok(None),
        Some(_) => Err("image detail must be a string when provided".to_string()),
        None => Ok(None),
    }
}

pub(super) fn normalize_output_audio(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<FunctionCallOutputContentItem, ()> {
    let result = (|| -> Result<FunctionCallOutputContentItem, String> {
        let audio_url = if value.is_string() {
            value.to_rust_string_lossy(scope)
        } else if value.is_object() && !value.is_array() {
            let object = v8::Local::<v8::Object>::try_from(value)
                .map_err(|_| AUDIO_HELPER_EXPECTS_MESSAGE.to_string())?;
            parse_output_audio(scope, object)?
        } else {
            return Err(AUDIO_HELPER_EXPECTS_MESSAGE.to_string());
        };

        if audio_url.is_empty() {
            return Err(AUDIO_HELPER_EXPECTS_MESSAGE.to_string());
        }
        let Some((scheme, _)) = audio_url.split_once(':') else {
            return Err(INVALID_AUDIO_URL_ERROR.to_string());
        };
        if !scheme.eq_ignore_ascii_case("data") {
            return Err(INVALID_AUDIO_URL_ERROR.to_string());
        }

        Ok(FunctionCallOutputContentItem::InputAudio { audio_url })
    })();

    match result {
        Ok(item) => Ok(item),
        Err(error_text) => {
            throw_type_error(scope, &error_text);
            Err(())
        }
    }
}

fn parse_output_audio(
    scope: &mut v8::PinScope<'_, '_>,
    object: v8::Local<'_, v8::Object>,
) -> Result<String, String> {
    let audio_url_key = v8::String::new(scope, "audio_url")
        .ok_or_else(|| "failed to allocate audio helper keys".to_string())?;
    let Some(audio_url) = object.get(scope, audio_url_key.into()) else {
        return Err(AUDIO_HELPER_EXPECTS_MESSAGE.to_string());
    };
    if audio_url.is_undefined() {
        return Err(AUDIO_HELPER_EXPECTS_MESSAGE.to_string());
    }
    if !audio_url.is_string() {
        return Err(AUDIO_HELPER_EXPECTS_MESSAGE.to_string());
    }
    Ok(audio_url.to_rust_string_lossy(scope))
}

pub(super) fn v8_value_to_json(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> Result<Option<JsonValue>, String> {
    let tc = std::pin::pin!(v8::TryCatch::new(scope));
    let mut tc = tc.init();
    let Some(stringified) = v8::json::stringify(&tc, value) else {
        if tc.has_caught() {
            return Err(tc
                .exception()
                .map(|exception| value_to_error_text(&mut tc, exception))
                .unwrap_or_else(|| "unknown code mode exception".to_string()));
        }
        return Ok(None);
    };
    serde_json::from_str(&stringified.to_rust_string_lossy(&tc))
        .map(Some)
        .map_err(|err| format!("failed to serialize JavaScript value: {err}"))
}

pub(super) fn json_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &JsonValue,
) -> Option<v8::Local<'s, v8::Value>> {
    let json = serde_json::to_string(value).ok()?;
    let json = v8::String::new(scope, &json)?;
    v8::json::parse(scope, json)
}

pub(super) fn value_to_error_text(
    scope: &mut v8::PinScope<'_, '_>,
    value: v8::Local<'_, v8::Value>,
) -> String {
    if value.is_object()
        && let Ok(object) = v8::Local::<v8::Object>::try_from(value)
        && let Some(key) = v8::String::new(scope, "stack")
        && let Some(stack) = object.get(scope, key.into())
        && stack.is_string()
    {
        return stack.to_rust_string_lossy(scope);
    }
    value.to_rust_string_lossy(scope)
}

pub(super) fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    if let Some(message) = v8::String::new(scope, message) {
        scope.throw_exception(message.into());
    }
}
