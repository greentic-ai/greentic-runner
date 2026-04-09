#[cfg(target_arch = "wasm32")]
use std::collections::BTreeMap;

#[cfg(target_arch = "wasm32")]
use greentic_interfaces_guest::component_v0_6::node;
#[cfg(target_arch = "wasm32")]
use greentic_types::cbor::canonical;
#[cfg(target_arch = "wasm32")]
use greentic_types::schemas::common::schema_ir::{AdditionalProperties, SchemaIr};
#[cfg(target_arch = "wasm32")]
use greentic_types::schemas::component::v0_6_0::{ComponentInfo, I18nText};

#[cfg(target_arch = "wasm32")]
wit_bindgen::generate!({
    path: "wit",
    world: "component-v0-v6-v0",
    generate_all,
});

// i18n: runtime lookup + embedded CBOR bundle helpers.
pub mod i18n;
pub mod i18n_bundle;
// qa: mode normalization, QA spec generation, apply-answers validation.
pub mod qa;

const COMPONENT_NAME: &str = "component-v0-6-dummy";
#[cfg(target_arch = "wasm32")]
const COMPONENT_ORG: &str = "com.example";
#[cfg(target_arch = "wasm32")]
const COMPONENT_VERSION: &str = "0.1.0";

#[cfg(target_arch = "wasm32")]
#[used]
#[unsafe(link_section = ".greentic.wasi")]
static WASI_TARGET_MARKER: [u8; 13] = *b"wasm32-wasip2";

#[cfg(target_arch = "wasm32")]
struct Component;

#[cfg(target_arch = "wasm32")]
impl node::Guest for Component {
    // Component metadata advertised to host/operator tooling.
    // Extend here when you add more operations or capability declarations.
    fn describe() -> node::ComponentDescriptor {
        let input_schema_cbor = input_schema_cbor();
        let output_schema_cbor = output_schema_cbor();
        let mut ops = vec![node::Op {
            name: "run".to_string(),
            summary: Some("Handle a single message input".to_string()),
            input: node::IoSchema {
                schema: node::SchemaSource::InlineCbor(input_schema_cbor.clone()),
                content_type: "application/cbor".to_string(),
                schema_version: None,
            },
            output: node::IoSchema {
                schema: node::SchemaSource::InlineCbor(output_schema_cbor.clone()),
                content_type: "application/cbor".to_string(),
                schema_version: None,
            },
            examples: Vec::new(),
        }];
        ops.extend(vec![
            node::Op {
                name: "qa-spec".to_string(),
                summary: Some("Return QA spec for requested mode".to_string()),
                input: node::IoSchema {
                    schema: node::SchemaSource::InlineCbor(input_schema_cbor.clone()),
                    content_type: "application/cbor".to_string(),
                    schema_version: None,
                },
                output: node::IoSchema {
                    schema: node::SchemaSource::InlineCbor(output_schema_cbor.clone()),
                    content_type: "application/cbor".to_string(),
                    schema_version: None,
                },
                examples: Vec::new(),
            },
            node::Op {
                name: "apply-answers".to_string(),
                summary: Some("Apply QA answers and optionally return config override".to_string()),
                input: node::IoSchema {
                    schema: node::SchemaSource::InlineCbor(input_schema_cbor.clone()),
                    content_type: "application/cbor".to_string(),
                    schema_version: None,
                },
                output: node::IoSchema {
                    schema: node::SchemaSource::InlineCbor(output_schema_cbor.clone()),
                    content_type: "application/cbor".to_string(),
                    schema_version: None,
                },
                examples: Vec::new(),
            },
            node::Op {
                name: "i18n-keys".to_string(),
                summary: Some("Return i18n keys referenced by QA/setup".to_string()),
                input: node::IoSchema {
                    schema: node::SchemaSource::InlineCbor(input_schema_cbor.clone()),
                    content_type: "application/cbor".to_string(),
                    schema_version: None,
                },
                output: node::IoSchema {
                    schema: node::SchemaSource::InlineCbor(output_schema_cbor),
                    content_type: "application/cbor".to_string(),
                    schema_version: None,
                },
                examples: Vec::new(),
            },
        ]);
        node::ComponentDescriptor {
            name: COMPONENT_NAME.to_string(),
            version: COMPONENT_VERSION.to_string(),
            summary: Some(format!("Greentic component {COMPONENT_NAME}")),
            capabilities: Vec::new(),
            ops,
            schemas: Vec::new(),
            setup: None,
        }
    }

