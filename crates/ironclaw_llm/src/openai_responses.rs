//! OpenAI-compatible Responses API provider.
//!
//! This provider is intentionally separate from rig-core's generic OpenAI
//! Responses path so IronClaw can preserve `function_call` / `call_id`
//! round-tripping across tool loops.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use reqwest::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use rust_decimal::Decimal;
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::RegistryProviderConfig;
use crate::costs;
use crate::error::LlmError;
use crate::provider::{
    ChatMessage, CompletionRequest, CompletionResponse, ContentPart, FinishReason, LlmProvider,
    ModelMetadata, Role, ToolCall, ToolCompletionRequest, ToolCompletionResponse, ToolDefinition,
    strip_unsupported_completion_params, strip_unsupported_tool_params,
};
use crate::retry::parse_retry_after_value;
use crate::tool_schema::{ToolSchemaPolicy, shape_tool_schema};

#[cfg(test)]
const PROVIDER: &str = "openai_compatible";

pub struct OpenAiResponsesProvider {
    client: Client,
    provider_id: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    extra_headers: HeaderMap,
    unsupported_params: HashSet<String>,
    input_cost: Decimal,
    output_cost: Decimal,
}

impl OpenAiResponsesProvider {
    pub fn new(config: &RegistryProviderConfig, base_url: String) -> Result<Self, LlmError> {
        let mut extra_headers = HeaderMap::new();
        for (key, value) in &config.extra_headers {
            let name = match HeaderName::from_bytes(key.as_bytes()) {
                Ok(name) => name,
                Err(e) => {
                    tracing::warn!(
                        provider = %config.provider_id,
                        header = %key,
                        error = %e,
                        "Skipping extra header: invalid name",
                    );
                    continue;
                }
            };
            let value = match HeaderValue::from_str(value) {
                Ok(value) => value,
                Err(e) => {
                    tracing::warn!(
                        provider = %config.provider_id,
                        header = %key,
                        error = %e,
                        "Skipping extra header: invalid value",
                    );
                    continue;
                }
            };
            extra_headers.insert(name, value);
        }

        let (input_cost, output_cost) =
            costs::model_cost(&config.model).unwrap_or_else(costs::default_cost);

        Ok(Self {
            client: Client::new(),
            provider_id: config.provider_id.clone(),
            base_url,
            model: config.model.clone(),
            api_key: config
                .api_key
                .as_ref()
                .map(|key| key.expose_secret().to_string()),
            extra_headers,
            unsupported_params: config.unsupported_params.iter().cloned().collect(),
            input_cost,
            output_cost,
        })
    }

