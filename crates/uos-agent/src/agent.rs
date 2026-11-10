use serde_json::Value;
use uos_llm::{
    ChatBackend, ChatRequest, FunctionCall, LlmError, Message, Role, ToolCall, ToolSpec,
};

use crate::{Approver, AuditEntry, AuditLog, Decision, Policy, Risk, Tool, Verdict};

const SYSTEM_PROMPT: &str = "You are the UnifiedOS system agent. You operate this Linux machine \
for the user through the tools provided. Inspect before you change: read the current state, then \
act. Use launch_app to run program files, including Windows .exe files; never run them with \
run_command. When the task is done, answer in plain language with what you found or changed. Answer in English \
unless the user wrote in another language. If a tool is refused, do not retry it; tell the user what was not done.";

const REFUSED: &str = "The user refused this action. It was not performed.";

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Model(#[from] LlmError),
    #[error("could not write the audit log, so the action was not run: {0}")]
    Audit(#[source] std::io::Error),
    #[error("{tool} ran, but its outcome could not be written to the audit log: {source}")]
    OutcomeNotLogged {
        tool: String,
        source: std::io::Error,
    },
    #[error("the request was not finished within {0} steps")]
    StepLimit(usize),
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub model: String,
    pub gpu_layers: Option<u32>,
    pub context_tokens: Option<u32>,
    pub max_steps: usize,
    pub policy: Policy,
}

pub struct Agent<'a> {
    backend: &'a dyn ChatBackend,
    approver: &'a dyn Approver,
    tools: Vec<Box<dyn Tool>>,
    audit: AuditLog,
    config: AgentConfig,
}

impl<'a> Agent<'a> {
    pub fn new(
        backend: &'a dyn ChatBackend,
        approver: &'a dyn Approver,
        tools: Vec<Box<dyn Tool>>,
        audit: AuditLog,
        config: AgentConfig,
    ) -> Self {
        Self {
            backend,
            approver,
            tools,
            audit,
            config,
        }
    }

    /// Works on one request until the model gives a final answer.
    pub fn run(&self, request: &str) -> Result<String, AgentError> {
        let specs: Vec<ToolSpec> = self.tools.iter().map(|tool| tool.spec()).collect();
        let mut messages = vec![
            Message::new(Role::System, SYSTEM_PROMPT),
            Message::new(Role::User, request),
        ];
        for _ in 0..self.config.max_steps {
            let reply = self.backend.chat(&ChatRequest {
                model: &self.config.model,
                messages: &messages,
                tools: &specs,
                gpu_layers: self.config.gpu_layers,
                context_tokens: self.config.context_tokens,
            })?;
            let calls = match reply.tool_calls.as_slice() {
                [] => tool_call_in_text(&reply.content, &specs),
                structured => structured.to_vec(),
            };
            if calls.is_empty() {
                return Ok(reply.content);
            }
            let results = calls
                .iter()
                .map(|call| self.execute(call))
                .collect::<Result<Vec<_>, _>>()?;
            messages.push(reply);
            messages.extend(results);
        }
        Err(AgentError::StepLimit(self.config.max_steps))
    }

    fn execute(&self, call: &ToolCall) -> Result<Message, AgentError> {
        let name = call.function.name.as_str();
        let args = &call.function.arguments;
        let Some(tool) = self.tools.iter().find(|tool| tool.spec().name == name) else {
            return Ok(Message::tool_result(name, format!("error: no tool named '{name}'")));
        };
        let risk = tool.risk(args);
        let decision = match self.config.policy.verdict(risk) {
            Verdict::Allow => Decision::Auto,
            Verdict::Ask if self.approver.approve(name, args, risk) => Decision::Approved,
            Verdict::Ask => Decision::Refused,
        };
        // Recorded before acting: if the log cannot be written, nothing runs.
        self.record(name, args, risk, decision)
            .map_err(AgentError::Audit)?;
        if decision == Decision::Refused {
            return Ok(Message::tool_result(name, REFUSED));
        }
        let (outcome, text) = match tool.run(args) {
            Ok(output) => (Decision::Succeeded, output),
            Err(error) => (Decision::Failed, format!("error: {error}")),
        };
        self.record(name, args, risk, outcome)
            .map_err(|source| AgentError::OutcomeNotLogged {
                tool: name.to_string(),
                source,
            })?;
        Ok(Message::tool_result(name, text))
    }

    fn record(&self, tool: &str, args: &Value, risk: Risk, decision: Decision) -> std::io::Result<()> {
        self.audit.record(&AuditEntry {
            tool,
            args,
            risk,
            decision,
        })
    }
}

