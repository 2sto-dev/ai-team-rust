//! The tool-calling loop shared by every agent: the model may call its MCP tools a bounded
//! number of times before giving its answer. Every call is recorded in the audit trail.

use anyhow::Result;
use serde_json::json;

use super::AgentContext;
use crate::{
    llm::{ChatMessage, LlmProvider},
    mcp::Toolbox,
};

/// Tool rounds allowed for one answer.
pub const MAX_TOOL_ROUNDS: usize = 8;
const ARGUMENT_AUDIT_CHARS: usize = 500;

/// Answers `prompt`, letting the model use `toolbox` when it has one.
pub async fn answer(
    llm: &dyn LlmProvider,
    system_prompt: &str,
    prompt: &str,
    toolbox: Option<&Toolbox>,
    ctx: &AgentContext,
) -> Result<String> {
    let Some(toolbox) = toolbox else {
        return llm.complete(system_prompt, prompt).await;
    };
    let tools = toolbox.specs().await?;
    if tools.is_empty() {
        return llm.complete(system_prompt, prompt).await;
    }

    let mut messages = vec![ChatMessage::User(prompt.to_string())];
    for _ in 0..MAX_TOOL_ROUNDS {
        let turn = llm.chat(system_prompt, &messages, tools).await?;
        if turn.tool_calls.is_empty() {
            return Ok(turn.text);
        }
        messages.push(ChatMessage::Assistant {
            text: turn.text.clone(),
            tool_calls: turn.tool_calls.clone(),
        });
        for call in &turn.tool_calls {
            let outcome = toolbox.call(&call.name, call.arguments.clone()).await?;
            let arguments: String = call
                .arguments
                .to_string()
                .chars()
                .take(ARGUMENT_AUDIT_CHARS)
                .collect();
            ctx.record(
                "TOOL_CALL",
                &json!({
                    "tool": call.name,
                    "arguments": arguments,
                    "is_error": outcome.is_error,
                    "result_chars": outcome.text.len(),
                }),
            )?;
            messages.push(ChatMessage::Tool {
                call_id: call.id.clone(),
                name: call.name.clone(),
                content: outcome.text,
            });
        }
    }

    // Budget spent: one last turn for the answer (tools stay declared because the history
    // contains tool calls, but further calls are ignored).
    messages.push(ChatMessage::User(
        "You have used every tool call available for this answer. Give your final answer now, \
         without calling tools."
            .to_string(),
    ));
    let turn = llm.chat(system_prompt, &messages, tools).await?;
    ctx.record(
        "TOOL_BUDGET_EXHAUSTED",
        &json!({ "ignored_calls": turn.tool_calls.len() }),
    )?;
    Ok(turn.text)
}
