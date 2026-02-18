//! Integration tests for session persistence (M6a).
//!
//! Requires a running PostgreSQL instance. Set DATABASE_URL to run:
//!
//!   docker compose up -d
//!   DATABASE_URL=postgres://cherub:cherub_dev@localhost:5480/cherub \
//!     cargo test --features sessions session_persistence
//!
//! Tests are skipped automatically when DATABASE_URL is not set.

#[cfg(feature = "sessions")]
mod tests {
    use cherub::providers::{ContentBlock, Message, StopReason, UserContent};
    use cherub::storage::SessionStore;
    use cherub::storage::connect;
    use cherub::storage::pg_session_store::PgSessionStore;
    use secrecy::SecretString;
    use uuid::Uuid;

    /// Returns the DATABASE_URL or skips the test if unset.
    fn db_url() -> Option<String> {
        std::env::var("DATABASE_URL").ok()
    }

    macro_rules! skip_without_db {
        ($url:ident) => {
            let Some($url) = db_url() else {
                eprintln!("skipping: DATABASE_URL not set");
                return;
            };
        };
    }

    #[tokio::test]
    async fn connect_and_migrate() {
        skip_without_db!(url);
        let pool = connect(SecretString::from(url.clone()))
            .await
            .expect("connect");
        drop(pool);
    }

    #[tokio::test]
    async fn get_or_create_session_creates_new() {
        skip_without_db!(url);
        let pool = connect(SecretString::from(url.clone()))
            .await
            .expect("connect");
        let store = PgSessionStore::new(pool);

        // Use a unique connector_id per test run to avoid state bleed.
        let connector_id = format!("test-{}", Uuid::now_v7());
        let (session_id, messages) = store
            .get_or_create_session("test", &connector_id)
            .await
            .expect("get_or_create_session");

        assert!(messages.is_empty(), "new session should have no messages");
        assert_eq!(session_id.get_version_num(), 7, "should be UUID v7");
    }

    #[tokio::test]
    async fn get_or_create_session_resumes_existing() {
        skip_without_db!(url);
        let pool = connect(SecretString::from(url.clone()))
            .await
            .expect("connect");
        let store = PgSessionStore::new(pool);

        let connector_id = format!("test-resume-{}", Uuid::now_v7());

        // Create session
        let (id1, _) = store
            .get_or_create_session("test", &connector_id)
            .await
            .expect("first call");

        // Resume should return the same session ID
        let (id2, _) = store
            .get_or_create_session("test", &connector_id)
            .await
            .expect("second call");

        assert_eq!(id1, id2, "should resume the same session");
    }

    #[tokio::test]
    async fn push_and_load_messages() {
        skip_without_db!(url);
        let pool = connect(SecretString::from(url.clone()))
            .await
            .expect("connect");
        let store = PgSessionStore::new(pool);

        let connector_id = format!("test-push-{}", Uuid::now_v7());
        let (session_id, _) = store
            .get_or_create_session("test", &connector_id)
            .await
            .expect("create session");

        let msg = Message::user_text("hello from integration test");
        store
            .push_message(session_id, 0, &msg)
            .await
            .expect("push_message");

        let loaded = store
            .load_messages(session_id)
            .await
            .expect("load_messages");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], msg);
    }

    #[tokio::test]
    async fn session_resume_loads_messages_in_order() {
        skip_without_db!(url);
        let pool = connect(SecretString::from(url.clone()))
            .await
            .expect("connect");
        let store = PgSessionStore::new(pool);

        let connector_id = format!("test-order-{}", Uuid::now_v7());
        let (session_id, _) = store
            .get_or_create_session("test", &connector_id)
            .await
            .expect("create session");

        let msgs = vec![
            Message::user_text("first"),
            Message::Assistant {
                content: vec![ContentBlock::Text {
                    text: "second".to_owned(),
                }],
                stop_reason: StopReason::EndTurn,
            },
            Message::user_text("third"),
        ];

        for (ordinal, msg) in msgs.iter().enumerate() {
            store
                .push_message(session_id, ordinal as i32, msg)
                .await
                .expect("push_message");
        }

        // Resume via get_or_create_session — should load all messages
        let (resumed_id, loaded) = store
            .get_or_create_session("test", &connector_id)
            .await
            .expect("resume session");

        assert_eq!(resumed_id, session_id);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded, msgs);
    }

    #[tokio::test]
    async fn image_round_trip() {
        skip_without_db!(url);
        let pool = connect(SecretString::from(url.clone()))
            .await
            .expect("connect");
        let store = PgSessionStore::new(pool);

        let connector_id = format!("test-image-{}", Uuid::now_v7());
        let (session_id, _) = store
            .get_or_create_session("test", &connector_id)
            .await
            .expect("create session");

        // Simulate a multimodal user message with an image
        let image_data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
        let msg = Message::User {
            content: vec![
                UserContent::Text("here is an image".to_owned()),
                UserContent::Image {
                    media_type: "image/png".to_owned(),
                    data: image_data.to_owned(),
                },
            ],
        };

        store
            .push_message(session_id, 0, &msg)
            .await
            .expect("push image message");

        let loaded = store
            .load_messages(session_id)
            .await
            .expect("load messages");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], msg, "image round-trip should be lossless");
    }

    #[tokio::test]
    async fn tool_result_round_trip() {
        skip_without_db!(url);
        let pool = connect(SecretString::from(url.clone()))
            .await
            .expect("connect");
        let store = PgSessionStore::new(pool);

        let connector_id = format!("test-tool-{}", Uuid::now_v7());
        let (session_id, _) = store
            .get_or_create_session("test", &connector_id)
            .await
            .expect("create session");

        let msgs = vec![
            Message::Assistant {
                content: vec![ContentBlock::ToolUse {
                    id: "toolu_01".to_owned(),
                    name: "bash".to_owned(),
                    input: serde_json::json!({"command": "ls -la"}),
                }],
                stop_reason: StopReason::ToolUse,
            },
            Message::ToolResult {
                tool_use_id: "toolu_01".to_owned(),
                content: "file.txt\ndir/".to_owned(),
                is_error: false,
            },
        ];

        for (i, msg) in msgs.iter().enumerate() {
            store
                .push_message(session_id, i as i32, msg)
                .await
                .expect("push");
        }

        let loaded = store.load_messages(session_id).await.expect("load");
        assert_eq!(loaded, msgs);
    }

    #[tokio::test]
    async fn different_connectors_get_independent_sessions() {
        skip_without_db!(url);
        let pool = connect(SecretString::from(url.clone()))
            .await
            .expect("connect");
        let store = PgSessionStore::new(pool);

        let unique = Uuid::now_v7().to_string();
        let (id_cli, _) = store
            .get_or_create_session("cli", &unique)
            .await
            .expect("cli session");
        let (id_telegram, _) = store
            .get_or_create_session("telegram", &unique)
            .await
            .expect("telegram session");

        assert_ne!(
            id_cli, id_telegram,
            "different connectors should have independent sessions"
        );
    }
}
