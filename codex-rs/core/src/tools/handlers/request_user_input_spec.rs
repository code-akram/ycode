use codex_protocol::request_user_input::RequestUserInputQuestion;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::collections::BTreeMap;

pub const REQUEST_USER_INPUT_TOOL_NAME: &str = "request_user_input";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct RequestUserInputToolArgs {
    pub questions: Vec<RequestUserInputQuestion>,
}

pub fn create_request_user_input_tool() -> ToolSpec {
    let option_props = BTreeMap::from([
        (
            "label".to_string(),
            JsonSchema::string(Some("User-facing label (1-5 words).".to_string())),
        ),
        (
            "description".to_string(),
            JsonSchema::string(Some(
                "One short sentence explaining impact/tradeoff if selected.".to_string(),
            )),
        ),
    ]);

    let options_schema = JsonSchema::array(JsonSchema::object(
            option_props,
            Some(vec!["label".to_string(), "description".to_string()]),
            Some(false.into()),
        ), Some(
            "Provide 2-3 mutually exclusive choices. Put the recommended option first and suffix its label with \"(Recommended)\". Do not include an \"Other\" option in this list; the client will add a free-form \"Other\" option automatically."
                .to_string(),
        ));

    let question_props = BTreeMap::from([
        (
            "id".to_string(),
            JsonSchema::string(Some(
                "Stable identifier for mapping answers (snake_case).".to_string(),
            )),
        ),
        (
            "header".to_string(),
            JsonSchema::string(Some(
                "Short header label shown in the UI (12 or fewer chars).".to_string(),
            )),
        ),
        (
            "question".to_string(),
            JsonSchema::string(Some(
                "Single-sentence prompt shown to the user.".to_string(),
            )),
        ),
        ("options".to_string(), options_schema),
    ]);

    let questions_schema = JsonSchema::array(
        JsonSchema::object(
            question_props,
            Some(vec![
                "id".to_string(),
                "header".to_string(),
                "question".to_string(),
                "options".to_string(),
            ]),
            Some(false.into()),
        ),
        Some("Questions to show the user. Prefer 1 and do not exceed 3".to_string()),
    );

    let properties = BTreeMap::from([("questions".to_string(), questions_schema)]);

    ToolSpec::Function(ResponsesApiTool {
        name: REQUEST_USER_INPUT_TOOL_NAME.to_string(),
        description:
            "Request user input for one to three short questions and wait for the response."
                .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["questions".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

pub(crate) fn normalize_request_user_input_tool_args(
    mut args: RequestUserInputToolArgs,
) -> Result<RequestUserInputToolArgs, String> {
    let missing_options = args
        .questions
        .iter()
        .any(|question| question.options.as_ref().is_none_or(Vec::is_empty));
    if missing_options {
        return Err("request_user_input requires non-empty options for every question".to_string());
    }

    for question in &mut args.questions {
        question.is_other = true;
    }

    Ok(args)
}
