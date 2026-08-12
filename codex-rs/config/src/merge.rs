use codex_network_proxy::normalize_host;
use toml::Value as TomlValue;

/// Merge config `overlay` into `base`, giving `overlay` precedence.
pub fn merge_toml_values(base: &mut TomlValue, overlay: &TomlValue) {
    merge_toml_values_at_path(base, overlay, &mut Vec::new());
}

pub(crate) fn is_multi_agent_v2_feature_path<S: AsRef<str>>(path: &[S]) -> bool {
    match path {
        [features, feature] => {
            features.as_ref() == "features" && feature.as_ref() == "multi_agent_v2"
        }
        _ => false,
    }
}

fn merge_toml_values_at_path(base: &mut TomlValue, overlay: &TomlValue, path: &mut Vec<String>) {
    if is_multi_agent_v2_feature_path(path) {
        if let TomlValue::Boolean(enabled) = base
            && overlay.is_table()
        {
            *base = TomlValue::Table(toml::map::Map::from_iter([(
                "enabled".to_string(),
                TomlValue::Boolean(*enabled),
            )]));
        } else if let TomlValue::Table(table) = base
            && let TomlValue::Boolean(enabled) = overlay
        {
            table.insert("enabled".to_string(), TomlValue::Boolean(*enabled));
            return;
        }
    }

    if let TomlValue::Table(overlay_table) = overlay
        && let TomlValue::Table(base_table) = base
    {
        let mut overlay_table = overlay_table.clone();
        if is_permission_network_domains_path(path) {
            normalize_network_domain_keys(base_table);
            normalize_network_domain_keys(&mut overlay_table);
        }
        if is_shell_environment_filters_path(path) {
            normalize_case_insensitive_keys(base_table);
            normalize_case_insensitive_keys(&mut overlay_table);
        }

        for (key, value) in overlay_table {
            path.push(key.clone());
            if let Some(existing) = base_table.get_mut(&key) {
                merge_toml_values_at_path(existing, &value, path);
            } else {
                base_table.insert(key, value);
            }
            path.pop();
        }
    } else {
        *base = overlay.clone();
    }
}

fn is_shell_environment_filters_path(path: &[String]) -> bool {
    matches!(
        path,
        [policy, filters]
            if policy == "shell_environment_policy" && filters == "filters"
    )
}

/// Looks up a shell-environment filter pattern while ignoring case.
pub fn shell_environment_filter_entry<'a>(
    root: &'a TomlValue,
    path: &[String],
) -> Option<(&'a String, &'a TomlValue)> {
    let [policy, filters, pattern] = path else {
        return None;
    };
    if policy != "shell_environment_policy" || filters != "filters" {
        return None;
    }

    let pattern = pattern.to_lowercase();
    root.get(policy)?
        .get(filters)?
        .as_table()?
        .iter()
        .find(|(candidate, _)| candidate.to_lowercase() == pattern)
}

fn is_permission_network_domains_path(path: &[String]) -> bool {
    matches!(
        path,
        [permissions, _, network, domains]
            if permissions == "permissions" && network == "network" && domains == "domains"
    )
}

fn normalize_network_domain_keys(table: &mut toml::map::Map<String, TomlValue>) {
    let entries = std::mem::take(table);
    for (pattern, value) in entries {
        table.insert(normalize_host(&pattern), value);
    }
}

fn normalize_case_insensitive_keys(table: &mut toml::map::Map<String, TomlValue>) {
    let entries = std::mem::take(table);
    for (key, value) in entries {
        table.insert(key.to_lowercase(), value);
    }
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;
