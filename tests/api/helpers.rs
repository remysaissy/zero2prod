use once_cell::sync::Lazy;
use reqwest::{Client, Response, Url};
use sqlx::Executor;
use sqlx::{Connection, PgConnection, PgPool};
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zero2prod::configuration::{get_configuration, DatabaseSettings};
use zero2prod::startup::{get_connection_pool, Application};
use zero2prod::telemetry::{get_subscriber, init_subscriber};

/// Confirmation links embedded in the request to the email API.
pub struct ConfirmationLinks {
    pub html: Url,
    pub plain_text: Url,
}

pub struct TestApp {
    pub address: String,
    pub port: u16,
    pub db_pool: PgPool,
    pub email_server: MockServer,
}

impl TestApp {
    pub async fn create_unconfirmed_subscriber(&self) -> ConfirmationLinks {
        let body = "name=le%20guin&email=ursula_le_guin%40gmail.com";
        let _mock_guard = Mock::given(path("/email"))
            .and(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .named("Create unconfirmed subscriber")
            .expect(1)
            .mount_as_scoped(&self.email_server)
            .await;
        self.post_subscriptions(body.into()).await;

        let email_request = &self
            .email_server
            .received_requests()
            .await
            .unwrap()
            .pop()
            .unwrap();
        self.get_confirmation_links(email_request)
    }

    pub async fn create_confirmed_subscriber(&self) {
        // We can then reuse the same helper and just add
        // an extra step to actually call the confirmation link!
        let confirmation_link = self.create_unconfirmed_subscriber().await;
        reqwest::get(confirmation_link.html)
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }

    pub async fn post_subscriptions(&self, body: String) -> Response {
        Client::new()
            .post(format!("{}/subscriptions", &self.address))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .expect("Failed to execute request")
    }

    /// Extract the confirmation links embedded in the request to the email API.
    pub fn get_confirmation_links(&self, email_request: &wiremock::Request) -> ConfirmationLinks {
        let body: serde_json::Value = serde_json::from_slice(&email_request.body).unwrap();
        // Extract the link from one of the request fields.
        let get_link = |s: &str| {
            let links: Vec<_> = linkify::LinkFinder::new()
                .links(s)
                .filter(|l| *l.kind() == linkify::LinkKind::Url)
                .collect();
            assert_eq!(links.len(), 1);
            let raw_link = links[0].as_str().to_owned();
            let mut confirmation_link = Url::parse(&raw_link).unwrap();
            // Let's make sure we don't call random APIs on the web
            assert_eq!(confirmation_link.host_str().unwrap(), "127.0.0.1");
            confirmation_link.set_port(Some(self.port)).unwrap();
            confirmation_link
        };
        let html = get_link(&body["HtmlBody"].as_str().unwrap());
        let plain_text = get_link(&body["TextBody"].as_str().unwrap());
        ConfirmationLinks { html, plain_text }
    }
}

// Ensure that the 'tracing' stack is only initialised once using 'once_cell'
static TRACING: Lazy<()> = Lazy::new(|| {
    let default_filter_level = "info".to_string();
    let subscriber_name = "test".to_string();
    // We cannot assign the output of `get_subscriber` to a variable based on the value of `TEST_LOG`
    // because the sink is part of the type returned by `get_subscriber`, therefore they are not the
    // same type. We could work around it, but this is the most straight-forward way of moving forward.
    if std::env::var("TEST_LOG").is_ok() {
        let subscriber = get_subscriber(subscriber_name, default_filter_level, std::io::stdout);
        init_subscriber(subscriber);
    } else {
        let subscriber = get_subscriber(subscriber_name, default_filter_level, std::io::sink);
        init_subscriber(subscriber);
    }
});

pub async fn spawn_app() -> TestApp {
    Lazy::force(&TRACING);

    // Launch a mock server for emails.
    let email_server = MockServer::start().await;

    // Randomise configuration to ensure test isolation
    let configuration = {
        let mut configuration = get_configuration().expect("Failed to read configuration");
        configuration.database.database_name = Uuid::new_v4().to_string();
        configuration.application.port = 0;
        configuration.email_client.base_url = email_server.uri();
        configuration
    };

    // Create and migrate the database
    configure_database(&configuration.database).await;

    // Launch the application as a background task
    let application = Application::build(configuration.clone())
        .await
        .expect("Failed to build application");
    let port = application.port();
    let address = format!("http://127.0.0.1:{}", port);
    let _ = tokio::spawn(application.run_until_stopped());

    TestApp {
        address,
        port,
        db_pool: get_connection_pool(&configuration),
        email_server,
    }
}

async fn configure_database(config: &DatabaseSettings) -> PgPool {
    // Create database
    let mut connection = PgConnection::connect_with(&config.without_db())
        .await
        .expect("Failed to connect to Postgres");
    connection
        .execute(format!(r#"CREATE DATABASE "{}";"#, config.database_name).as_str())
        .await
        .expect("Failed to create database");

    // Migrate database
    let db_pool = PgPool::connect_with(config.with_db())
        .await
        .expect("Failed to connect to Postgres");
    sqlx::migrate!("./migrations")
        .run(&db_pool)
        .await
        .expect("Failed to migrate the database");
    db_pool
}
