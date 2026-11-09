use super::*;
use tokio::io::AsyncWriteExt;

pub(crate) async fn database_error(code: &str, constraint: Option<&str>) -> tokio_postgres::Error {
    let mut body = format!("SERROR\0C{code}\0Mtest database error\0").into_bytes();
    if let Some(constraint) = constraint {
        body.extend_from_slice(format!("n{constraint}\0").as_bytes());
    }
    body.push(0);
    let mut message = vec![b'E'];
    message.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
    message.extend_from_slice(&body);
    let (stream, mut peer) = tokio::io::duplex(4096);
    peer.write_all(&message).await.unwrap();
    let result = tokio_postgres::Config::new()
        .user("test")
        .ssl_mode(tokio_postgres::config::SslMode::Disable)
        .connect_raw(stream, tokio_postgres::NoTls)
        .await;
    match result {
        Ok(_) => panic!("PostgreSQL error frame must reject startup"),
        Err(error) => error,
    }
}

#[tokio::test]
async fn unique_job_insert_maps_to_conflict_without_relying_on_a_constraint_name() {
    for constraint in [None, Some("renamed_active_scope_index")] {
        let error = database_error("23505", constraint).await;
        let (status, body) = job_insert_error(error);
        assert_eq!(status, StatusCode::CONFLICT, "constraint={constraint:?}");
        assert_eq!(body["error"]["code"], "conflict");
        assert_eq!(
            body["error"]["message"],
            "a database encryption job with this scope is already queued or running"
        );
    }
}

#[tokio::test]
async fn other_job_insert_database_errors_remain_internal_errors() {
    let error = database_error("23503", Some("idx_database_encryption_jobs_active_scope")).await;
    assert_eq!(job_insert_error(error).0, StatusCode::INTERNAL_SERVER_ERROR);
}
