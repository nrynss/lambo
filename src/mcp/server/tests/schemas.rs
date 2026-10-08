//! Published tool list, parameter schemas and their goldens.

use super::*;

#[tokio::test]
async fn the_router_publishes_exactly_the_seven_spec_tools() {
    let s = server("mcp-seven").await;
    let mut names: Vec<String> = tools(&s).iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "lambo_derive",
            "lambo_inspect",
            "lambo_recall",
            "lambo_record_action",
            "lambo_reserve",
            "lambo_saints",
            "lambo_stats",
        ],
        "spec §6.2 names the seven tools exactly; adding or renaming one is a spec change"
    );
    s.mem.close().await.expect("close");
}

/// Every tool must advertise a usable object schema with `agent_id`, or a
/// client cannot call it correctly (spec §2.2 — calls carry `agent_id`).
#[tokio::test]
async fn every_tool_schema_is_an_object_requiring_agent_id() {
    let s = server("mcp-schemas").await;
    for t in tools(&s) {
        let schema = serde_json::to_value(&*t.input_schema).unwrap();
        assert_eq!(
            schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "{} schema must be an object",
            t.name
        );
        assert!(
            schema["properties"].get("agent_id").is_some(),
            "{} must take agent_id",
            t.name
        );
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        assert!(
            required.iter().any(|r| r == "agent_id"),
            "{} must REQUIRE agent_id, not merely accept it",
            t.name
        );
        assert!(
            t.description.as_ref().is_some_and(|d| !d.is_empty()),
            "{} must carry a description — it is what the model routes on",
            t.name
        );
    }
    s.mem.close().await.expect("close");
}

/// **F18 (P6 carryover), pinned.** No tool may accept a client *flush*
/// timestamp. `event_time` is the deliberate exception on derive and
/// record_action: historical about-time, not observed-at time.
///
/// This asserts on the *published schema*, so it fails for a future agent
/// who adds a timestamp field to any params struct.
#[tokio::test]
async fn f18_no_tool_schema_accepts_a_client_timestamp() {
    let s = server("mcp-f18").await;
    const BANNED: &[&str] = &[
        "timestamp",
        "created_at",
        "createdat",
        "now",
        "time",
        "when",
        "date",
        "occurred_at",
        "logical_time",
    ];
    for t in tools(&s) {
        let schema = serde_json::to_value(&*t.input_schema).unwrap();
        for path in schema_property_paths(&schema) {
            let leaf = path.rsplit('.').next().unwrap_or(&path).to_lowercase();
            let leaf = leaf.trim_end_matches("[]").to_string();
            assert!(
                !BANNED.contains(&leaf.as_str()),
                "F18: tool {} accepts '{}' — created_at and flush timestamps are \
                     stamped server-side; only explicit historical event_time is client-supplied",
                t.name,
                path
            );
        }
    }
    s.mem.close().await.expect("close");
}