    fn api_url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    fn build_headers(&self, accept: &'static str) -> Result<HeaderMap, LlmError> {
        let mut headers = self.extra_headers.clone();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static(accept));
        if let Some(api_key) = &self.api_key {
            let value = HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|e| {
                LlmError::RequestFailed {
                    provider: self.provider_id.clone(),
                    reason: format!("Invalid API key header value: {e}"),
                }
            })?;
            headers.insert(AUTHORIZATION, value);
        }
        Ok(headers)
    }

    fn build_request_body(
        &self,
        model: &str,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        tool_choice: Option<&str>,
        temperature: Option<f32>,
        max_tokens: Option<u32>,
    ) -> Value {
        let instructions = messages
            .iter()
            .filter(|message| message.role == Role::System)
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        let input = messages
            .iter()
            .filter(|message| message.role != Role::System)
            .flat_map(convert_message)
            .collect::<Vec<_>>();

        let mut body = json!({
            "model": model,
            "input": input,
            "store": false,
            "stream": true,
        });

        if !instructions.is_empty() {
            body["instructions"] = Value::String(instructions);
        }
        if let Some(temperature) = temperature {
            body["temperature"] = json!(round_f32_to_f64(temperature));
        }
        if let Some(max_tokens) = max_tokens {
            body["max_output_tokens"] = json!(max_tokens);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.iter().map(convert_tool_definition).collect());
            body["tool_choice"] = Value::String(tool_choice.unwrap_or("auto").to_string());
            body["parallel_tool_calls"] = Value::Bool(true);
        }

        body
    }

    async fn send_request(&self, body: Value) -> Result<ParsedResponse, LlmError> {
        let url = self.api_url("responses");
        let response = self
            .client
            .post(&url)
            .headers(self.build_headers("text/event-stream")?)
            .json(&body)
            .send()
            .await
            .map_err(|e| LlmError::RequestFailed {
                provider: self.provider_id.clone(),
                reason: format!("HTTP request failed: {e}"),
            })?;

        let status = response.status();
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get("retry-after")
                .map(parse_retry_after_value);
            let body_text = response.text().await.unwrap_or_default();
            return match status.as_u16() {
                401 => Err(LlmError::AuthFailed {
                    provider: self.provider_id.clone(),
                }),
                429 => Err(LlmError::RateLimited {
                    provider: self.provider_id.clone(),
                    retry_after,
                }),
                413 => {
                    let lower = body_text.to_ascii_lowercase();
                    let (used, limit) = crate::rig_adapter::parse_token_counts(&lower);
                    Err(LlmError::ContextLengthExceeded { used, limit })
                }
                500..=599 => {
                    tracing::debug!(
                        provider = %self.provider_id,
                        status = status.as_u16(),
                        body_preview = ironclaw_common::truncate_for_preview(&body_text, 512).as_str(),
                        "OpenAI-compatible Responses upstream 5xx response"
                    );
                    Err(LlmError::BadGateway {
                        provider: self.provider_id.clone(),
                        status: status.as_u16(),
                        retry_after,
                    })
                }
                _ => Err(LlmError::RequestFailed {
                    provider: self.provider_id.clone(),
                    reason: format!(
                        "HTTP {status}: {}",
                        ironclaw_common::truncate_for_preview(&body_text, 512)
                    ),
                }),
            };
        }

        let body_text = response.text().await.map_err(|e| LlmError::RequestFailed {
            provider: self.provider_id.clone(),
            reason: format!("Failed to read response body: {e}"),
        })?;
        parse_sse_response(&self.provider_id, &body_text)
    }

    async fn list_models_inner(&self) -> Result<Vec<String>, LlmError> {
        let url = self.api_url("models");
        let response = self
            .client
            .get(&url)
            .headers(self.build_headers("application/json")?)
            .send()
            .await
            .map_err(|e| LlmError::RequestFailed {
                provider: self.provider_id.clone(),
                reason: format!("Failed to fetch models: {e}"),
            })?;

        let status = response.status();
        let body_text = response.text().await.map_err(|e| LlmError::RequestFailed {
            provider: self.provider_id.clone(),
            reason: format!("Failed to read models response: {e}"),
        })?;
        if !status.is_success() {
            return Err(LlmError::RequestFailed {
                provider: self.provider_id.clone(),
                reason: format!(
                    "Models endpoint returned HTTP {status}: {}",
                    ironclaw_common::truncate_for_preview(&body_text, 512)
                ),
            });
        }

        let value: Value = serde_json::from_str(&body_text)?;
        Ok(extract_model_ids(&value))
    }
}

#[async_trait]
impl LlmProvider for OpenAiResponsesProvider {
    fn model_name(&self) -> &str {
        &self.model
    }

    fn cost_per_token(&self) -> (Decimal, Decimal) {
        (self.input_cost, self.output_cost)
    }

    async fn complete(
        &self,
        mut request: CompletionRequest,
    ) -> Result<CompletionResponse, LlmError> {
        let model_override = request.take_model_override();
        strip_unsupported_completion_params(&self.unsupported_params, &mut request);

        let mut messages = request.messages;
        crate::provider::sanitize_tool_messages(&mut messages);
        let model = model_override.as_deref().unwrap_or(&self.model);
        let body = self.build_request_body(
            model,
            &messages,
            &[],
            None,
            request.temperature,
            request.max_tokens,
        );
        let parsed = self.send_request(body).await?;

        Ok(CompletionResponse {
            content: parsed.text_content,
            input_tokens: parsed.input_tokens,
            output_tokens: parsed.output_tokens,
            finish_reason: parsed.finish_reason,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        })
    }

