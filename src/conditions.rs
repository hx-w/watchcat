use crate::models::{BackoffKind, PolicyAction};

pub const DEFAULT_PROMPT: &str = "Continue the previous unfinished task. Inspect the latest checkpoint and persisted changes first. Do not repeat completed work.";

#[derive(Clone, Copy, Debug)]
pub struct ConditionDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub action: PolicyAction,
    pub backoff: Option<BackoffKind>,
    pub initial_delay_seconds: u64,
    pub max_delay_seconds: u64,
    pub max_attempts: usize,
}

const fn retry(
    name: &'static str,
    description: &'static str,
    initial_delay_seconds: u64,
    max_delay_seconds: u64,
) -> ConditionDefinition {
    ConditionDefinition {
        name,
        description,
        action: PolicyAction::Retry,
        backoff: Some(BackoffKind::Exponential),
        initial_delay_seconds,
        max_delay_seconds,
        max_attempts: 5,
    }
}

const fn skip(name: &'static str, description: &'static str) -> ConditionDefinition {
    ConditionDefinition {
        name,
        description,
        action: PolicyAction::Skip,
        backoff: None,
        initial_delay_seconds: 0,
        max_delay_seconds: 0,
        max_attempts: 0,
    }
}

pub static CONDITIONS: &[ConditionDefinition] = &[
    retry(
        "network.connection_failed",
        "Could not establish or maintain a provider connection",
        5,
        120,
    ),
    retry(
        "network.stream_failed",
        "The provider response stream failed or disconnected",
        5,
        120,
    ),
    retry("network.timeout", "A provider request timed out", 10, 180),
    retry(
        "capacity.model_overloaded",
        "The selected model is temporarily overloaded",
        15,
        300,
    ),
    retry(
        "capacity.service_overloaded",
        "The provider service is temporarily overloaded",
        15,
        300,
    ),
    retry(
        "capacity.rate_limited",
        "The provider temporarily limited request throughput",
        30,
        600,
    ),
    retry(
        "capacity.server_throttled",
        "The provider temporarily throttled requests",
        15,
        300,
    ),
    retry(
        "service.server_error",
        "The provider returned a transient server error",
        10,
        300,
    ),
    retry(
        "service.conflict",
        "The provider rejected a transient conflicting request",
        5,
        120,
    ),
    retry(
        "retry.provider_exhausted",
        "The provider exhausted its internal retry attempts",
        15,
        300,
    ),
    skip(
        "auth.invalid",
        "Authentication is missing, invalid, or expired",
    ),
    skip("billing.required", "Billing or credits require user action"),
    skip(
        "capability.model_unavailable",
        "The requested model is unavailable to this account",
    ),
    skip(
        "capability.access_denied",
        "The account or organization cannot use this capability",
    ),
    skip(
        "capability.feature_unsupported",
        "The selected model does not support a requested feature",
    ),
    skip(
        "capability.entitlement_required",
        "The requested capability requires another entitlement",
    ),
    skip(
        "capability.verification_required",
        "The provider requires additional account verification",
    ),
    skip(
        "context.window_exceeded",
        "The request exceeded the model context window",
    ),
    skip(
        "context.output_limit",
        "The response reached the model output-token limit",
    ),
    skip(
        "quota.usage_exhausted",
        "The account usage allowance is exhausted",
    ),
    skip(
        "request.invalid",
        "The provider rejected the request as invalid",
    ),
    skip(
        "request.too_large",
        "The request exceeded the provider size limit",
    ),
    skip("sandbox.failed", "The local execution sandbox failed"),
    skip(
        "failure.unknown",
        "The provider failure could not be classified safely",
    ),
];

pub fn definition(name: &str) -> Option<&'static ConditionDefinition> {
    CONDITIONS.iter().find(|condition| condition.name == name)
}

pub fn is_known(name: &str) -> bool {
    definition(name).is_some()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn condition_names_are_unique_and_defaults_are_valid() {
        let mut names = HashSet::new();
        for condition in CONDITIONS {
            assert!(names.insert(condition.name));
            assert!(condition.name.contains('.'));
            match condition.action {
                PolicyAction::Retry => {
                    assert!(condition.backoff.is_some());
                    assert!(condition.initial_delay_seconds > 0);
                    assert!(condition.max_delay_seconds >= condition.initial_delay_seconds);
                    assert!(condition.max_attempts > 0);
                }
                PolicyAction::Skip => assert!(condition.backoff.is_none()),
            }
        }
    }
}
