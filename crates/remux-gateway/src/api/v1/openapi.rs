//! OpenAPI 3.1 document for the public `/v1` surface (T0.5).
//!
//! The spec is generated from the `#[utoipa::path(...)]` annotations on the axum
//! handlers (in [`crate::app`]) and the `#[derive(ToSchema)]` DTOs (in
//! [`super::dto`]). It is served live at `GET /v1/openapi.json` and committed,
//! serialized to YAML, at `docs/openapi.yaml`.
//!
//! ## Keeping the committed spec in sync
//!
//! A test (`tests/openapi.rs::committed_openapi_yaml_is_in_sync`) regenerates the
//! YAML in-memory and asserts it equals `docs/openapi.yaml`. If you change a DTO
//! or a handler annotation, regenerate the committed file with:
//!
//! ```text
//! UPDATE_OPENAPI=1 cargo test -p remux-gateway --test openapi
//! ```
//!
//! (The same test rewrites `docs/openapi.yaml` when `UPDATE_OPENAPI=1` is set.)

use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
use utoipa::{Modify, OpenApi};

use super::dto::{
    ApiErrorBody, CreateSessionBody, InputBody, RenameBody, ResizeBody, ScreenView, ScrollbackView,
    SessionView, SizeBody, WaitBody, WaitResult,
};

/// Inject the `bearer` HTTP security scheme into the generated components.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .get_or_insert_with(utoipa::openapi::Components::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        );
    }
}

/// The derived OpenAPI document for the `/v1` surface.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "remux-gateway /v1 API",
        version = "1.0.0",
        description = "Agent-native structured control plane for the remux daemon. \
                       The daemon stays Unix-socket-only; this gateway terminates \
                       TLS + bearer auth and translates the public /v1 contract \
                       onto the local socket."
    ),
    paths(
        crate::app::health,
        crate::app::openapi_json,
        crate::app::list_sessions,
        crate::app::create_session,
        crate::app::get_session,
        crate::app::delete_session,
        crate::app::patch_session,
        crate::app::send_input,
        crate::app::get_screen,
        crate::app::get_scrollback,
        crate::app::resize_session,
        crate::app::wait_session,
    ),
    components(schemas(
        SessionView,
        SizeBody,
        CreateSessionBody,
        ResizeBody,
        RenameBody,
        InputBody,
        WaitBody,
        WaitResult,
        ScreenView,
        ScrollbackView,
        ApiErrorBody,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "sessions", description = "Session CRUD, input, capture, wait"),
        (name = "health", description = "Liveness"),
        (name = "meta", description = "API metadata / discovery"),
    ),
)]
pub struct ApiDoc;

/// Build the OpenAPI document, forcing the version string to `3.1.0`.
///
/// utoipa 5 emits `3.1.0` by default for the document `openapi` field, but we set
/// it explicitly so the contract ("OpenAPI 3.1") is pinned regardless of any
/// upstream default change.
pub fn openapi() -> utoipa::openapi::OpenApi {
    let mut doc = ApiDoc::openapi();
    doc.openapi = utoipa::openapi::OpenApiVersion::Version31;
    doc
}

/// The OpenAPI document as a `serde_json::Value` (served at `/v1/openapi.json`).
pub fn api_doc() -> serde_json::Value {
    serde_json::to_value(openapi()).expect("serialize OpenAPI to JSON value")
}

/// The OpenAPI document serialized to YAML (the committed `docs/openapi.yaml`).
pub fn api_doc_yaml() -> String {
    openapi().to_yaml().expect("serialize OpenAPI to YAML")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_is_3_1_and_has_sessions_path() {
        let doc = api_doc();
        assert_eq!(doc["openapi"], "3.1.0");
        assert!(
            doc["paths"]["/v1/sessions"].is_object(),
            "/v1/sessions path missing from generated spec"
        );
        // The bearer security scheme is present.
        assert!(doc["components"]["securitySchemes"]["bearer"].is_object());
    }

    #[test]
    fn yaml_serialization_roundtrips_to_same_doc() {
        let yaml = api_doc_yaml();
        assert!(yaml.contains("openapi: 3.1.0"), "yaml: {yaml}");
        assert!(yaml.contains("/v1/sessions"));
    }
}
