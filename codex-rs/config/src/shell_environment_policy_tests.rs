use super::*;
use pretty_assertions::assert_eq;

#[test]
fn shell_environment_policy_accepts_filters() {
    let filtered: ShellEnvironmentPolicyToml = toml::from_str(
        r#"
[filters]
"FLIP_TO_EXCLUDE" = "exclude"
"FLIP_TO_INCLUDE" = "include"
"#,
    )
    .expect("filters should be valid in config.toml");
    assert_eq!(
        filtered,
        ShellEnvironmentPolicyToml {
            filters: Some(BTreeMap::from([
                (
                    "FLIP_TO_EXCLUDE".to_string(),
                    ShellEnvironmentPolicyFilter::Exclude,
                ),
                (
                    "FLIP_TO_INCLUDE".to_string(),
                    ShellEnvironmentPolicyFilter::Include,
                ),
            ])),
            ..Default::default()
        }
    );
    let resolved = ShellEnvironmentPolicy::from(filtered);
    assert_eq!(resolved.exclude.len(), 1);
    assert_eq!(resolved.include_only.len(), 1);
}

#[test]
fn shell_environment_policy_rejects_legacy_lists() {
    let error = toml::from_str::<ShellEnvironmentPolicyToml>(
        r#"
exclude = ["LEGACY_*"]
"#,
    )
    .expect_err("obsolete exclude arrays should be rejected");
    assert!(error.to_string().contains("exclude"));
}

#[test]
fn shell_environment_policy_rejects_case_variant_filters_within_layer() {
    let error = toml::from_str::<ShellEnvironmentPolicyToml>(
        r#"
[filters]
"AWS_*" = "exclude"
"aws_*" = "include"
"#,
    )
    .expect_err("case-variant filters in one layer should be rejected");

    assert!(
        error
            .to_string()
            .contains("duplicate shell environment filter")
    );
}

#[test]
fn shell_environment_policy_rejects_unicode_case_variant_filters_within_layer() {
    let error = toml::from_str::<ShellEnvironmentPolicyToml>(
        r#"
[filters]
"СЕКРЕТ_*" = "exclude"
"секрет_*" = "include"
"#,
    )
    .expect_err("Unicode case-variant filters in one layer should be rejected");

    assert!(
        error
            .to_string()
            .contains("duplicate shell environment filter")
    );
}
