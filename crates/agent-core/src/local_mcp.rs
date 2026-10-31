//! Configuration and authority for optional Local MCP tools. No transport or spawning.

use crate::adapter::Mode;
use crate::local_agent::{Gate, Tool};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub const APPROVAL_TOOL: &str = "local_mcp_tool";
pub const APPROVAL_OPTIONS: [crate::event::Decision; 4] = [
    crate::event::Decision::Allow,
    crate::event::Decision::AllowForSession,
    crate::event::Decision::AllowAlways,
    crate::event::Decision::Deny,
];

/// Content fingerprints, never a digest of argument values or resolved credentials.
pub fn revision(value: &impl Serialize) -> Result<String, &'static str> {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(value).map_err(|_| "Could not fingerprint MCP metadata.")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub fn alias(server: &str, name: &str) -> Result<String, &'static str> {
    Ok(format!("mcp_{}", &revision(&(server, name))?[..32]))
}

struct NoRetrieval;
impl jsonschema::Retrieve for NoRetrieval {
    fn retrieve(
        &self,
        _: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("External schema references are disabled.".into())
    }
}

/// Compiled once at discovery. Errors deliberately contain no schema or instance text.
#[derive(Clone)]
pub struct CompiledSchema(Arc<jsonschema::Validator>);
impl CompiledSchema {
    pub fn compile(schema: &Value) -> Result<Self, &'static str> {
        bounded_schema_graph(schema)?;
        if serde_json::to_vec(schema)
            .map_err(|_| "Invalid MCP schema encoding.")?
            .len()
            > 65536
        {
            return Err("MCP input schema exceeds 64 KiB.");
        }
        jsonschema::options()
            .with_pattern_options(
                jsonschema::PatternOptions::regex()
                    .size_limit(1024 * 1024)
                    .dfa_size_limit(1024 * 1024),
            )
            .with_retriever(NoRetrieval)
            .build(schema)
            .map(|validator| Self(Arc::new(validator)))
            .map_err(|_| {
                "MCP input schema could not compile with local references and linear patterns."
            })
    }
    pub fn validate(&self, value: &Value) -> Result<(), &'static str> {
        if !value.is_object()
            || serde_json::to_vec(value)
                .map_err(|_| "Invalid MCP argument encoding.")?
                .len()
                > 65536
        {
            return Err("MCP arguments must be an object of at most 64 KiB.");
        }
        if self.0.is_valid(value) {
            Ok(())
        } else {
            Err("MCP arguments do not match the advertised input schema.")
        }
    }
}

