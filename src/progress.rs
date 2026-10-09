//! Bounded, public activity content. Private model history is never transformed here.
use serde_json::{Value, json};

pub const MAX_CONTENT_BYTES: usize = 32 * 1024;
const REDACTED: &str = "[redacted]";

/// Deployment values are scrubbed before activity snapshots reach durable storage.
/// This is an exact-value filter, not a detector for arbitrary secrets in task files.
#[derive(Clone, Default)]
pub struct PublicRedactor {
    secrets: Vec<String>,
}

impl PublicRedactor {
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut secrets = Vec::new();
        for value in values {
            if value.is_empty() {
                continue;
            }
            let escaped = serde_json::to_string(&value).expect("strings serialize");
            secrets.push(escaped[1..escaped.len() - 1].to_string());
            secrets.push(value);
        }
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.dedup();
        Self { secrets }
    }

    pub fn text(&self, value: &str, partial: bool) -> String {
        // Collect ranges in the original input. Replacing a partial suffix first
        // would break a complete credential with an overlapping prefix/suffix;
        // replacing in several passes could also inspect our own marker text.
        let mut ranges = Vec::new();
        for secret in &self.secrets {
            let mut search_from = 0;
            while let Some(offset) = value[search_from..].find(secret) {
                let start = search_from + offset;
                ranges.push((start, start + secret.len()));
                // Advance one codepoint, not one match, to cover overlapping
                // occurrences of periodic credentials as well.
                search_from = start
                    + value[start..]
                        .chars()
                        .next()
                        .expect("nonempty secret")
                        .len_utf8();
            }
            // Cumulative streaming snapshots can end halfway through a credential.
            // Withhold even a short suffix until the next snapshot disambiguates it.
            if partial {
                let end = value.len().min(secret.len().saturating_sub(1));
                if let Some(length) = (1..=end)
                    .rev()
                    .find(|&n| secret.is_char_boundary(n) && value.ends_with(&secret[..n]))
                {
                    ranges.push((value.len() - length, value.len()));
                }
            }
        }
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in ranges {
            if let Some(previous) = merged.last_mut()
                && start <= previous.1
            {
                previous.1 = previous.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        let mut clean = String::with_capacity(value.len());
        let mut cursor = 0;
        for (start, end) in merged {
            clean.push_str(&value[cursor..start]);
            clean.push_str(REDACTED);
            cursor = end;
        }
        clean.push_str(&value[cursor..]);
        clean
    }

    pub fn event(&self, data: &mut Value) {
        let Some(item) = data.get_mut("item").and_then(Value::as_object_mut) else {
            return;
        };
        let partial = item.get("status").and_then(Value::as_str) == Some("in_progress")
            || item.get("truncated").and_then(Value::as_bool) == Some(true);
        let mut truncated = false;
        for field in ["text", "input", "output", "error"] {
            if let Some(value) = item.get_mut(field)
                && let Some(text) = value.as_str()
            {
                let clean = self.text(text, partial);
                let (bounded, clipped) = bounded(&clean);
                truncated |= clipped;
                *value = Value::String(bounded);
            }
        }
        // JSON escaping can multiply byte size. Leave room for the Core envelope,
        // even when several fields consist entirely of control characters.
        while serde_json::to_vec(&*item).expect("JSON serializes").len() > 180 * 1024 {
            let largest = ["text", "input", "output", "error"]
                .into_iter()
                .filter_map(|field| {
                    item.get(field)
                        .and_then(Value::as_str)
                        .map(|text| (field, text.len()))
                })
                .max_by_key(|(_, length)| *length);
            let Some((field, length)) = largest.filter(|(_, length)| *length > 0) else {
                break;
            };
            let text = item
                .get(field)
                .and_then(Value::as_str)
                .expect("string field");
            let mut end = length / 2;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            item[field] = json!(&text[..end]);
            truncated = true;
        }
        if truncated {
            item.insert("truncated".into(), Value::Bool(true));
        }
    }
}

pub(crate) fn bounded(value: &str) -> (String, bool) {
    let mut end = value.len().min(MAX_CONTENT_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_owned(), end < value.len())
}

fn sensitive_field(name: &str) -> bool {
    let name = name.to_ascii_lowercase().replace('-', "_");
    matches!(
        name.as_str(),
        "authorization"
            | "proxy_authorization"
            | "api_key"
            | "apikey"
            | "password"
            | "passwd"
            | "secret"
            | "token"
            | "access_token"
            | "refresh_token"
            | "client_secret"
            | "cookie"
            | "set_cookie"
    )
}

fn sanitize_fields(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        if sensitive_field(key) {
                            json!(REDACTED)
                        } else {
                            sanitize_fields(value)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(sanitize_fields).collect()),
        _ => value.clone(),
    }
}

/// Complete cumulative snapshot of a PTC invocation or nested tool call.
pub(crate) struct Activity {
    value: Value,
}

impl Activity {
    pub(crate) fn new(id: String, kind: &str, name: &str, input: &Value) -> Self {
        let mut item = Self {
            value: json!({
                "id":id,"kind":kind,"status":"in_progress","phase":null,"name":name,
                "text":null,"input":null,"output":null,"error":null,"elapsed_ms":null,"truncated":false,
            }),
        };
        item.content("input", input);
        item
    }

