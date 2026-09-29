//! 思维链事件
//!
//! 处理 reasoningContentEvent 类型的事件。KRS 先用这类事件流出思考内容，
//! 再用 assistantResponseEvent 流出正文。实测 payload 为 `{"text":"..."}`。

use serde::{Deserialize, Serialize};

use crate::kiro::parser::error::ParseResult;
use crate::kiro::parser::frame::Frame;

use super::base::EventPayload;

/// 思维链事件
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningContentEvent {
    /// 思考内容片段
    #[serde(default)]
    pub text: String,

    /// 捕获其他未使用的字段（如 signature），确保反序列化兼容性
    #[serde(flatten)]
    #[serde(skip_serializing)]
    #[allow(dead_code)]
    extra: serde_json::Value,
}

impl EventPayload for ReasoningContentEvent {
    fn from_frame(frame: &Frame) -> ParseResult<Self> {
        frame.payload_as_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_text() {
        let event: ReasoningContentEvent = serde_json::from_str(r#"{"text":"Let me"}"#).unwrap();
        assert_eq!(event.text, "Let me");
    }

    #[test]
    fn deserialize_with_extra_fields() {
        let event: ReasoningContentEvent =
            serde_json::from_str(r#"{"text":"x","signature":"abc"}"#).unwrap();
        assert_eq!(event.text, "x");
    }

    #[test]
    fn missing_text_defaults_to_empty() {
        let event: ReasoningContentEvent = serde_json::from_str(r#"{}"#).unwrap();
        assert!(event.text.is_empty());
    }
}