    // Single ABI entrypoint. Keep this dispatcher model intact.
    // Extend behavior by adding/adjusting operation branches in `run_component_cbor`.
    fn invoke(
        operation: String,
        envelope: node::InvocationEnvelope,
    ) -> Result<node::InvocationResult, node::NodeError> {
        let output = run_component_cbor(&operation, envelope.payload_cbor);
        Ok(node::InvocationResult {
            ok: true,
            output_cbor: output,
            output_metadata_cbor: None,
        })
    }
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct CabiList {
    ptr: *mut u8,
    len: usize,
}

#[cfg(target_arch = "wasm32")]
#[repr(C)]
struct CabiStringList {
    ptr: *mut CabiList,
    len: usize,
}

#[cfg(target_arch = "wasm32")]
static mut QA_SPEC_RET: CabiList = CabiList {
    ptr: std::ptr::null_mut(),
    len: 0,
};

#[cfg(target_arch = "wasm32")]
static mut APPLY_ANSWERS_RET: CabiList = CabiList {
    ptr: std::ptr::null_mut(),
    len: 0,
};

#[cfg(target_arch = "wasm32")]
static mut I18N_KEYS_RET: CabiStringList = CabiStringList {
    ptr: std::ptr::null_mut(),
    len: 0,
};

#[cfg(target_arch = "wasm32")]
fn cabi_mode(mode: i32) -> qa::NormalizedMode {
    match mode {
        0 | 1 => qa::NormalizedMode::Setup,
        2 => qa::NormalizedMode::Update,
        3 => qa::NormalizedMode::Remove,
        _ => qa::NormalizedMode::Setup,
    }
}

#[cfg(target_arch = "wasm32")]
unsafe fn export_vec_bytes(bytes: Vec<u8>, ret: *mut CabiList) -> *mut u8 {
    let boxed = bytes.into_boxed_slice();
    let ptr = boxed.as_ptr() as *mut u8;
    let len = boxed.len();
    std::mem::forget(boxed);
    unsafe {
        (*ret).ptr = ptr;
        (*ret).len = len;
        ret.cast()
    }
}

#[cfg(target_arch = "wasm32")]
unsafe fn post_return_vec_bytes(arg0: *mut u8) {
    let ret = unsafe { &*(arg0.cast::<CabiList>()) };
    if ret.len == 0 || ret.ptr.is_null() {
        return;
    }
    let layout = std::alloc::Layout::array::<u8>(ret.len).expect("byte layout");
    unsafe {
        std::alloc::dealloc(ret.ptr, layout);
    }
}

#[cfg(target_arch = "wasm32")]
unsafe fn export_i18n_keys_list(keys: Vec<String>) -> *mut u8 {
    let len = keys.len();
    let layout = std::alloc::Layout::array::<CabiList>(len).expect("string list layout");
    let ptr = if layout.size() == 0 {
        std::ptr::null_mut()
    } else {
        let raw = unsafe { std::alloc::alloc(layout) }.cast::<CabiList>();
        if raw.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        raw
    };
    for (idx, key) in keys.into_iter().enumerate() {
        let boxed = key.into_bytes().into_boxed_slice();
        let item_ptr = boxed.as_ptr() as *mut u8;
        let item_len = boxed.len();
        std::mem::forget(boxed);
        unsafe {
            ptr.add(idx).write(CabiList {
                ptr: item_ptr,
                len: item_len,
            });
        }
    }
    unsafe {
        I18N_KEYS_RET.ptr = ptr;
        I18N_KEYS_RET.len = len;
        (&raw mut I18N_KEYS_RET).cast()
    }
}

#[cfg(target_arch = "wasm32")]
unsafe fn post_return_i18n_keys(arg0: *mut u8) {
    let ret = unsafe { &*(arg0.cast::<CabiStringList>()) };
    for idx in 0..ret.len {
        let item = unsafe { &*ret.ptr.add(idx) };
        if item.len == 0 || item.ptr.is_null() {
            continue;
        }
        let layout = std::alloc::Layout::array::<u8>(item.len).expect("string layout");
        unsafe {
            std::alloc::dealloc(item.ptr, layout);
        }
    }
    if ret.len == 0 || ret.ptr.is_null() {
        return;
    }
    let layout = std::alloc::Layout::array::<CabiList>(ret.len).expect("string list layout");
    unsafe {
        std::alloc::dealloc(ret.ptr.cast(), layout);
    }
}

#[cfg(target_arch = "wasm32")]
#[unsafe(export_name = "greentic:component/component-qa@0.6.0#qa-spec")]
unsafe extern "C" fn export_component_qa_spec(mode: i32) -> *mut u8 {
    let bytes = qa::qa_spec_cbor(cabi_mode(mode));
    unsafe { export_vec_bytes(bytes, &raw mut QA_SPEC_RET) }
}

#[cfg(target_arch = "wasm32")]
#[unsafe(export_name = "cabi_post_greentic:component/component-qa@0.6.0#qa-spec")]
unsafe extern "C" fn post_return_component_qa_spec(arg0: *mut u8) {
    unsafe { post_return_vec_bytes(arg0) }
}

#[cfg(target_arch = "wasm32")]
#[unsafe(export_name = "greentic:component/component-qa@0.6.0#apply-answers")]
unsafe extern "C" fn export_component_apply_answers(
    mode: i32,
    current_config_ptr: *mut u8,
    current_config_len: usize,
    answers_ptr: *mut u8,
    answers_len: usize,
) -> *mut u8 {
    let current_config =
        unsafe { Vec::from_raw_parts(current_config_ptr, current_config_len, current_config_len) };
    let answers = unsafe { Vec::from_raw_parts(answers_ptr, answers_len, answers_len) };
    let bytes = qa::apply_answers_cbor(cabi_mode(mode), &current_config, &answers);
    unsafe { export_vec_bytes(bytes, &raw mut APPLY_ANSWERS_RET) }
}

#[cfg(target_arch = "wasm32")]
#[unsafe(export_name = "cabi_post_greentic:component/component-qa@0.6.0#apply-answers")]
unsafe extern "C" fn post_return_component_apply_answers(arg0: *mut u8) {
    unsafe { post_return_vec_bytes(arg0) }
}

#[cfg(target_arch = "wasm32")]
#[unsafe(export_name = "greentic:component/component-i18n@0.6.0#i18n-keys")]
unsafe extern "C" fn export_component_i18n_keys() -> *mut u8 {
    unsafe { export_i18n_keys_list(qa::i18n_keys()) }
}

#[cfg(target_arch = "wasm32")]
#[unsafe(export_name = "cabi_post_greentic:component/component-i18n@0.6.0#i18n-keys")]
unsafe extern "C" fn post_return_component_i18n_keys(arg0: *mut u8) {
    unsafe { post_return_i18n_keys(arg0) }
}

#[cfg(target_arch = "wasm32")]
fn component_descriptor_payload_json() -> serde_json::Value {
    // The runner-host introspection logic expects CBOR-encoded JSON with:
    // - `operations`: list of objects with `name`/`id` and `input_schema`/`output_schema`
    // - `config_schema`: an object
    serde_json::json!({
        "operations": [
            {
                "name": "run",
                "input": { "schema": { "type": "object" } },
                "output": { "schema": { "type": "object" } },
                // Keep the legacy/alternate keys too.
                "input_schema": { "type": "object" },
                "output_schema": { "type": "object" }
            }
        ],
        "config_schema": { "type": "object" }
    })
}

#[cfg(target_arch = "wasm32")]
struct ComponentDescriptorExports;

#[cfg(target_arch = "wasm32")]
impl exports::greentic::component::component_descriptor::Guest for ComponentDescriptorExports {
    fn describe() -> Vec<u8> {
        encode_cbor(&component_descriptor_payload_json())
    }
}

#[cfg(target_arch = "wasm32")]
export!(ComponentDescriptorExports);

#[cfg(target_arch = "wasm32")]
greentic_interfaces_guest::export_component_v060!(Component);

// Default user-operation implementation.
// Replace this with domain logic for your component.
pub fn handle_message(operation: &str, input: &str) -> String {
    format!("{COMPONENT_NAME}::{operation} => {}", input.trim())
}

#[cfg(target_arch = "wasm32")]
fn encode_cbor<T: serde::Serialize>(value: &T) -> Vec<u8> {
    canonical::to_canonical_cbor_allow_floats(value).expect("encode cbor")
}

#[cfg(target_arch = "wasm32")]
// Accept canonical CBOR first, then fall back to JSON for local debugging.
fn parse_payload(input: &[u8]) -> serde_json::Value {
    if let Ok(value) = canonical::from_cbor(input) {
        return value;
    }
    serde_json::from_slice(input).unwrap_or_else(|_| serde_json::json!({}))
}

#[cfg(target_arch = "wasm32")]
// Keep ingress compatibility: default/setup/install -> setup, update/upgrade -> update.
fn normalized_mode(payload: &serde_json::Value) -> qa::NormalizedMode {
    let mode = payload
        .get("mode")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("operation").and_then(|v| v.as_str()))
        .unwrap_or("setup");
    qa::normalize_mode(mode).unwrap_or(qa::NormalizedMode::Setup)
}