/// Cached reference nodes can still expand exponentially during validation. Bound the expanded
/// graph to 8,192 values and 64 levels, counting repeated edges; reject recursive schemas,
/// scopes we cannot resolve locally and annotation keywords that revalidate child graphs.
/// Literal data counts toward size/depth without acquiring schema keyword semantics.
fn bounded_schema_graph(root: &Value) -> Result<(), &'static str> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Context {
        Schema,
        Map,
        Array,
        Dependencies,
        Literal,
    }
    type Contexts = std::collections::HashMap<usize, Context>;
    fn classify(
        node: &Value,
        context: Context,
        depth: usize,
        contexts: &mut Contexts,
    ) -> Result<(), &'static str> {
        if depth > 64 || contexts.len() >= 8192 {
            return Err("MCP input schema could not compile: size or depth policy refused it.");
        }
        contexts.insert(std::ptr::from_ref(node) as usize, context);
        match node {
            Value::Object(fields) => {
                for (name, value) in fields {
                    let child = match context {
                        Context::Map => Context::Schema,
                        Context::Dependencies if !value.is_array() => Context::Schema,
                        Context::Schema => match name.as_str() {
                            "$defs" | "definitions" | "properties" | "patternProperties"
                            | "dependentSchemas" => Context::Map,
                            "allOf" | "anyOf" | "oneOf" | "prefixItems" => Context::Array,
                            "items" if value.is_array() => Context::Array,
                            "items"
                            | "additionalItems"
                            | "additionalProperties"
                            | "contains"
                            | "contentSchema"
                            | "if"
                            | "then"
                            | "else"
                            | "not"
                            | "propertyNames" => Context::Schema,
                            "dependencies" => Context::Dependencies,
                            _ => Context::Literal,
                        },
                        _ => Context::Literal,
                    };
                    classify(value, child, depth + 1, contexts)?;
                }
            }
            Value::Array(values) => {
                for value in values {
                    classify(
                        value,
                        if context == Context::Array {
                            Context::Schema
                        } else {
                            Context::Literal
                        },
                        depth + 1,
                        contexts,
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn visit(
        node: &Value,
        root: &Value,
        level: usize,
        contexts: &Contexts,
        active: &mut BTreeSet<usize>,
        memo: &mut std::collections::HashMap<usize, (usize, usize)>,
    ) -> Result<(usize, usize), &'static str> {
        const ERROR: &str = "MCP input schema could not compile: expansion, depth or local-reference policy refused it.";
        if level > 64 {
            return Err(ERROR);
        }
        let id = std::ptr::from_ref(node) as usize;
        if active.contains(&id) {
            return Err(ERROR);
        }
        if let Some(&(cost, depth)) = memo.get(&id) {
            return if level + depth <= 65 {
                Ok((cost, depth))
            } else {
                Err(ERROR)
            };
        }
        active.insert(id);
        let mut cost = 1usize;
        let mut depth = 1usize;
        let mut include = |child: &Value| -> Result<(), &'static str> {
            let (child_cost, child_depth) = visit(child, root, level + 1, contexts, active, memo)?;
            cost = cost.saturating_add(child_cost);
            depth = depth.max(child_depth + 1);
            if cost > 8192 || depth > 64 {
                return Err(ERROR);
            }
            Ok(())
        };
        match node {
            Value::Object(fields) => {
                if contexts.get(&id) == Some(&Context::Schema) {
                    if fields.contains_key("unevaluatedProperties")
                        || fields.contains_key("unevaluatedItems")
                    {
                        return Err(ERROR);
                    }
                    for key in [
                        "$id",
                        "id",
                        "$anchor",
                        "$dynamicAnchor",
                        "$recursiveAnchor",
                        "$dynamicRef",
                        "$recursiveRef",
                    ] {
                        if fields
                            .get(key)
                            .is_some_and(|v| v.is_string() || v.is_boolean())
                        {
                            return Err(ERROR);
                        }
                    }
                    if let Some(reference) = fields.get("$ref") {
                        let reference = reference.as_str().ok_or(ERROR)?;
                        let pointer = reference
                            .strip_prefix('#')
                            .filter(|p| (p.is_empty() || p.starts_with('/')) && !p.contains('%'))
                            .ok_or(ERROR)?;
                        let target = root.pointer(pointer).ok_or(ERROR)?;
                        if contexts.get(&(std::ptr::from_ref(target) as usize))
                            != Some(&Context::Schema)
                            || !(target.is_object() || target.is_boolean())
                        {
                            return Err(ERROR);
                        }
                        include(target)?;
                    }
                }
                for child in fields.values() {
                    include(child)?;
                }
            }
            Value::Array(values) => {
                for child in values {
                    include(child)?;
                }
            }
            _ => {}
        }
        active.remove(&id);
        memo.insert(id, (cost, depth));
        Ok((cost, depth))
    }
    let mut contexts = Contexts::new();
    classify(root, Context::Schema, 1, &mut contexts)?;
    visit(
        root,
        root,
        1,
        &contexts,
        &mut BTreeSet::new(),
        &mut std::collections::HashMap::new(),
    )
    .map(|_| ())
}

#[derive(Clone)]
pub struct RegistryTool {
    pub resolved: ResolvedTool,
    pub label: String,
    pub validator: CompiledSchema,
}

/// Rememberable authority contains identities and revisions, never arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub server_id: String,
    pub tool: String,
    pub server_revision: String,
    pub schema_revision: String,
}
impl Grant {
    pub fn valid(&self) -> bool {
        !self.server_id.is_empty()
            && self.server_id.len() <= 64
            && !self.tool.is_empty()
            && self.tool.len() <= 256
            && !self.tool.chars().any(char::is_control)
            && [&self.server_revision, &self.schema_revision]
                .iter()
                .all(|r| r.len() == 64 && r.bytes().all(|c| c.is_ascii_hexdigit()))
    }
}