/// Collect **every** property path in a published schema, following `$ref`
/// into `$defs`, `items` into array element schemas, `additionalProperties`
/// into map values and the `allOf`/`anyOf`/`oneOf` combinators.
///
/// R1/T82-4: the F18 guard used to read only the **root** `properties` map,
/// so a `created_at` added to `WireConcept` — which `lambo_derive` publishes
/// through `properties.concepts.items.$ref` → `$defs.WireConcept` — passed
/// the entire suite. Mutation-verified: adding that field now fails this
/// test and `f18_tool_schemas_match_the_golden_property_set` below.
fn schema_property_paths(schema: &serde_json::Value) -> Vec<String> {
    fn walk(
        node: &serde_json::Value,
        prefix: &str,
        root: &serde_json::Value,
        depth: usize,
        out: &mut Vec<String>,
    ) {
        // `$defs` are acyclic here, but a recursive wire type would be a
        // legitimate future shape — bound the walk rather than hang.
        if depth > 16 {
            return;
        }
        if let Some(r) = node.get("$ref").and_then(|v| v.as_str())
            && let Some(name) = r.strip_prefix("#/$defs/")
            && let Some(target) = root.get("$defs").and_then(|d| d.get(name))
        {
            walk(target, prefix, root, depth + 1, out);
        }
        if let Some(props) = node.get("properties").and_then(|v| v.as_object()) {
            for (k, v) in props {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                out.push(path.clone());
                walk(v, &path, root, depth + 1, out);
            }
        }
        if let Some(items) = node.get("items") {
            walk(items, &format!("{prefix}[]"), root, depth + 1, out);
        }
        if let Some(ap) = node.get("additionalProperties")
            && ap.is_object()
        {
            walk(ap, &format!("{prefix}.*"), root, depth + 1, out);
        }
        for key in ["allOf", "anyOf", "oneOf"] {
            if let Some(arr) = node.get(key).and_then(|v| v.as_array()) {
                for sub in arr {
                    walk(sub, prefix, root, depth + 1, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(schema, "", schema, 0, &mut out);
    out.sort();
    out.dedup();
    out
}

/// **F18 as an allowlist, not a denylist** (R1/T82-4).
///
/// A denylist of nine spellings is not a statement about client-supplied
/// logical time: `ts`, `as_of` and `client_clock_ms` all sail through one.
/// This pins the exact set of property paths every tool publishes, so
/// *any* new field on *any* tool — nested or not, however named — fails
/// here and forces a human to decide whether it is a timestamp in
/// disguise. Update it deliberately, and only after answering that.
#[tokio::test]
async fn f18_tool_schemas_match_the_golden_property_set() {
    let s = server("mcp-f18-golden").await;
    let golden: std::collections::BTreeMap<&str, Vec<&str>> = [
        (
            "lambo_derive",
            vec![
                "agent_id",
                "concepts",
                "concepts[].concept_type",
                "concepts[].content",
                "event_time",
                "parent_of",
                "parent_of[].child",
                "parent_of[].parent",
            ],
        ),
        ("lambo_inspect", vec!["agent_id", "depth", "focus"]),
        (
            "lambo_recall",
            vec![
                "agent_id",
                "max_tokens",
                "query",
                "top_k",
                "traversal_depth",
            ],
        ),
        (
            "lambo_record_action",
            vec![
                "action",
                "agent_id",
                "depends_on",
                "event_time",
                "modifies",
                "produces",
            ],
        ),
        (
            "lambo_reserve",
            vec!["agent_id", "node_id", "release", "ttl_seconds"],
        ),
        ("lambo_saints", vec!["agent_id"]),
        ("lambo_stats", vec!["agent_id", "receipt", "wait_ms"]),
    ]
    .into_iter()
    .collect();

    for t in tools(&s) {
        let schema = serde_json::to_value(&*t.input_schema).unwrap();
        let found = schema_property_paths(&schema);
        let expected = golden
            .get(t.name.as_ref())
            .unwrap_or_else(|| panic!("no golden property set for tool {}", t.name));
        assert_eq!(
            found,
            expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "tool {} publishes a different property set than the golden one. If the \
                 change is intended, confirm no new field carries client-supplied logical \
                 time (F18) and then update the golden set.",
            t.name
        );
    }
    s.mem.close().await.expect("close");
}

/// `event_time` is a deliberately narrow historical-evidence surface:
/// optional on the two writes, RFC3339 on the wire, and described as
/// about-time rather than a caller-controlled flush clock.
#[tokio::test]
async fn write_tool_schemas_document_optional_rfc3339_event_time() {
    let s = server("mcp-event-time-schema").await;
    for name in ["lambo_derive", "lambo_record_action"] {
        let tool = tools(&s)
            .into_iter()
            .find(|t| t.name == name)
            .expect("write tool is published");
        let schema = serde_json::to_value(&*tool.input_schema).expect("schema");
        let event_time = &schema["properties"]["event_time"];
        assert_eq!(event_time["format"], json!("date-time"), "{name}");
        assert!(
            schema["required"]
                .as_array()
                .is_none_or(|required| !required.iter().any(|p| p == "event_time")),
            "event_time must remain optional: {schema}"
        );
        let description = event_time["description"].as_str().unwrap_or_default();
        assert!(
            description.contains("historical about-time") && description.contains("live fact"),
            "{name} must explain historical and live semantics: {description}"
        );
    }
    s.mem.close().await.expect("close");
}

/// **T88-H1 pinned.** Nothing a client reads may carry internal notes.
///
/// `WireConceptType`'s rustdoc was published verbatim as its JSON-Schema
/// `description` in every `tools/list` response, and it carried a review
/// marker ("Byte-echo note (R4 nit)"), a dependency's internals (rmcp's
/// `Parameters<T>` extractor), an internal helper name (`validate_size`) and
/// a "revisit if…" note. Every MCP client and every model saw it.
///
/// The trap is that rustdoc on these types is *simultaneously* developer
/// documentation and wire copy, and nothing in the type system says so — the
/// next person to explain a subtlety in a `///` above a params field
/// republishes it to the world. This guard covers the whole published
/// surface (tool descriptions **and** every schema string) so that mistake
/// fails here instead of shipping.
#[tokio::test]
async fn published_schemas_carry_no_internal_notes() {
    let s = server("mcp-wire-hygiene").await;
    // Distinctive enough not to collide with legitimate wire copy: each is
    // a review marker, a dependency internal, an internal symbol, or a
    // note-to-self that has no meaning to a client.
    const MARKERS: &[&str] = &[
        "rmcp",
        "revisit",
        "spec §",
        "t82-",
        "r1/",
        "r4 nit",
        "byte-echo",
        "handoff log",
        "validate_size",
        "todo",
        "fixme",
        "xxx",
    ];

    for t in tools(&s) {
        // The tool description and the full input schema — every string a
        // client can read, not just the ones we remembered to check.
        let mut published = serde_json::to_string(&*t.input_schema).expect("schema to json");
        if let Some(d) = &t.description {
            published.push(' ');
            published.push_str(d);
        }
        let haystack = published.to_lowercase();
        for m in MARKERS {
            assert!(
                !haystack.contains(m),
                "tool {} publishes internal note marker {m:?} to every MCP client. \
                     Rustdoc on a params struct/field/enum in this module becomes the \
                     JSON-Schema description on the wire — keep engineering rationale in a \
                     plain `//` comment (T88-H1). Offending text: {published}",
                t.name,
            );
        }
    }
    s.mem.close().await.expect("close");
}

/// **T88-H4 pinned.** Published schemas carry the runtime's enforceable
/// maxima so a client can pre-validate, and `top_k`'s published minimum is
/// corrected from `0` (which the runtime refuses) to `1`.
///
/// Two properties are pinned end-to-end: every **integer** field carries
/// both a `minimum` and a `maximum` (the audit found none did), and every
/// **string** field (including array entries) carries `maxLength` equal to
/// the runtime's per-string cap. The exact bounds per field are asserted
/// too, so a future widening of a cap is a deliberate, explicit change
/// here rather than a silent drift.
#[tokio::test]
async fn published_schemas_carry_runtime_maxima() {
    let s = server("mcp-maxima").await;
    // (tool, field path as `schema_property_paths` renders it, min, max).
    let integer_bounds: &[(&str, &str, i64, i64)] = &[
        ("lambo_recall", "max_tokens", 1, 100_000),
        ("lambo_recall", "top_k", 1, 100),
        ("lambo_recall", "traversal_depth", 0, 5),
        ("lambo_inspect", "depth", 0, 5),
        ("lambo_reserve", "ttl_seconds", 1, 3_600),
        // J3. The literal in `StatsParams` must BE `RECEIPT_WAIT_MAX`:
        // schemars takes a literal, so this is the only thing keeping the
        // published maximum and the enforced one the same number.
        (
            "lambo_stats",
            "wait_ms",
            0,
            crate::writeq::RECEIPT_WAIT_MAX.as_millis() as i64,
        ),
    ];

    for t in tools(&s) {
        let schema = serde_json::to_value(&*t.input_schema).unwrap();
        let leaves = schema_leaves(&schema);
        // Every integer-typed leaf must be bounded — nothing unbounded on
        // the wire that the runtime caps (`top_k` 1..=100 etc.).
        for (path, node) in &leaves {
            if type_includes(node, "integer") {
                assert!(
                    node.get("minimum").is_some() && node.get("maximum").is_some(),
                    "tool {} integer field {path:?} must publish both minimum and maximum \
                         (T88-H4): {}",
                    t.name,
                    node
                );
            }
            if type_includes(node, "string") && node.get("enum").is_none() {
                assert_eq!(
                    node.get("maxLength").and_then(|v| v.as_u64()),
                    Some(16_384),
                    "tool {} string field {path:?} must publish maxLength 16384 matching \
                         the runtime per-string cap (T88-H4): {}",
                    t.name,
                    node
                );
            }
        }
        // Exact bounds for the fields the audit named.
        for &(tool, path, min, max) in integer_bounds {
            if tool == t.name.as_ref() {
                let n = leaves
                    .iter()
                    .find(|(p, _)| p == path)
                    .map(|(_, n)| n)
                    .unwrap_or_else(|| panic!("tool {} missing integer field {path:?}", t.name));
                assert_eq!(
                    n.get("minimum").and_then(|v| v.as_i64()),
                    Some(min),
                    "tool {} {path:?} minimum",
                    t.name
                );
                assert_eq!(
                    n.get("maximum").and_then(|v| v.as_i64()),
                    Some(max),
                    "tool {} {path:?} maximum",
                    t.name
                );
            }
        }
    }
    s.mem.close().await.expect("close");
}

/// True when a JSON-Schema node's `type` names the kind — `type` may be a
/// bare string ("string", "integer") or an array of types, as schemars
/// emits for an `Option<T>` (`["integer","null"]`).
fn type_includes(node: &serde_json::Value, kind: &str) -> bool {
    match node.get("type") {
        Some(serde_json::Value::String(s)) => s == kind,
        Some(serde_json::Value::Array(a)) => a.iter().any(|v| v.as_str() == Some(kind)),
        _ => false,
    }
}

/// Collect every primitive leaf `(path, node)` in a published schema,
/// following `$ref` into `$defs`, `items` into array elements and
/// `properties` into nested objects — the same walk
/// [`schema_property_paths`] does, but keeping the leaf **node** so its
/// bounds and `maxLength` can be asserted.
fn schema_leaves(schema: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    fn walk(
        node: &serde_json::Value,
        root: &serde_json::Value,
        prefix: &str,
        out: &mut Vec<(String, serde_json::Value)>,
    ) {
        let node = match node.get("$ref").and_then(|v| v.as_str()) {
            Some(r) if r.starts_with("#/$defs/") => root
                .get("$defs")
                .and_then(|d| d.get(&r["#/$defs/".len()..]))
                .unwrap_or(node),
            _ => node,
        };
        let has_children = node.get("properties").is_some() || node.get("items").is_some();
        if node.get("type").is_some() && !has_children {
            out.push((prefix.to_string(), node.clone()));
            return;
        }
        if let Some(props) = node.get("properties").and_then(|v| v.as_object()) {
            for (k, v) in props {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                walk(v, root, &path, out);
            }
        }
        if let Some(items) = node.get("items") {
            walk(items, root, &format!("{prefix}[]"), out);
        }
    }
    let mut out = Vec::new();
    walk(schema, schema, "", &mut out);
    out
}

/// The walker must actually descend — a guard that only ever sees the root
/// is the bug R1/T82-4 found, and it looks identical from the outside.
#[test]
fn the_schema_walker_reaches_nested_and_referenced_properties() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "concepts": { "type": "array", "items": { "$ref": "#/$defs/Wire" } },
            "bag": { "additionalProperties": { "properties": { "deep": {} } } }
        },
        "$defs": { "Wire": { "properties": { "created_at": {}, "content": {} } } }
    });
    let paths = schema_property_paths(&schema);
    assert!(
        paths.contains(&"concepts[].created_at".to_string()),
        "a $ref'd nested property must be reachable, got {paths:?}"
    );
    assert!(paths.contains(&"bag.*.deep".to_string()), "{paths:?}");
}

/// Every name the router publishes must be drivable by `call` above —
/// otherwise these tests could silently stop covering a renamed tool.
#[tokio::test]
async fn every_published_tool_name_is_exercised_by_the_test_harness() {
    let s = server("mcp-harness").await;
    for t in tools(&s) {
        assert!(
            matches!(
                t.name.as_ref(),
                "lambo_recall"
                    | "lambo_derive"
                    | "lambo_record_action"
                    | "lambo_reserve"
                    | "lambo_inspect"
                    | "lambo_saints"
                    | "lambo_stats"
            ),
            "published tool {} has no harness arm",
            t.name
        );
    }
    s.mem.close().await.expect("close");
}

/// **R1/T82-11 pinned.** An unknown field is refused, not silently
/// discarded — including a `created_at` on a *nested* wire type. A client
/// that believes it backdated an interaction must be told it did not.
#[test]
fn unknown_fields_are_refused_by_every_params_struct() {
    let ts = "1999-01-01T00:00:00Z";
    assert!(serde_json::from_value::<DeriveParams>(serde_json::json!({
        "agent_id": "a", "concepts": [], "created_at": ts
    }))
    .is_err());
    assert!(serde_json::from_value::<DeriveParams>(serde_json::json!({
        "agent_id": "a",
        "concepts": [{"content": "x", "concept_type": "entity", "created_at": ts}]
    }))
    .is_err());
    assert!(serde_json::from_value::<RecallParams>(serde_json::json!({
        "agent_id": "a", "query": "x", "ts": 1
    }))
    .is_err());
    assert!(
        serde_json::from_value::<RecordActionParams>(serde_json::json!({
            "agent_id": "a", "action": "x", "as_of": ts
        }))
        .is_err()
    );
    assert!(serde_json::from_value::<ReserveParams>(serde_json::json!({
        "agent_id": "a", "node_id": "x", "client_clock_ms": 1
    }))
    .is_err());
    // …and the legitimate shapes still parse.
    assert!(serde_json::from_value::<DeriveParams>(serde_json::json!({
        "agent_id": "a", "concepts": [{"content": "x", "concept_type": "entity"}]
    }))
    .is_ok());
    assert!(serde_json::from_value::<DeriveParams>(serde_json::json!({
        "agent_id": "a",
        "concepts": [{"content": "x", "concept_type": "entity"}],
        "event_time": "2020-01-01T00:00:00Z"
    }))
    .is_ok());
    assert!(
        serde_json::from_value::<RecordActionParams>(serde_json::json!({
            "agent_id": "a", "action": "x", "event_time": "2020-01-01T00:00:00Z"
        }))
        .is_ok()
    );
    assert!(serde_json::from_value::<DeriveParams>(serde_json::json!({
        "agent_id": "a",
        "concepts": [{"content": "x", "concept_type": "entity"}],
        "event_time": "not-rfc3339"
    }))
    .is_err());
}

#[tokio::test]
async fn get_info_advertises_tools_and_names_the_session() {
    let s = server("mcp-info").await;
    let info = s.get_info();
    assert!(
        info.capabilities.tools.is_some(),
        "tools capability must be advertised"
    );
    assert_eq!(info.server_info.name, "lambo");
    let instructions = info.instructions.expect("instructions");
    assert!(instructions.contains("mcp-info"));
    assert!(
        instructions.contains("created_at is server-stamped")
            && instructions.contains("optional RFC3339 event_time")
            && instructions.contains("historical about-time")
            && instructions.contains("omit it for a live fact"),
        "instructions must distinguish server-stamped flush time from optional historical \
             event time: {instructions}"
    );
    assert!(
        !instructions.contains("Never send a timestamp"),
        "the obsolete blanket timestamp prohibition must not suppress event_time: {instructions}"
    );
    assert!(
        instructions.contains("Ordering is yours to manage"),
        "instructions should tell the model that write-then-read ordering is its \
             responsibility (N7)"
    );
    // J3 made the old wording FALSE: a write's tool call returning no
    // longer means the write is visible. The instructions are the one place
    // every model reads before it acts, so they must name the receipt and
    // the way to wait on it.
    assert!(
        instructions.contains("applied in the BACKGROUND"),
        "instructions must say writes are asynchronous (J3): {instructions}"
    );
    assert!(
        instructions.contains("receipt"),
        "instructions must name the receipt: {instructions}"
    );
    assert!(
        instructions.contains("wait_ms"),
        "instructions must say how to wait for a write (J3): {instructions}"
    );
    assert!(
        !instructions.contains("a read sees a write only after that write's own tool call"),
        "the pre-J3 ordering sentence is now false and must not survive: {instructions}"
    );
    // J3-R1-10: the ordering promise is scoped to SEQUENTIAL submissions,
    // which is what an agent can actually assert. Two concurrent calls from
    // one agent pin their chain positions and their lane positions in two
    // different critical sections, so the two orders can disagree — and for
    // genuinely concurrent calls the caller has no order to preserve
    // anyway.
    assert!(
        instructions.contains("one after another are applied in that order"),
        "the ordering promise must state its scope: {instructions}"
    );
    assert!(
        !instructions.contains("your writes are applied in the order you sent them"),
        "the unscoped ordering promise is stronger than what holds: {instructions}"
    );
    s.mem.close().await.expect("close");
}