#[cfg(target_arch = "wasm32")]
// Minimal schema for generic operation input.
// Extend these schemas when you harden operation contracts.
fn input_schema() -> SchemaIr {
    SchemaIr::Object {
        properties: BTreeMap::from([(
            "input".to_string(),
            SchemaIr::String {
                min_len: Some(0),
                max_len: None,
                regex: None,
                format: None,
            },
        )]),
        required: vec!["input".to_string()],
        additional: AdditionalProperties::Allow,
    }
}

#[cfg(target_arch = "wasm32")]
fn output_schema() -> SchemaIr {
    SchemaIr::Object {
        properties: BTreeMap::from([(
            "message".to_string(),
            SchemaIr::String {
                min_len: Some(0),
                max_len: None,
                regex: None,
                format: None,
            },
        )]),
        required: vec!["message".to_string()],
        additional: AdditionalProperties::Allow,
    }
}

#[cfg(target_arch = "wasm32")]
#[allow(dead_code)]
fn config_schema() -> SchemaIr {
    SchemaIr::Object {
        properties: BTreeMap::new(),
        required: Vec::new(),
        additional: AdditionalProperties::Forbid,
    }
}

#[cfg(target_arch = "wasm32")]
#[allow(dead_code)]
fn component_info() -> ComponentInfo {
    ComponentInfo {
        id: format!("{COMPONENT_ORG}.{COMPONENT_NAME}"),
        version: COMPONENT_VERSION.to_string(),
        role: "tool".to_string(),
        display_name: Some(I18nText::new(
            "component.display_name",
            Some(COMPONENT_NAME.to_string()),
        )),
    }
}

#[cfg(target_arch = "wasm32")]
fn input_schema_cbor() -> Vec<u8> {
    encode_cbor(&input_schema())
}

#[cfg(target_arch = "wasm32")]
fn output_schema_cbor() -> Vec<u8> {
    encode_cbor(&output_schema())
}

#[cfg(target_arch = "wasm32")]
// Central operation dispatcher.
// This is the primary extension point for new operations.
fn run_component_cbor(operation: &str, input: Vec<u8>) -> Vec<u8> {
    let value = parse_payload(&input);
    let output = match operation {
        "qa-spec" => {
            let mode = normalized_mode(&value);
            qa::qa_spec_json(mode)
        }
        "apply-answers" => {
            let mode = normalized_mode(&value);
            qa::apply_answers(mode, &value)
        }
        "i18n-keys" => serde_json::Value::Array(
            qa::i18n_keys()
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
        _ => {
            let op_name = value
                .get("operation")
                .and_then(|v| v.as_str())
                .unwrap_or(operation);
            let input_text = value
                .get("input")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| value.to_string());
            serde_json::json!({
                "message": handle_message(op_name, &input_text)
            })
        }
    };
    encode_cbor(&output)
}
