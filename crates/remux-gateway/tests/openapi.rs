//! OpenAPI 3.1 spec tests (T0.5).
//!
//! - Asserts the generated spec is OpenAPI 3.1 and contains the expected paths.
//! - A **drift test** asserts the committed `docs/openapi.yaml` matches the spec
//!   regenerated in-memory. If it drifts, the test fails with a message telling
//!   you to regenerate.
//!
//! ## Regenerating the committed spec
//!
//! ```text
//! UPDATE_OPENAPI=1 cargo test -p remux-gateway --test openapi
//! ```
//!
//! With `UPDATE_OPENAPI=1` set, the drift test (re)writes `docs/openapi.yaml`
//! instead of asserting, then passes.

use std::path::PathBuf;

use remux_gateway::api::v1::openapi;

/// Absolute path to the committed `docs/openapi.yaml` (repo root is two levels up
/// from this crate's manifest dir).
fn committed_yaml_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent() // crates/
        .and_then(|p| p.parent()) // repo root
        .expect("repo root from crate manifest dir")
        .join("docs")
        .join("openapi.yaml")
}

#[test]
fn generated_spec_is_openapi_3_1() {
    let doc = openapi::api_doc();
    let version = doc["openapi"].as_str().expect("openapi version string");
    assert!(
        version.starts_with("3.1"),
        "expected OpenAPI 3.1.x, got {version}"
    );
    // A known path is present.
    assert!(
        doc["paths"]["/v1/sessions"].is_object(),
        "/v1/sessions path missing"
    );
    assert!(
        doc["paths"]["/v1/sessions/{id}/screen"].is_object(),
        "/v1/sessions/{{id}}/screen path missing"
    );
    // Bearer security scheme.
    assert!(
        doc["components"]["securitySchemes"]["bearer"].is_object(),
        "bearer security scheme missing"
    );
    // The error shape is a component.
    assert!(
        doc["components"]["schemas"]["ApiErrorBody"].is_object(),
        "ApiErrorBody schema missing"
    );
}

#[test]
fn committed_openapi_yaml_is_in_sync() {
    let generated = openapi::api_doc_yaml();
    let path = committed_yaml_path();

    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(&path, &generated)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
        eprintln!("UPDATE_OPENAPI=1: wrote {}", path.display());
        return;
    }

    let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "could not read committed spec {}: {e}\n\
             Regenerate it with: UPDATE_OPENAPI=1 cargo test -p remux-gateway --test openapi",
            path.display()
        )
    });

    assert_eq!(
        committed.trim_end(),
        generated.trim_end(),
        "\n\ndocs/openapi.yaml is out of sync with the generated OpenAPI spec.\n\
         Regenerate it with:\n\n    UPDATE_OPENAPI=1 cargo test -p remux-gateway --test openapi\n"
    );
}
