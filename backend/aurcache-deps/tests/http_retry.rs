//! How a request to the AUR rides out a bad answer.
//!
//! The AUR's gateway answers 502 now and then, and a version check used to
//! give up its whole pass on the first one, logging every package of the
//! batch in the URL as it did.

use aurcache_deps::AurClient;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client_for(server: &MockServer) -> AurClient {
    AurClient::with_urls(format!("{}/rpc/v5", server.uri()))
}

#[tokio::test]
async fn a_bad_gateway_is_retried() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(502))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"snapshot".to_vec()))
        .mount(&server)
        .await;

    let bytes = client_for(&server)
        .download_snapshot_bytes("hello")
        .await
        .expect("the second attempt succeeds");

    assert_eq!(bytes, b"snapshot");
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_not_found_is_an_answer_and_not_retried() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    client_for(&server)
        .download_snapshot_bytes("hello")
        .await
        .expect_err("a 404 fails");

    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_failed_query_does_not_list_its_packages() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;

    let error = client_for(&server)
        .multi_info_of(&["first-package", "second-package"])
        .await
        .expect_err("every attempt fails");

    let message = error.to_string();
    assert!(message.contains("502"), "{message}");
    assert!(message.contains("/rpc/v5/info"), "{message}");
    assert!(!message.contains("first-package"), "{message}");
}