    fn content(&mut self, field: &str, value: &Value) {
        let serialized = match value {
            Value::String(text) => text.clone(),
            _ => serde_json::to_string_pretty(&sanitize_fields(value)).expect("JSON serializes"),
        };
        let (content, truncated) = bounded(&serialized);
        self.value[field] = json!(content);
        if truncated {
            self.value["truncated"] = json!(true);
        }
    }

    pub(crate) fn finish(
        &mut self,
        result: &anyhow::Result<Value>,
        elapsed_ms: u64,
        cancelled: bool,
    ) {
        self.value["elapsed_ms"] = json!(elapsed_ms);
        self.value["status"] = json!(if cancelled {
            "interrupted"
        } else if result.as_ref().is_ok_and(succeeded) {
            "completed"
        } else {
            "failed"
        });
        match result {
            Ok(value) => self.content("output", value),
            Err(error) => self.content("error", &json!(error.to_string())),
        }
    }

    pub(crate) fn event(&self) -> Value {
        json!({"item":self.value})
    }
}

pub(crate) fn succeeded(value: &Value) -> bool {
    value
        .get("exit_code")
        .and_then(Value::as_i64)
        .is_none_or(|code| code == 0)
        && value.get("isError").and_then(Value::as_bool) != Some(true)
        && !matches!(
            value.get("status").and_then(Value::as_str),
            Some("failed" | "terminated")
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cumulative_snapshots_never_reveal_split_credentials() {
        let secret = "sk-test-秘密-credential";
        let redactor = PublicRedactor::new([secret.to_string(), "line\nsecret".into()]);
        for end in 1..=secret.len() {
            if secret.is_char_boundary(end) {
                assert_eq!(
                    redactor.text(&format!("answer {}", &secret[..end]), true),
                    "answer [redacted]"
                );
            }
        }
        assert_eq!(
            redactor.text("prefix sk-test-more", true),
            "prefix sk-test-more"
        );
        assert_eq!(redactor.text("line\\nsecret", false), REDACTED);
    }

    #[test]
    fn overlapping_secrets_and_prefixes_are_scrubbed_as_whole_ranges() {
        for secret in ["sk-demo-s", "abcabc", "aaaa", "秘密秘"] {
            let redactor = PublicRedactor::new([secret.to_owned()]);
            for end in 1..=secret.len() {
                if secret.is_char_boundary(end) {
                    assert_eq!(redactor.text(&secret[..end], true), REDACTED);
                }
            }
            assert_eq!(
                redactor.text(&format!("{secret} ok {secret}"), true),
                "[redacted] ok [redacted]"
            );
        }
        let redactor = PublicRedactor::new(["abcdef".to_owned(), "defghi".to_owned()]);
        assert_eq!(redactor.text("abcdefghi", false), REDACTED);
        let redactor = PublicRedactor::new(["ababab".to_owned()]);
        assert_eq!(redactor.text("abababab", false), REDACTED);
    }

    #[test]
    fn escaped_multi_field_snapshot_stays_within_persistent_frame_budget() {
        let mut item = json!({"item": {"id":"tool:1","status":"completed","text":"\u{1}".repeat(MAX_CONTENT_BYTES),
            "input":"\u{2}".repeat(MAX_CONTENT_BYTES),"output":"\u{3}".repeat(MAX_CONTENT_BYTES),
            "error":"\u{4}".repeat(MAX_CONTENT_BYTES),"truncated":false}});
        PublicRedactor::default().event(&mut item);
        assert!(serde_json::to_vec(&item).unwrap().len() < 200 * 1024);
        assert_eq!(item["item"]["truncated"], true);
    }

    #[test]
    fn activities_bound_unicode_and_sanitize_nested_fields() {
        let mut activity = Activity::new(
            "tool:1".into(),
            "tool_call",
            "example",
            &json!({"nested":[{"api_key":"hidden","path":"keep"}]}),
        );
        activity.finish(
            &Ok(json!({"stdout":"中".repeat(MAX_CONTENT_BYTES),"exit_code":3})),
            42,
            false,
        );
        let event = activity.event();
        assert!(!event.to_string().contains("hidden"));
        assert!(event.to_string().contains("keep"));
        assert_eq!(event["item"]["status"], "failed");
        assert_eq!(event["item"]["truncated"], true);
        assert!(event["item"]["output"].as_str().unwrap().len() <= MAX_CONTENT_BYTES);
    }

    #[test]
    fn redaction_does_not_change_item_identity_or_expand_content_past_limit() {
        let redactor = PublicRedactor::new(["x".to_string()]);
        let mut item = json!({"item":{"id":"x","status":"completed","text":"x ".repeat(MAX_CONTENT_BYTES / 2),"truncated":false}});
        redactor.event(&mut item);
        assert_eq!(item["item"]["id"], "x");
        assert_eq!(item["item"]["truncated"], true);
        assert_eq!(
            item["item"]["text"].as_str().unwrap().len(),
            MAX_CONTENT_BYTES
        );
    }
}
