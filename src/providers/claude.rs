use serde_json::Value;

use crate::providers::codex::ClassifiedError;

/// Classifies the official `StopFailure` hook payload from Claude Code.
///
/// Claude's `rate_limit` and `model_not_found` values are deliberately mapped
/// conservatively because the hook does not always distinguish account limits
/// from temporary capacity, or a missing model from missing entitlement.
pub fn classify_claude_error(error: &str, details: Option<&Value>) -> ClassifiedError {
    let detail_code = details
        .and_then(|value| value.get("type").or_else(|| value.get("code")))
        .and_then(Value::as_str);
    let condition = match (error, detail_code) {
        ("overloaded", Some("model_overloaded")) => "capacity.model_overloaded",
        ("overloaded", _) => "capacity.service_overloaded",
        ("rate_limit", Some("server_throttled")) => "capacity.server_throttled",
        ("rate_limit", Some("usage_limit")) => "quota.usage_exhausted",
        ("rate_limit", _) => "capacity.rate_limited",
        ("authentication_failed", _) => "auth.invalid",
        ("oauth_org_not_allowed", _) => "capability.access_denied",
        ("billing_error", _) => "billing.required",
        ("invalid_request", Some("feature_unsupported")) => "capability.feature_unsupported",
        ("invalid_request", _) => "request.invalid",
        ("model_not_found", Some("access_denied")) => "capability.access_denied",
        ("model_not_found", _) => "capability.model_unavailable",
        ("server_error", _) => "service.server_error",
        ("max_output_tokens", _) => "context.output_limit",
        ("unknown", _) | (_, _) => "failure.unknown",
    };
    ClassifiedError {
        condition: condition.into(),
        provider_code: error.into(),
        retry_after_seconds: details.and_then(|value| {
            value
                .get("retry_after_seconds")
                .or_else(|| value.get("retryAfterSeconds"))
                .and_then(Value::as_u64)
        }),
        scope: if condition == "capacity.model_overloaded" {
            Some("model".into())
        } else if condition == "capacity.service_overloaded" {
            Some("service".into())
        } else {
            None
        },
    }
}

pub fn classify_claude_hook(payload: &Value) -> ClassifiedError {
    let error = payload
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let details = payload.get("error_details");
    let mut classified = classify_claude_error(error, details);
    if error == "overloaded" && payload.get("model").and_then(Value::as_str).is_some() {
        classified.condition = "capacity.model_overloaded".into();
        classified.scope = Some("model".into());
    }
    if details.is_some_and(Value::is_string) {
        // Official StopFailure currently permits free-form details. Keep the
        // top-level code authoritative instead of parsing display text.
        classified.retry_after_seconds = None;
    }
    classified
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn maps_every_official_stop_failure_code() {
        let cases = [
            ("overloaded", "capacity.service_overloaded"),
            ("rate_limit", "capacity.rate_limited"),
            ("authentication_failed", "auth.invalid"),
            ("oauth_org_not_allowed", "capability.access_denied"),
            ("billing_error", "billing.required"),
            ("invalid_request", "request.invalid"),
            ("model_not_found", "capability.model_unavailable"),
            ("server_error", "service.server_error"),
            ("max_output_tokens", "context.output_limit"),
            ("unknown", "failure.unknown"),
        ];
        for (code, expected) in cases {
            assert_eq!(classify_claude_error(code, None).condition, expected);
        }
    }

    #[test]
    fn uses_structured_details_without_guessing_from_messages() {
        let error = classify_claude_error(
            "overloaded",
            Some(&json!({"type": "model_overloaded", "retry_after_seconds": 17})),
        );
        assert_eq!(error.condition, "capacity.model_overloaded");
        assert_eq!(error.retry_after_seconds, Some(17));
        assert_eq!(error.scope.as_deref(), Some("model"));
    }

    #[test]
    fn hook_payload_keeps_free_form_details_conservative() {
        let classified = classify_claude_hook(&json!({
            "error": "rate_limit",
            "error_details": "429 Too Many Requests"
        }));
        assert_eq!(classified.condition, "capacity.rate_limited");
        assert_eq!(classified.retry_after_seconds, None);
    }

    #[test]
    fn hook_payload_scopes_overload_when_the_provider_identifies_a_model() {
        let classified = classify_claude_hook(&json!({
            "error": "overloaded",
            "model": "claude-opus"
        }));
        assert_eq!(classified.condition, "capacity.model_overloaded");
        assert_eq!(classified.scope.as_deref(), Some("model"));
    }
}