    async fn complete_with_tools(
        &self,
        mut request: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let model_override = request.take_model_override();
        strip_unsupported_tool_params(&self.unsupported_params, &mut request);

        let name_map = request
            .tools
            .iter()
            .filter_map(|tool| {
                let sanitized = sanitize_tool_name(&tool.name);
                (sanitized != tool.name).then(|| (sanitized, tool.name.clone()))
            })
            .collect::<HashMap<_, _>>();

        let mut messages = request.messages;
        crate::provider::sanitize_tool_messages(&mut messages);
        let model = model_override.as_deref().unwrap_or(&self.model);
        let body = self.build_request_body(
            model,
            &messages,
            &request.tools,
            request.tool_choice.as_deref(),
            request.temperature,
            request.max_tokens,
        );
        let mut parsed = self.send_request(body).await?;

        for tool_call in &mut parsed.tool_calls {
            if let Some(original) = name_map.get(&tool_call.name) {
                tool_call.name = original.clone();
            }
        }

        let finish_reason = if parsed.tool_calls.is_empty() {
            parsed.finish_reason
        } else {
            FinishReason::ToolUse
        };

        Ok(ToolCompletionResponse {
            content: (!parsed.text_content.is_empty()).then_some(parsed.text_content),
            tool_calls: parsed.tool_calls,
            input_tokens: parsed.input_tokens,
            output_tokens: parsed.output_tokens,
            finish_reason,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning: None,
        })
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.list_models_inner().await
    }

    async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> {
        Ok(ModelMetadata {
            id: self.model.clone(),
            context_length: None,
        })
    }

    fn effective_model_name(&self, requested_model: Option<&str>) -> String {
        crate::provider::normalized_model_override(requested_model)
            .unwrap_or(&self.model)
            .to_string()
    }
}

fn convert_message(message: &ChatMessage) -> Vec<Value> {
    match message.role {
        Role::System => Vec::new(),
        Role::User => {
            let content = if message.content_parts.is_empty() {
                vec![json!({
                    "type": "input_text",
                    "text": message.content,
                })]
            } else {
                let mut parts = Vec::new();
                if !message.content.is_empty() {
                    parts.push(json!({
                        "type": "input_text",
                        "text": message.content,
                    }));
                }
                parts.extend(message.content_parts.iter().map(|part| match part {
                    ContentPart::Text { text } => json!({
                        "type": "input_text",
                        "text": text,
                    }),
                    ContentPart::ImageUrl { image_url } => json!({
                        "type": "input_image",
                        "image_url": image_url.url,
                    }),
                }));
                parts
            };
            vec![json!({
                "type": "message",
                "role": "user",
                "content": content,
            })]
        }
        Role::Assistant => {
            if let Some(tool_calls) = &message.tool_calls {
                let mut items = Vec::new();
                if !message.content.is_empty() {
                    items.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": message.content,
                        }],
                    }));
                }
                items.extend(tool_calls.iter().map(|tool_call| {
                    let arguments = if tool_call.arguments.is_string() {
                        tool_call.arguments.as_str().unwrap_or("{}").to_string()
                    } else {
                        tool_call.arguments.to_string()
                    };
                    json!({
                        "type": "function_call",
                        "call_id": tool_call.id,
                        "name": sanitize_tool_name(&tool_call.name),
                        "arguments": arguments,
                    })
                }));
                items
            } else {
                vec![json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": message.content,
                    }],
                })]
            }
        }
        Role::Tool => vec![json!({
            "type": "function_call_output",
            "call_id": message.tool_call_id.as_deref().unwrap_or(""),
            "output": message.content,
        })],
    }
}

fn convert_tool_definition(tool: &ToolDefinition) -> Value {
    let mut description = tool.description.clone();
    let parameters = shape_tool_schema(
        ToolSchemaPolicy::StrictOpenAi,
        &tool.parameters,
        &mut description,
    );
    json!({
        "type": "function",
        "name": sanitize_tool_name(&tool.name),
        "description": description,
        "parameters": parameters,
    })
}