/// An opaque token binds the displayed call to immutable raw arguments in runner memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalStamp {
    pub request_id: String,
    pub generation: String,
    pub call_id: String,
    pub grant: Grant,
}
impl ApprovalStamp {
    pub fn valid(&self) -> bool {
        self.grant.valid()
            && [&self.request_id, &self.generation, &self.call_id]
                .iter()
                .all(|s| {
                    !s.is_empty()
                        && s.len() <= 128
                        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalCard {
    pub binding: ApprovalStamp,
    pub server_label: String,
    pub arguments: Value,
}

/// Carried in Local's Approve.updated_input. Other adapters keep their native semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAnswer {
    pub binding: ApprovalStamp,
    pub edited_arguments: Option<Value>,
}

/// The caller scrubs first. Whole structured JSON is kept or explicitly omitted; serialized
/// JSON is never sliced. Resource links are data and are never followed.
pub fn result_text(result: &Value, reservation: usize) -> String {
    let max = reservation.min(crate::local_context::MAX_RESULT_BYTES);
    let mut content = Vec::new();
    for block in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let text = match block.get("type").and_then(Value::as_str) {
            Some("text") => block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("[Invalid text content omitted.]")
                .to_owned(),
            Some("resource") => block
                .pointer("/resource/text")
                .and_then(Value::as_str)
                .unwrap_or("[Non-text embedded resource omitted.]")
                .to_owned(),
            Some("resource_link") => "[Resource link omitted; not fetched.]".into(),
            _ => "[Unsupported binary or non-text content omitted.]".into(),
        };
        content.push(text);
    }
    let mut output = serde_json::json!({"isError":result.get("isError").and_then(Value::as_bool).unwrap_or(false),"content":content});
    if let Some(structured) = result.get("structuredContent") {
        output["structuredContent"] = structured.clone();
    }
    let encode = |v: &Value| {
        serde_json::to_string(v).unwrap_or_else(|_| "{\"isError\":true,\"content\":[]}".into())
    };
    let encoded = encode(&output);
    if encoded.len() <= max {
        return encoded;
    }
    if output
        .as_object_mut()
        .is_some_and(|o| o.remove("structuredContent").is_some())
    {
        content.push("[Structured content omitted because it exceeds the result budget.]".into());
    }
    let joined = content.join("\n");
    output["truncated"] = Value::Bool(true);
    let mut low = 0;
    let mut high = joined.len();
    while low < high {
        let mut end = (low + high).div_ceil(2);
        while end < joined.len() && !joined.is_char_boundary(end) {
            end += 1;
        }
        output["content"] = serde_json::json!([format!("{}\n[Result truncated.]", &joined[..end])]);
        if encode(&output).len() <= max {
            low = end;
        } else {
            high = end.saturating_sub(1);
            while !joined.is_char_boundary(high) {
                high -= 1;
            }
        }
    }
    output["content"] = serde_json::json!([format!("{}\n[Result truncated.]", &joined[..low])]);
    let encoded = encode(&output);
    if encoded.len() <= max {
        encoded
    } else {
        encode(&serde_json::json!({"isError":output["isError"],"truncated":true}))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    #[default]
    Inherit,
    Ask,
    Allow,
    Deny,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum EnvironmentValue {
    Reference { from_env: String },
    Literal(String),
}

impl std::fmt::Debug for EnvironmentValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reference { from_env } => f
                .debug_struct("Reference")
                .field("from_env", from_env)
                .finish(),
            Self::Literal(_) => f.write_str("Literal(<redacted>)"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpToolPolicy {
    pub name: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub approval: ApprovalPolicy,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    #[serde(default)]
    pub required: bool,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, EnvironmentValue>,
    pub tools: Vec<McpToolPolicy>,
    pub approval: ApprovalPolicy,
    pub startup_timeout_secs: u64,
    pub call_timeout_secs: u64,
}

impl std::fmt::Debug for McpServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServerConfig")
            .field("id", &self.id)
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=64).contains(&self.id.len())
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(
                "MCP server ID must contain 1–64 lowercase letters, digits or hyphens.".into(),
            );
        }
        if self.label.trim().is_empty()
            || self.label.len() > 256
            || self.command.len() > 4096
            || self.cwd.len() > 4096
            || self.command.trim().is_empty()
            || self.cwd.trim().is_empty()
            || self.command.contains('\0')
            || self.cwd.contains('\0')
            || self.args.iter().any(|a| a.contains('\0'))
            || self.args.len() > 128
            || self.args.iter().map(String::len).sum::<usize>() > 65536
            || self.env.len() > 128
            || self.tools.len() > 256
            || !(1..=300).contains(&self.startup_timeout_secs)
            || !(1..=600).contains(&self.call_timeout_secs)
            || self.approval == ApprovalPolicy::Inherit
        {
            return Err(format!("Invalid MCP definition or bounds for {}.", self.id));
        }
        for (name, value) in &self.env {
            if name.len() > 256 || !crate::provider::valid_env_name(name) {
                return Err(format!("Invalid MCP environment name: {name}"));
            }
            match value {
                EnvironmentValue::Reference { from_env }
                    if from_env.len() > 256 || !crate::provider::valid_env_name(from_env) =>
                {
                    return Err(format!("Invalid environment reference for {name}"))
                }
                EnvironmentValue::Literal(value)
                    if value.contains('\0')
                        || value.len() > 65536
                        || crate::redact::is_sensitive_name(name)
                        || crate::redact::redact(value) != *value =>
                {
                    return Err(format!("MCP environment {name} must use a reference for credentials or a bounded non-secret literal."));
                }
                _ => {}
            }
        }
        let mut names = BTreeSet::new();
        for tool in &self.tools {
            if tool.name.is_empty()
                || tool.name.len() > 256
                || tool.name.chars().any(char::is_control)
                || !names.insert(&tool.name)
            {
                return Err(format!("Invalid or duplicate tool name on {}.", self.id));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolIdentity {
    Builtin { name: String },
    Mcp { server_id: String, name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedTool {
    pub identity: ToolIdentity,
    pub alias: String,
    pub schema: Value,
    pub server_revision: String,
    pub schema_revision: String,
}

impl ResolvedTool {
    pub fn builtin(tool: Tool, schema: Value) -> Self {
        Self {
            identity: ToolIdentity::Builtin {
                name: tool.name().into(),
            },
            alias: tool.name().into(),
            schema,
            server_revision: String::new(),
            schema_revision: String::new(),
        }
    }
}

/// A grant is valid for exactly the call that was shown, including its generation and schema.
pub struct ApprovalBinding {
    pub stamp: ApprovalStamp,
    pub arguments: Value,
}

/// A changed executable/environment or a narrower policy ends the current authority.
pub fn narrows(old: &[McpServerConfig], new: &[McpServerConfig]) -> bool {
    fn restrictive(old: ApprovalPolicy, new: ApprovalPolicy) -> bool {
        old != new && (new == ApprovalPolicy::Deny || old == ApprovalPolicy::Allow)
    }
    old.iter().filter(|s| s.enabled).any(|before| {
        new.iter().find(|s| s.id == before.id).is_none_or(|after| {
            !after.enabled
                || before.command != after.command
                || before.args != after.args
                || before.cwd != after.cwd
                || before.env != after.env
                || before.tools.iter().any(|tool| {
                    after
                        .tools
                        .iter()
                        .find(|t| t.name == tool.name)
                        .is_none_or(|next| {
                            tool.read_only && !next.read_only
                                || restrictive(
                                    effective_policy(before, tool),
                                    effective_policy(after, next),
                                )
                        })
                })
        })
    })
}

fn effective_policy(server: &McpServerConfig, tool: &McpToolPolicy) -> ApprovalPolicy {
    if server.approval == ApprovalPolicy::Deny || tool.approval == ApprovalPolicy::Deny {
        return ApprovalPolicy::Deny;
    }
    match tool.approval {
        ApprovalPolicy::Inherit => server.approval,
        ApprovalPolicy::Allow if server.approval != ApprovalPolicy::Allow => ApprovalPolicy::Ask,
        other => other,
    }
}

/// Eligibility has already established selection, advertisement and valid arguments.
pub fn gate(server: &McpServerConfig, tool: &McpToolPolicy, mode: Mode, eligible: bool) -> Gate {
    if !eligible
        || !server.enabled
        || !server.tools.contains(tool)
        || server.approval == ApprovalPolicy::Deny
        || tool.approval == ApprovalPolicy::Deny
    {
        return Gate::Refuse("The MCP tool is disabled, disallowed or unavailable this turn.");
    }
    if mode == Mode::Plan && !tool.read_only {
        return Gate::Refuse("Plan requires an explicitly read-only MCP tool.");
    }
    let policy = effective_policy(server, tool);
    if mode == Mode::Bypass || policy == ApprovalPolicy::Allow {
        Gate::Run
    } else {
        Gate::Ask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn server() -> McpServerConfig {
        serde_json::from_value(serde_json::json!({"id":"docs","label":"Docs","enabled":true,"command":"server","args":[],"cwd":"$WORKSPACE","env":{},"tools":[{"name":"read","read_only":true}],"approval":"ask","startup_timeout_secs":15,"call_timeout_secs":60})).unwrap()
    }
    #[test]
    fn configuration_round_trip_and_names_only_debug() {
        let mut s = server();
        s.env.insert(
            "SERVICE_TOKEN".into(),
            EnvironmentValue::Reference {
                from_env: "SERVICE_TOKEN".into(),
            },
        );
        s.env.insert(
            "LANG".into(),
            EnvironmentValue::Literal("private-canary".into()),
        );
        s.validate().unwrap();
        assert!(!format!("{s:?}").contains("private-canary"));
        assert_eq!(
            serde_json::from_str::<McpServerConfig>(&serde_json::to_string(&s).unwrap()).unwrap(),
            s
        );
        s.env.insert(
            "SERVICE_TOKEN".into(),
            EnvironmentValue::Literal("private-canary".into()),
        );
        assert!(s.validate().is_err());
    }

    #[test]
    fn every_sensitive_environment_name_requires_a_reference() {
        for name in [
            "DB_PASSWD",
            "SSH_PASSPHRASE",
            "SIGNING_KEY",
            "PRIVATE_MATERIAL",
            "SERVICE_TOKEN",
        ] {
            let mut s = server();
            s.env.insert(
                name.into(),
                EnvironmentValue::Literal("ordinary-value".into()),
            );
            assert!(s.validate().is_err(), "{name}");
            s.env.insert(
                name.into(),
                EnvironmentValue::Reference {
                    from_env: name.into(),
                },
            );
            assert!(s.validate().is_ok(), "{name}");
        }
    }

    #[test]
    fn narrowing_uses_the_effective_policy_for_every_old_and_new_combination() {
        let policies = [
            ApprovalPolicy::Inherit,
            ApprovalPolicy::Ask,
            ApprovalPolicy::Allow,
            ApprovalPolicy::Deny,
        ];
        for old_server in policies {
            for old_tool in policies {
                for new_server in policies {
                    for new_tool in policies {
                        let mut old = server();
                        old.approval = old_server;
                        old.tools[0].approval = old_tool;
                        let mut new = old.clone();
                        new.approval = new_server;
                        new.tools[0].approval = new_tool;
                        let narrowed =
                            narrows(std::slice::from_ref(&old), std::slice::from_ref(&new));
                        for mode in [Mode::Plan, Mode::Ask, Mode::AcceptEdits, Mode::Bypass] {
                            if gate(&old, &old.tools[0], mode, true) == Gate::Run
                                && gate(&new, &new.tools[0], mode, true) != Gate::Run
                            {
                                assert!(narrowed,"{old_server:?}/{old_tool:?} -> {new_server:?}/{new_tool:?}, {mode:?}");
                            }
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn every_mode_preserves_eligibility_and_deny() {
        let mut s = server();
        for mode in [Mode::Plan, Mode::Ask, Mode::AcceptEdits, Mode::Bypass] {
            assert!(matches!(
                gate(&s, &s.tools[0], mode, false),
                Gate::Refuse(_)
            ));
            assert_eq!(
                gate(&s, &s.tools[0], mode, true),
                if mode == Mode::Bypass {
                    Gate::Run
                } else {
                    Gate::Ask
                }
            );
            let mut write = s.tools[0].clone();
            write.read_only = false;
            s.tools[0] = write.clone();
            assert_eq!(
                matches!(gate(&s, &write, mode, true), Gate::Refuse(_)),
                mode == Mode::Plan
            );
            s.tools[0].read_only = true;
        }
        s.approval = ApprovalPolicy::Deny;
        assert!(matches!(
            gate(&s, &s.tools[0], Mode::Bypass, true),
            Gate::Refuse(_)
        ));
    }
    #[test]
    fn validators_use_linear_patterns_and_never_retrieve_external_references() {
        use serde_json::json;
        let schema = json!({"type":"object","properties":{"value":{"type":"string","pattern":"^(a+)+$"}},"required":["value"],"additionalProperties":false});
        let validator = CompiledSchema::compile(&schema).unwrap();
        assert!(validator.validate(&json!({"value":"aaa"})).is_ok());
        let secret = format!("{}private-canary", "a".repeat(8192));
        let why = validator.validate(&json!({"value":secret})).unwrap_err();
        assert!(!why.contains("private-canary"));
        assert!(why.len() < 128);
        for reference in [
            "file:///tmp/schema.json",
            "https://example.com/schema.json",
            "../schema.json",
        ] {
            assert!(CompiledSchema::compile(&json!({"$ref":reference})).is_err());
        }
        assert!(CompiledSchema::compile(
            &json!({"type":"object","patternProperties":{"(?=unsafe)":{"type":"string"}}})
        )
        .is_err());
        let local = CompiledSchema::compile(&json!({"type":"object","$defs":{"number":{"type":"number"}},"properties":{"n":{"$ref":"#/$defs/number"}}})).unwrap();
        assert!(local.validate(&json!({"n":1})).is_ok());
        assert!(local.validate(&json!({"n":"wrong"})).is_err());
    }

    #[test]
    fn schema_graphs_bound_cycles_diamonds_and_reference_depth() {
        use serde_json::json;
        for schema in [
            json!({"$ref":"#"}),
            json!({"$defs":{"a":{"$ref":"#/$defs/b"},"b":{"$ref":"#/$defs/a"}},"$ref":"#/$defs/a"}),
        ] {
            assert!(CompiledSchema::compile(&schema).is_err());
        }
        let mut defs = serde_json::Map::from_iter([("d0".into(), json!({"type":"object"}))]);
        for i in 1..=24 {
            let reference = format!("#/$defs/d{}", i - 1);
            defs.insert(
                format!("d{i}"),
                json!({"allOf":[{"$ref":reference},{"$ref":reference}]}),
            );
        }
        let diamond = json!({"$defs":defs,"$ref":"#/$defs/d24"});
        assert!(serde_json::to_vec(&diamond).unwrap().len() < 65536);
        assert!(CompiledSchema::compile(&diamond).is_err());
        let mut defs = serde_json::Map::from_iter([("d0".into(), json!({"type":"object"}))]);
        for i in 1..=80 {
            defs.insert(format!("d{i}"), json!({"$ref":format!("#/$defs/d{}",i-1)}));
        }
        assert!(CompiledSchema::compile(&json!({"$defs":defs,"$ref":"#/$defs/d80"})).is_err());
        let mut inline = json!({"type":"object"});
        for _ in 0..40 {
            inline = json!({"allOf":[inline]});
        }
        assert!(CompiledSchema::compile(&inline).is_err());
        for keyword in ["unevaluatedProperties", "unevaluatedItems"] {
            let mut annotations = json!({"type":"object"});
            for _ in 0..24 {
                annotations = json!({"allOf":[annotations],keyword:false});
            }
            assert!(serde_json::to_vec(&annotations).unwrap().len() < 65536);
            assert!(CompiledSchema::compile(&annotations).is_err());
        }
        for schema in [
            json!({"$id":"https://example.com/schema"}),
            json!({"$dynamicRef":"#"}),
            json!({"$recursiveRef":"#"}),
            json!({"$ref":"#anchor"}),
            json!({"$ref":"#/$defs/a%20b"}),
        ] {
            assert!(CompiledSchema::compile(&schema).is_err());
        }
        let small = CompiledSchema::compile(&json!({"$defs":{"leaf":{"type":"object","required":["value"]},"shared":{"allOf":[{"$ref":"#/$defs/leaf"},{"$ref":"#/$defs/leaf"}]}},"allOf":[{"$ref":"#/$defs/shared"},{"$ref":"#/$defs/shared"}]})).unwrap();
        assert!(small.validate(&json!({"value":1})).is_ok());
        assert!(small.validate(&json!({})).is_err());
    }

    #[test]
    fn schema_keywords_do_not_apply_to_property_names_or_literal_payloads() {
        use serde_json::json;
        for schema in [
            json!({"type":"object","properties":{"id":{"type":"string"}}}),
            json!({"default":{"id":"main"}}),
            json!({"examples":[{"id":"1"}]}),
            json!({"properties":{"$ref":{"type":"string"}}}),
            json!({"$defs":{"$id":{"type":"string"}},"properties":{"value":{"$ref":"#/$defs/$id"}}}),
            json!({"const":{"$ref":"https://example.com/data","unevaluatedProperties":false,"$id":"literal"}}),
            json!({"enum":[{"$dynamicRef":"#","id":"literal"}]}),
        ] {
            assert!(CompiledSchema::compile(&schema).is_ok(), "{schema}");
        }
        let validator = CompiledSchema::compile(
            &json!({"properties":{"$ref":{"type":"string"},"id":{"type":"string"}}}),
        )
        .unwrap();
        assert!(validator
            .validate(&json!({"$ref":"literal","id":"value"}))
            .is_ok());
        assert!(validator.validate(&json!({"id":2})).is_err());
        for pointer in ["#/default", "#/examples/0", "#/properties"] {
            assert!(CompiledSchema::compile(&json!({"$ref":pointer,"default":{"type":"object"},"examples":[{"type":"object"}],"properties":{}})).is_err());
        }
    }

    #[test]
    fn every_executable_schema_slot_keeps_reference_and_cost_checks() {
        use serde_json::json;
        for keyword in [
            "additionalItems",
            "additionalProperties",
            "contains",
            "contentSchema",
            "if",
            "then",
            "else",
            "not",
            "propertyNames",
            "items",
        ] {
            assert!(
                bounded_schema_graph(&json!({keyword:{"$ref":"#"}})).is_err(),
                "{keyword}"
            );
        }
        for keyword in ["allOf", "anyOf", "oneOf", "prefixItems", "items"] {
            assert!(
                bounded_schema_graph(&json!({keyword:[{"$ref":"#"}]})).is_err(),
                "{keyword}"
            );
        }
        for keyword in [
            "$defs",
            "definitions",
            "properties",
            "patternProperties",
            "dependentSchemas",
            "dependencies",
        ] {
            assert!(
                bounded_schema_graph(&json!({keyword:{"id":{"$ref":"#"}}})).is_err(),
                "{keyword}"
            );
        }
        assert!(bounded_schema_graph(
            &json!({"dependencies":{"id":["$ref","$id","unevaluatedProperties"]}})
        )
        .is_ok());
    }
    #[test]
    fn bounded_results_keep_is_error_json_and_omit_binary_and_links() {
        use serde_json::json;
        let result = json!({"isError":true,"content":[
            {"type":"text","text":"雪".repeat(2000)},
            {"type":"image","data":"BINARY-CANARY"},
            {"type":"resource_link","uri":"https://example.com/private"},
            {"type":"resource","resource":{"text":"embedded"}}
        ],"structuredContent":{"payload":"x".repeat(5000)}});
        let full = result_text(&result, 30000);
        assert!(full.contains("Unsupported binary"));
        assert!(full.contains("not fetched"));
        assert!(!full.contains("BINARY-CANARY"));
        assert!(!full.contains("https://"));
        for cap in [128, 256, 1024] {
            let text = result_text(&result, cap);
            assert!(text.len() <= cap);
            let parsed: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(parsed["isError"], true);
            assert_eq!(parsed["truncated"], true);
        }
    }
    #[test]
    fn aliases_are_short_and_identity_based_and_only_narrowing_revokes() {
        let a = alias("a", "read").unwrap();
        assert_ne!(a, alias("b", "read").unwrap());
        assert!(a.len() <= 64);
        let old = server();
        let mut new = old.clone();
        new.tools.push(McpToolPolicy {
            name: "extra".into(),
            read_only: true,
            approval: ApprovalPolicy::Ask,
        });
        assert!(!narrows(
            std::slice::from_ref(&old),
            std::slice::from_ref(&new)
        ));
        new.tools[0].read_only = false;
        assert!(narrows(
            std::slice::from_ref(&old),
            std::slice::from_ref(&new)
        ));
        assert!(narrows(&[old], &[]));
    }
    #[test]
    fn complete_policy_matrix_never_widens_its_server_or_bypass_eligibility() {
        let mut s = server();
        for server_policy in [
            ApprovalPolicy::Ask,
            ApprovalPolicy::Allow,
            ApprovalPolicy::Deny,
        ] {
            s.approval = server_policy;
            for tool_policy in [
                ApprovalPolicy::Inherit,
                ApprovalPolicy::Ask,
                ApprovalPolicy::Allow,
                ApprovalPolicy::Deny,
            ] {
                for readonly in [true, false] {
                    s.tools[0].approval = tool_policy;
                    s.tools[0].read_only = readonly;
                    for mode in [Mode::Plan, Mode::Ask, Mode::AcceptEdits, Mode::Bypass] {
                        let actual = gate(&s, &s.tools[0], mode, true);
                        let denied = server_policy == ApprovalPolicy::Deny
                            || tool_policy == ApprovalPolicy::Deny
                            || mode == Mode::Plan && !readonly;
                        let run = mode == Mode::Bypass
                            || server_policy == ApprovalPolicy::Allow
                                && matches!(
                                    tool_policy,
                                    ApprovalPolicy::Inherit | ApprovalPolicy::Allow
                                );
                        assert_eq!(
                            actual,
                            if denied {
                                Gate::Refuse(
                                    if server_policy == ApprovalPolicy::Deny
                                        || tool_policy == ApprovalPolicy::Deny
                                    {
                                        "The MCP tool is disabled, disallowed or unavailable this turn."
                                    } else {
                                        "Plan requires an explicitly read-only MCP tool."
                                    },
                                )
                            } else if run {
                                Gate::Run
                            } else {
                                Gate::Ask
                            }
                        );
                    }
                }
            }
        }
    }
}
