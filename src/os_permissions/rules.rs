use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub const DIRECTORY_RULE: &str = "directory-access";

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DialogSettings {
    pub rules: BTreeMap<String, DialogRule>,
}

impl Default for DialogSettings {
    fn default() -> Self {
        Self {
            rules: BTreeMap::from([(DIRECTORY_RULE.into(), DialogRule::default())]),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DialogRule {
    pub enabled: bool,
    /// Exact bundle identifier or absolute executable path of the window owner.
    pub app: Option<String>,
    /// Exact window title.
    pub title: Option<String>,
    /// Substring of the primary static text, not supplementary descriptions.
    pub text: Option<String>,
    /// Exact button label.
    pub button: Option<String>,
}

impl Default for DialogRule {
    fn default() -> Self {
        Self {
            enabled: true,
            app: None,
            title: None,
            text: None,
            button: None,
        }
    }
}

impl DialogSettings {
    pub fn validate(&self) -> Result<()> {
        if self.rules.len() > 100 {
            bail!("at most 100 dialog rules are supported");
        }
        for (name, rule) in &self.rules {
            if name.is_empty()
                || name.len() > 80
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            {
                bail!("invalid dialog rule name: {name}");
            }
            for value in [&rule.app, &rule.title, &rule.text, &rule.button]
                .into_iter()
                .flatten()
            {
                if value.trim().is_empty()
                    || value.len() > 512
                    || value.chars().any(char::is_control)
                {
                    bail!(
                        "dialog rule {name} requires nonempty, single-line match values (up to 512 bytes)"
                    );
                }
            }
            if name == DIRECTORY_RULE {
                if rule.app.is_some()
                    || rule.title.is_some()
                    || rule.text.is_some()
                    || rule.button.is_some()
                {
                    bail!("directory-access is built in; only enabled can be changed");
                }
            } else if rule.app.is_none()
                || rule.button.is_none()
                || (rule.title.is_none() && rule.text.is_none())
            {
                bail!("dialog rule {name} requires app, button, and title or text");
            }
            if let Some(app) = &rule.app {
                if !app.starts_with('/') && (!app.contains('.') || app.contains(['*', '?', ' '])) {
                    bail!(
                        "dialog rule {name}: app must be an exact bundle ID or absolute executable path"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn observes(&self, executable: &str, bundle: &str) -> bool {
        self.rules.iter().any(|(name, rule)| {
            rule.enabled
                && if name == DIRECTORY_RULE {
                    executable.starts_with("/System/Library/")
                } else {
                    rule.app
                        .as_deref()
                        .is_some_and(|app| app == executable || app == bundle)
                }
        })
    }

    /// Multiple matching rules are ambiguous, even if they name the same button.
    pub fn matching<'a>(
        &'a self,
        executable: &str,
        bundle: &str,
        title: &str,
        heading: &str,
    ) -> Vec<(&'a str, &'a DialogRule)> {
        self.rules
            .iter()
            .filter(|(name, rule)| {
                if !rule.enabled {
                    return false;
                }
                if name.as_str() == DIRECTORY_RULE {
                    return executable.starts_with("/System/Library/")
                        && super::is_file_access_request(heading);
                }
                rule.app
                    .as_deref()
                    .is_some_and(|app| app == executable || app == bundle)
                    && rule
                        .title
                        .as_deref()
                        .is_none_or(|expected| title == expected)
                    && rule
                        .text
                        .as_deref()
                        .is_none_or(|expected| heading.contains(expected))
            })
            .map(|(name, rule)| (name.as_str(), rule))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_rules_require_all_selectors_and_detect_ambiguity() {
        let mut settings = DialogSettings::default();
        let rule = DialogRule {
            app: Some("org.example.Helper".into()),
            title: Some("Ready".into()),
            text: Some("Continue".into()),
            button: Some("Allow".into()),
            ..Default::default()
        };
        settings.rules.insert("helper".into(), rule.clone());
        settings.validate().unwrap();
        assert_eq!(
            settings
                .matching(
                    "/Applications/Helper",
                    "org.example.Helper",
                    "Ready",
                    "Continue now"
                )
                .len(),
            1
        );
        assert!(
            settings
                .matching(
                    "/Applications/Other",
                    "org.example.Other",
                    "Ready",
                    "Continue now"
                )
                .is_empty()
        );
        assert!(
            settings
                .matching(
                    "/Applications/Helper",
                    "org.example.Helper",
                    "Other",
                    "Continue now"
                )
                .is_empty()
        );
        assert!(
            settings
                .matching(
                    "/Applications/Helper",
                    "org.example.Helper",
                    "Ready",
                    "Wait"
                )
                .is_empty()
        );
        settings.rules.insert("duplicate".into(), rule);
        assert_eq!(
            settings
                .matching(
                    "/Applications/Helper",
                    "org.example.Helper",
                    "Ready",
                    "Continue now"
                )
                .len(),
            2
        );
        settings.rules.get_mut("helper").unwrap().enabled = false;
        assert_eq!(
            settings
                .matching(
                    "/Applications/Helper",
                    "org.example.Helper",
                    "Ready",
                    "Continue now"
                )
                .len(),
            1
        );
    }

    #[test]
    fn broad_or_invalid_rules_are_rejected() {
        let mut settings = DialogSettings::default();
        settings.rules.insert("bad".into(), DialogRule::default());
        assert!(settings.validate().is_err());
        settings.rules.remove("bad");
        settings.rules.get_mut(DIRECTORY_RULE).unwrap().button = Some("Delete".into());
        assert!(settings.validate().is_err());
    }
}
