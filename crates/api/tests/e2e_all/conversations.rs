use crate::common::*;
use axum::http::Method;

#[tokio::test]
async fn retired_conversation_routes_require_an_api_key_then_return_gone() {
    let server = setup_test_server().await;

    let missing_auth = server.post("/v1/conversations").await;
    assert_eq!(missing_auth.status_code(), 401);

    let invalid_auth = server
        .post("/v1/conversations")
        .add_header("Authorization", "Bearer sk-invalid")
        .await;
    assert_eq!(invalid_auth.status_code(), 401);

    let (api_key, _) = create_org_and_api_key(&server).await;
    let routes = [
        (Method::POST, "/v1/conversations"),
        (Method::GET, "/v1/conversations/"),
        (Method::POST, "/v1/conversations/batch"),
        (Method::GET, "/v1/conversations/conv_example"),
        (Method::POST, "/v1/conversations/conv_example"),
        (Method::DELETE, "/v1/conversations/conv_example"),
        (Method::POST, "/v1/conversations/conv_example/pin"),
        (Method::DELETE, "/v1/conversations/conv_example/pin"),
        (Method::POST, "/v1/conversations/conv_example/archive"),
        (Method::DELETE, "/v1/conversations/conv_example/archive"),
        (Method::POST, "/v1/conversations/conv_example/clone"),
        (Method::GET, "/v1/conversations/conv_example/items"),
        (Method::POST, "/v1/conversations/conv_example/items"),
        (Method::PATCH, "/v1/conversations/conv_example/unknown"),
    ];

    for (method, path) in routes {
        let response = server
            .method(method.clone(), path)
            .add_header("Authorization", format!("Bearer {api_key}"))
            .await;

        assert_eq!(
            response.status_code(),
            410,
            "{method} {path} must return 410 Gone after authentication"
        );

        let error = response.json::<api::models::ErrorResponse>();
        assert_eq!(error.error.r#type, "gone");
        assert_eq!(
            error.error.code.as_deref(),
            Some("conversation_api_retired")
        );
        assert!(error.error.message.contains("POST /v1/responses"));
    }
}
