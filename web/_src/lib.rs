mod components;
mod config;
mod i18n;
mod routes;
mod state;
mod subscriber;

use anyhow::Result;
use axum::{
    http::{header, StatusCode, Uri},
    response::IntoResponse,
    routing::get,
    Extension, Router,
};
use evento::PgConsumer;
use evento_axum::{UserLanguage, QuerySource, AcceptLanguageSource};
use leptos::*;
use pikav_client::timada::SimpleEvent;
use rust_embed::RustEmbed;
use sqlx::PgPool;
use state::WebContext;
use tracing::info;
use twa_jwks::JwksClient;

use crate::{components::NotFoundPage, config::Config, state::AppState};

pub async fn serve() -> Result<()> {
    let config = Config::new()?;
    let state_config = config.app.clone();

    let jwks = JwksClient::build(config.app.jwks_url).await?;
    let db = PgPool::connect(&config.dsn).await?;
    let pikva_client = pikav_client::Client::new(pikav_client::ClientOptions {
        url: config.app.pikav.url.to_owned(),
        namespace: config.app.pikav.namespace.to_owned(),
    })?;

    sqlx::migrate!("../migrations")
        .set_locking(false)
        .run(&db)
        .await?;

    let producer = PgConsumer::new(&db)
        .name(&config.region)
        .data(pikva_client.clone())
        .data(state_config.clone())
        .rules(starter_feed::rules())
        .start(config.app.evento_delay.unwrap_or(30))
        .await?;

    let command = evento::Command::new(&producer);
    let query = evento::Query::new().data(db.clone());

    let router = routes::create_router();

    let app = match config.app.base_url {
        Some(base_url) => Router::new().nest(&base_url, router),
        _ => router,
    }
    .fallback(get(static_handler))
    .layer(Extension(command))
    .layer(Extension(query))
    .layer(Extension(
        UserLanguage::config()
            .add_source(QuerySource::new("lang"))
            .add_source(AcceptLanguageSource)
            .build(),
    ))
    .layer(Extension(jwks))
    .layer(Extension(AppState {
        config: state_config,
        producer,
        db,
    }));

    #[cfg(debug_assertions)]
    pikva_client.publish(vec![SimpleEvent {
        user_id: "*".into(),
        topic: "sys".into(),
        event: "hot-reload".into(),
        data: "App was updated".into(),
    }]);

    info!("app listening on http://{}", &config.app.addr);

    let listener = tokio::net::TcpListener::bind(config.app.addr).await?;

    axum::serve(listener, app.into_make_service()).await?;

    Ok(())
}

#[derive(RustEmbed)]
#[folder = "public/"]
#[prefix = "/static/"]
struct Assets;

async fn static_handler(
    uri: Uri,
    Extension(app): Extension<AppState>,
    ctx: WebContext,
) -> impl IntoResponse {
    let uri = uri.to_string();
    let path = app
        .config
        .base_url
        .map(|base_url| {
            let mut uri = uri.to_owned();

            if uri.starts_with(&base_url) {
                uri.replace_range(0..base_url.len(), "");
            }

            uri
        })
        .unwrap_or(uri);

    if !path.starts_with("/static/") {
        return (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/html")],
            ctx.html(move || {
                view! { <NotFoundPage /> }
            }),
        )
            .into_response();
    }

    match Assets::get(path.as_str()) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref())], content.data).into_response()
        }
        None => (StatusCode::NOT_FOUND, "404 Not Found").into_response(),
    }
}