fn sanitize_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn round_f32_to_f64(value: f32) -> f64 {
    ((value as f64) * 1_000_000.0).round() / 1_000_000.0
}

fn extract_model_ids(value: &Value) -> Vec<String> {
    value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(|models| models.as_array())
        .map(|models| {
            models
                .iter()
                .filter_map(|model| {
                    model
                        .get("id")
                        .or_else(|| model.get("name"))
                        .or_else(|| model.get("model"))
                        .and_then(|id| id.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Deserialize)]
struct SseEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(flatten)]
    data: Value,
}

#[derive(Debug, Default)]
struct FunctionCallState {
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Default)]
struct ParsedResponse {
    text_content: String,
    tool_calls: Vec<ToolCall>,
    input_tokens: u32,
    output_tokens: u32,
    finish_reason: FinishReason,
}

fn parse_sse_response(provider: &str, body: &str) -> Result<ParsedResponse, LlmError> {
    let mut parsed = ParsedResponse {
        finish_reason: FinishReason::Stop,
        ..Default::default()
    };
    let mut active_function_calls: HashMap<String, FunctionCallState> = HashMap::new();
    let mut response_status: Option<String> = None;

    for line in body.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let data = if let Some(data) = line.strip_prefix("data: ") {
            data.trim()
        } else if let Some(data) = line.strip_prefix("data:") {
            data.trim()
        } else {
            continue;
        };
        if data == "[DONE]" {
            break;
        }

        let event: SseEvent = match serde_json::from_str(data) {
            Ok(event) => event,
            Err(e) => {
                tracing::trace!(data, error = %e, "Skipping unparseable Responses SSE event");
                continue;
            }
        };

        match event.event_type.as_str() {
            "response.output_text.delta" => {
                if let Some(delta) = event.data.get("delta").and_then(|delta| delta.as_str()) {
                    parsed.text_content.push_str(delta);
                }
            }
            "response.output_item.added" => {
                if let Some(item) = event.data.get("item")
                    && item.get("type").and_then(|value| value.as_str()) == Some("function_call")
                {
                    let item_id = item
                        .get("id")
                        .or_else(|| item.get("call_id"))
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string();
                    let call_id = item
                        .get("call_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or(&item_id)
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("")
                        .to_string();
                    active_function_calls.insert(
                        item_id,
                        FunctionCallState {
                            call_id,
                            name,
                            arguments: String::new(),
                        },
                    );
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = event
                    .data
                    .get("item_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                if let Some(state) = active_function_calls.get_mut(item_id)
                    && let Some(delta) = event.data.get("delta").and_then(|value| value.as_str())
                {
                    state.arguments.push_str(delta);
                }
            }
            "response.function_call_arguments.done" => {
                let item_id = event
                    .data
                    .get("item_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                if let Some(state) = active_function_calls.get_mut(item_id)
                    && let Some(arguments) =
                        event.data.get("arguments").and_then(|value| value.as_str())
                {
                    state.arguments = arguments.to_string();
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.data.get("item")
                    && item.get("type").and_then(|value| value.as_str()) == Some("function_call")
                {
                    let item_id = item
                        .get("id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    let state = active_function_calls.remove(item_id).unwrap_or_else(|| {
                        FunctionCallState {
                            call_id: item
                                .get("call_id")
                                .and_then(|value| value.as_str())
                                .unwrap_or(item_id)
                                .to_string(),
                            name: item
                                .get("name")
                                .and_then(|value| value.as_str())
                                .unwrap_or("")
                                .to_string(),
                            arguments: item
                                .get("arguments")
                                .and_then(|value| value.as_str())
                                .unwrap_or("{}")
                                .to_string(),
                        }
                    });
                    if !state.name.is_empty() {
                        parsed.tool_calls.push(ToolCall {
                            id: state.call_id,
                            name: state.name,
                            arguments: parse_arguments(&state.arguments),
                            reasoning: None,
                            signature: None,
                        });
                    }
                }
            }
            "response.completed" => {
                if let Some(response) = event.data.get("response") {
                    if let Some(usage) = response.get("usage") {
                        parsed.input_tokens = saturate_u32(
                            usage
                                .get("input_tokens")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0),
                        );
                        parsed.output_tokens = saturate_u32(
                            usage
                                .get("output_tokens")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0),
                        );
                    }
                    response_status = response
                        .get("status")
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                }
            }
            "response.failed" => {
                let reason = event
                    .data
                    .get("response")
                    .and_then(|response| response.get("status_details"))
                    .and_then(|details| details.get("error"))
                    .and_then(|error| error.get("message"))
                    .and_then(|message| message.as_str())
                    .unwrap_or("Unknown error");
                return Err(LlmError::RequestFailed {
                    provider: provider.to_string(),
                    reason: format!("Response failed: {reason}"),
                });
            }
            "error" => {
                let code = event
                    .data
                    .get("code")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown");
                let message = event
                    .data
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("Unknown error");
                return Err(LlmError::RequestFailed {
                    provider: provider.to_string(),
                    reason: format!("Error {code}: {message}"),
                });
            }
            _ => {}
        }
    }

    for (_, state) in active_function_calls {
        if !state.name.is_empty() {
            parsed.tool_calls.push(ToolCall {
                id: state.call_id,
                name: state.name,
                arguments: parse_arguments(&state.arguments),
                reasoning: None,
                signature: None,
            });
        }
    }

    parsed.finish_reason = if !parsed.tool_calls.is_empty() {
        FinishReason::ToolUse
    } else {
        match response_status.as_deref() {
            Some("incomplete") => FinishReason::Length,
            Some("completed") | None => FinishReason::Stop,
            _ => FinishReason::Stop,
        }
    };

    Ok(parsed)
}

fn parse_arguments(arguments: &str) -> Value {
    serde_json::from_str(arguments).unwrap_or_else(|_| Value::String(arguments.to_string()))
}

fn saturate_u32(value: u64) -> u32 {
    value.min(u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_messages_shapes_responses_input_items() {
        let messages = vec![
            ChatMessage::user("hello"),
            ChatMessage::assistant_with_tool_calls(
                Some("checking".to_string()),
                vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "read.file".to_string(),
                    arguments: json!({"path": "/tmp/a"}),
                    reasoning: None,
                    signature: None,
                }],
            ),
            ChatMessage::tool_result("call_1", "read.file", "contents"),
        ];

        let items = messages
            .iter()
            .flat_map(convert_message)
            .collect::<Vec<_>>();
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[1]["type"], "message");
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["name"], "read_file");
        assert_eq!(items[3]["type"], "function_call_output");
    }

    #[test]
    fn convert_tool_definition_uses_responses_function_shape() {
        let tool = ToolDefinition {
            name: "my.tool".to_string(),
            description: "Does work".to_string(),
            parameters: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
        };
        let converted = convert_tool_definition(&tool);
        assert_eq!(converted["type"], "function");
        assert_eq!(converted["name"], "my_tool");
        assert_eq!(converted["parameters"]["type"], "object");
    }

    #[test]
    fn parse_sse_text_response() {
        let body = r#"data: {"type":"response.output_text.delta","delta":"Hello "}

data: {"type":"response.output_text.delta","delta":"world"}

data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":7,"output_tokens":3}}}

"#;
        let parsed = parse_sse_response(PROVIDER, body).expect("parse");
        assert_eq!(parsed.text_content, "Hello world");
        assert_eq!(parsed.input_tokens, 7);
        assert_eq!(parsed.output_tokens, 3);
        assert_eq!(parsed.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn parse_sse_tool_call_response() {
        let body = r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"search"}}

data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"query\":"}

data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"rust\"}"}

data: {"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"search","arguments":"{\"query\":\"rust\"}"}}

data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":9,"output_tokens":5}}}

"#;
        let parsed = parse_sse_response(PROVIDER, body).expect("parse");
        assert_eq!(parsed.finish_reason, FinishReason::ToolUse);
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "call_1");
        assert_eq!(parsed.tool_calls[0].name, "search");
        assert_eq!(parsed.tool_calls[0].arguments, json!({"query": "rust"}));
    }
}
