//! The tool-calling loop shared by every agent: the model may call its MCP tools a bounded
//! number of times before giving its answer. Every call is recorded in the audit trail.

use anyhow::Result;
use serde_json::json;

use super::AgentContext;
use crate::{
    llm::{ChatMessage, LlmProvider, Usage},
    mcp::Toolbox,
};

/// One model call in the audit: who, which model, how many tokens, how many tool schemas
/// rode along. This is what cost analysis per role is built on.
fn record_call(
    ctx: &AgentContext,
    llm: &dyn LlmProvider,
    usage: Option<Usage>,
    tools: usize,
    round: usize,
) -> Result<()> {
    ctx.record(
        "LLM_CALL",
        &json!({
            "model": llm.model_name(),
            "input_tokens": usage.map(|usage| usage.input_tokens),
            "output_tokens": usage.map(|usage| usage.output_tokens),
            "tools": tools,
            "round": round,
        }),
    )
}

/// Tool rounds allowed for one answer.
pub const MAX_TOOL_ROUNDS: usize = 8;
const ARGUMENT_AUDIT_CHARS: usize = 500;

/// Planning is separate from implementation, including for CLI-backed department leads.
pub async fn plan_answer(
    llm: &dyn LlmProvider,
    system_prompt: &str,
    prompt: &str,
    ctx: &AgentContext,
) -> Result<String> {
    let completion = llm.generate_plan(system_prompt, prompt).await?;
    record_call(ctx, llm, completion.usage, 0, 0)?;
    Ok(completion.text)
}

/// Answers `prompt`, letting the model use `toolbox` when it has one.
pub async fn answer(
    llm: &dyn LlmProvider,
    system_prompt: &str,
    prompt: &str,
    toolbox: Option<&Toolbox>,
    ctx: &AgentContext,
) -> Result<String> {
    let tools = match toolbox {
        Some(toolbox) => {
            toolbox
                .specs_for(&format!("{system_prompt}\n{prompt}"))
                .await?
        }
        None => Vec::new(),
    };
    let tools = tools.as_slice();
    let (Some(toolbox), false) = (toolbox, tools.is_empty()) else {
        let completion = llm.generate(system_prompt, prompt).await?;
        record_call(ctx, llm, completion.usage, 0, 0)?;
        return Ok(completion.text);
    };

    let mut messages = vec![ChatMessage::User(prompt.to_string())];
    for round in 0..MAX_TOOL_ROUNDS {
        let turn = llm.chat(system_prompt, &messages, tools).await?;
        record_call(ctx, llm, turn.usage, tools.len(), round)?;
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
    record_call(ctx, llm, turn.usage, tools.len(), MAX_TOOL_ROUNDS)?;
    ctx.record(
        "TOOL_BUDGET_EXHAUSTED",
        &json!({ "ignored_calls": turn.tool_calls.len() }),
    )?;
    Ok(turn.text)
}