/// Some local models write a tool call as JSON in their answer instead of
/// using the structured field. Recognise that form, for known tools only; the
/// call then goes through the same rating and approval as any other.
fn tool_call_in_text(content: &str, specs: &[ToolSpec]) -> Vec<ToolCall> {
    const WRAPPERS: &[&str] = &["<tool_call>", "</tool_call>", "```json", "```"];
    let bare = WRAPPERS
        .iter()
        .fold(content.to_string(), |text, wrapper| text.replace(wrapper, ""));
    let Ok(Value::Object(mut call)) = serde_json::from_str::<Value>(bare.trim()) else {
        return Vec::new();
    };
    let name = call.get("name").and_then(Value::as_str).map(str::to_string);
    let arguments = call.remove("arguments").filter(Value::is_object);
    match (name, arguments) {
        (Some(name), Some(arguments)) if specs.iter().any(|spec| spec.name == name) => {
            vec![ToolCall {
                function: FunctionCall { name, arguments },
            }]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    use serde_json::json;

    use super::*;
    use crate::ToolError;

    /// Replays scripted replies and remembers what it was sent.
    struct ScriptedBackend {
        replies: RefCell<VecDeque<Message>>,
        seen: RefCell<Vec<Vec<Message>>>,
    }

    impl ScriptedBackend {
        fn new(replies: Vec<Message>) -> Self {
            Self {
                replies: RefCell::new(replies.into()),
                seen: RefCell::new(Vec::new()),
            }
        }

        fn last_tool_result(&self) -> String {
            let seen = self.seen.borrow();
            let last = seen.last().unwrap();
            last.iter()
                .rev()
                .find(|m| m.role == Role::Tool)
                .map(|m| m.content.clone())
                .unwrap()
        }
    }

    impl ChatBackend for ScriptedBackend {
        fn chat(&self, request: &ChatRequest<'_>) -> Result<Message, LlmError> {
            self.seen.borrow_mut().push(request.messages.to_vec());
            self.replies
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| LlmError::Response("script exhausted".into()))
        }
    }

    struct FixedApprover {
        answer: bool,
        asked: Cell<usize>,
    }

    impl Approver for FixedApprover {
        fn approve(&self, _tool: &str, _args: &Value, _risk: Risk) -> bool {
            self.asked.set(self.asked.get() + 1);
            self.answer
        }
    }

    struct FakeTool {
        risk: Risk,
        fails: bool,
        runs: Rc<Cell<usize>>,
    }

    impl Tool for FakeTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "fake",
                description: "test tool",
                parameters: json!({}),
            }
        }

        fn risk(&self, _args: &Value) -> Risk {
            self.risk
        }

        fn run(&self, _args: &Value) -> Result<String, ToolError> {
            self.runs.set(self.runs.get() + 1);
            if self.fails {
                Err(ToolError::Failed("boom".into()))
            } else {
                Ok("tool output".into())
            }
        }
    }

    fn call(tool: &str) -> Message {
        Message {
            tool_calls: vec![ToolCall {
                function: FunctionCall {
                    name: tool.into(),
                    arguments: json!({}),
                },
            }],
            ..Message::new(Role::Assistant, "")
        }
    }

    fn answer(text: &str) -> Message {
        Message::new(Role::Assistant, text)
    }

    struct Fixture {
        backend: ScriptedBackend,
        approver: FixedApprover,
        runs: Rc<Cell<usize>>,
        audit_path: std::path::PathBuf,
    }

    impl Fixture {
        fn new(name: &str, replies: Vec<Message>, approves: bool) -> Self {
            let audit_path = std::env::temp_dir()
                .join(format!("uos-agent-{}-{name}/audit.jsonl", std::process::id()));
            let _ = std::fs::remove_file(&audit_path);
            Self {
                backend: ScriptedBackend::new(replies),
                approver: FixedApprover {
                    answer: approves,
                    asked: Cell::new(0),
                },
                runs: Rc::new(Cell::new(0)),
                audit_path,
            }
        }

        fn run(&self, risk: Risk, fails: bool, policy: Policy) -> Result<String, AgentError> {
            let tool = FakeTool {
                risk,
                fails,
                runs: Rc::clone(&self.runs),
            };
            let config = AgentConfig {
                model: "test".into(),
                gpu_layers: None,
                context_tokens: None,
                max_steps: 3,
                policy,
            };
            Agent::new(
                &self.backend,
                &self.approver,
                vec![Box::new(tool)],
                AuditLog::new(&self.audit_path),
                config,
            )
            .run("do it")
        }

        fn audit_decisions(&self) -> Vec<String> {
            std::fs::read_to_string(&self.audit_path)
                .unwrap_or_default()
                .lines()
                .map(|line| {
                    let entry: Value = serde_json::from_str(line).unwrap();
                    entry["action"]["decision"].as_str().unwrap().to_string()
                })
                .collect()
        }
    }

    #[test]
    fn returns_the_answer_when_no_tool_is_needed() {
        let fixture = Fixture::new("plain", vec![answer("all good")], true);

        let result = fixture.run(Risk::Observe, false, Policy::default()).unwrap();

        assert_eq!(result, "all good");
        assert_eq!(fixture.runs.get(), 0);
    }

    #[test]
    fn runs_an_observe_tool_without_asking_and_feeds_back_the_result() {
        let fixture = Fixture::new("observe", vec![call("fake"), answer("done")], false);

        let result = fixture.run(Risk::Observe, false, Policy::default()).unwrap();

        assert_eq!(result, "done");
        assert_eq!(fixture.approver.asked.get(), 0);
        assert_eq!(fixture.runs.get(), 1);
        assert_eq!(fixture.backend.last_tool_result(), "tool output");
        assert_eq!(fixture.audit_decisions(), ["auto", "succeeded"]);
    }

    #[test]
    fn does_not_run_a_refused_tool_and_tells_the_model() {
        let fixture = Fixture::new("refused", vec![call("fake"), answer("ok")], false);

        fixture.run(Risk::Modify, false, Policy::default()).unwrap();

        assert_eq!(fixture.approver.asked.get(), 1);
        assert_eq!(fixture.runs.get(), 0);
        assert_eq!(fixture.backend.last_tool_result(), REFUSED);
        assert_eq!(fixture.audit_decisions(), ["refused"]);
    }

    #[test]
    fn runs_a_modify_tool_after_approval() {
        let fixture = Fixture::new("approved", vec![call("fake"), answer("ok")], true);

        fixture.run(Risk::Modify, false, Policy::default()).unwrap();

        assert_eq!(fixture.runs.get(), 1);
        assert_eq!(fixture.audit_decisions(), ["approved", "succeeded"]);
    }

    #[test]
    fn still_asks_for_privileged_tools_when_modify_is_auto_approved() {
        let fixture = Fixture::new("privileged", vec![call("fake"), answer("ok")], false);
        let policy = Policy {
            auto_approve_modify: true,
        };

        fixture.run(Risk::Privileged, false, policy).unwrap();

        assert_eq!(fixture.approver.asked.get(), 1);
        assert_eq!(fixture.runs.get(), 0);
    }

    #[test]
    fn reports_a_tool_failure_to_the_model_and_carries_on() {
        let fixture = Fixture::new("fails", vec![call("fake"), answer("recovered")], true);

        let result = fixture.run(Risk::Observe, true, Policy::default()).unwrap();

        assert_eq!(result, "recovered");
        assert_eq!(fixture.backend.last_tool_result(), "error: boom");
        assert_eq!(fixture.audit_decisions(), ["auto", "failed"]);
    }

    #[test]
    fn tells_the_model_about_an_unknown_tool() {
        let fixture = Fixture::new("unknown", vec![call("nope"), answer("ok")], true);

        fixture.run(Risk::Observe, false, Policy::default()).unwrap();

        assert_eq!(fixture.backend.last_tool_result(), "error: no tool named 'nope'");
        assert_eq!(fixture.runs.get(), 0);
    }

    #[test]
    fn runs_a_tool_call_the_model_wrote_as_text() {
        let as_text = answer("```json\n{\"name\": \"fake\", \"arguments\": {}}\n```");
        let fixture = Fixture::new("textcall", vec![as_text, answer("done")], false);

        let result = fixture.run(Risk::Observe, false, Policy::default()).unwrap();

        assert_eq!(result, "done");
        assert_eq!(fixture.runs.get(), 1);
    }

    #[test]
    fn still_asks_before_a_risky_tool_call_written_as_text() {
        let as_text = answer("<tool_call>{\"name\": \"fake\", \"arguments\": {}}</tool_call>");
        let fixture = Fixture::new("textrisky", vec![as_text, answer("ok")], false);

        fixture.run(Risk::Privileged, false, Policy::default()).unwrap();

        assert_eq!(fixture.approver.asked.get(), 1);
        assert_eq!(fixture.runs.get(), 0);
    }

    #[test]
    fn treats_json_that_is_not_a_known_tool_call_as_the_answer() {
        for text in [
            "{\"name\": \"other\", \"arguments\": {}}",
            "{\"name\": \"fake\"}",
            "{\"name\": \"fake\", \"arguments\": 3}",
            "The answer is {\"name\": \"fake\", \"arguments\": {}}",
        ] {
            let fixture = Fixture::new("textplain", vec![answer(text)], true);

            let result = fixture.run(Risk::Observe, false, Policy::default()).unwrap();

            assert_eq!(result, text);
            assert_eq!(fixture.runs.get(), 0);
        }
    }

    #[test]
    fn stops_after_the_step_limit() {
        let replies = vec![call("fake"), call("fake"), call("fake"), call("fake")];
        let fixture = Fixture::new("limit", replies, true);

        let result = fixture.run(Risk::Observe, false, Policy::default());

        assert!(matches!(result, Err(AgentError::StepLimit(3))));
        assert_eq!(fixture.runs.get(), 3);
    }

    #[test]
    fn takes_no_action_when_the_audit_log_cannot_be_written() {
        let mut fixture = Fixture::new("noaudit", vec![call("fake"), answer("ok")], true);
        fixture.audit_path = "/proc/uos-not-writable/audit.jsonl".into();

        let result = fixture.run(Risk::Observe, false, Policy::default());

        assert!(matches!(result, Err(AgentError::Audit(_))));
        assert_eq!(fixture.runs.get(), 0);
    }

    #[test]
    fn surfaces_a_model_failure() {
        let fixture = Fixture::new("model", Vec::new(), true);

        let result = fixture.run(Risk::Observe, false, Policy::default());

        assert!(matches!(result, Err(AgentError::Model(_))));
    }
}
