use axum::{
    async_trait,
    extract::FromRequestParts,
    response::{Html, IntoResponse},
    Extension, RequestPartsExt,
};
use chrono::{DateTime, Locale, TimeZone};
use evento::{EventoContext, PgProducer};
use http::{request::Parts, StatusCode};
use i18n_embed::{fluent::FluentLanguageLoader, LanguageLoader};
use leptos::*;
use minify_html::{minify, Cfg};
use serde::Deserialize;
use sqlx::PgPool;
use starter_core::axum_extra::UserLanguage;
use starter_feed::{FeedCommand, FeedQuery};
use std::{
    fmt::{self, Display},
    sync::Arc,
};
use tracing::{error, warn};
use twa_jwks::axum::JwtPayload;
use ulid::Ulid;
use unic_langid::LanguageIdentifier;
use validator::ValidationErrors;

use crate::{
    components::{
        InternalServerErrorAlert, InternalServerErrorPage, NotFoundPage, UnprocessableEntityAlert,
    },
    config::AppConfig,
    i18n::{LANGUAGES, LANGUAGE_LOADER},
};

#[derive(Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub producer: PgProducer,
    pub db: PgPool,
}

#[derive(Clone)]
pub struct AppContext {
    pub web_context: WebContext,
    pub feed_cmd: FeedCommand,
    pub feed_query: FeedQuery,
}

impl AppContext {
    pub fn html<F, N>(&self, f: F) -> impl IntoResponse
    where
        F: FnOnce() -> N + 'static,
        N: IntoView,
    {
        (StatusCode::OK, Html(self.web_context.html(f)))
    }

    pub fn internal_server_error<E: Display>(&self, err: E) -> impl IntoResponse {
        error!("{err}");

        (
            StatusCode::INTERNAL_SERVER_ERROR,
            self.html(move || {
                view! { <InternalServerErrorAlert /> }
            }),
        )
    }

    pub fn internal_server_error_page<E: Display>(&self, err: E) -> impl IntoResponse {
        error!("{err}");

        (
            StatusCode::INTERNAL_SERVER_ERROR,
            self.html(move || {
                view! { <InternalServerErrorPage /> }
            }),
        )
    }

    pub fn unprocessable_entity(&self, errors: ValidationErrors) -> impl IntoResponse {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            self.html(move || {
                view! { <UnprocessableEntityAlert errors=errors/> }
            }),
        )
    }

    pub fn not_found_page(&self) -> impl IntoResponse {
        (
            StatusCode::NOT_FOUND,
            self.html(move || {
                view! { <NotFoundPage /> }
            }),
        )
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for AppContext
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, Html<&'static str>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let JwtPayload(jwt_claims) =
            JwtPayload::<JwtClaims>::from_request_parts(parts, state).await?;

        let Ok(user_lang) = UserLanguage::from_request_parts(parts, state).await else {
            return Err((StatusCode::BAD_REQUEST, Html("Bad Request")));
        };

        let fl_loader = WebContext::fl_loader(user_lang);
        let lang = WebContext::lang(&fl_loader);

        let Extension(state) = parts
            .extract::<Extension<AppState>>()
            .await
            .expect("AppState not configured correctly");

        Ok(Self {
            feed_cmd: FeedCommand {
                producer: state.producer,
                user_id: jwt_claims.sub.to_owned(),
                request_id: Ulid::new().to_string(),
                user_lang: lang.clone(),
            },
            feed_query: FeedQuery {
                user_id: jwt_claims.sub,
                db: state.db,
            },
            web_context: WebContext {
                config: state.config.clone(),
                fl_loader: Arc::new(fl_loader),
                lang,
            },
        })
    }
}

#[derive(Clone)]
pub struct WebContext {
    pub config: AppConfig,
    pub lang: String,
    pub fl_loader: Arc<FluentLanguageLoader>,
}

impl WebContext {
    pub fn html<F, N>(&self, f: F) -> String
    where
        F: FnOnce() -> N + 'static,
        N: IntoView,
    {
        let ctx = self.clone();
        let html = ssr::render_to_string(move || {
            provide_context(ctx);

            f()
        });

        std::str::from_utf8(&minify(html.as_bytes(), &Cfg::new()))
            .unwrap_or_default()
            .to_owned()
    }

    pub fn create_url(&self, uri: impl Into<String>) -> String {
        let uri = uri.into();
        self.config
            .base_url
            .as_ref()
            .map(|base_url| format!("{base_url}{}", uri))
            .unwrap_or(uri)
    }

    pub fn create_static_url(&self, uri: impl Into<String>) -> String {
        self.create_url(format!("/static/{}", uri.into()))
    }

    pub fn create_css_url(&self, uri: impl Into<String>) -> String {
        self.create_static_url(format!("css/{}", uri.into()))
    }

    pub fn create_sse_url(&self, uri: impl Into<String>) -> String {
        format!("/pikav/{}{}", self.config.pikav.namespace, uri.into())
    }

    fn fl_loader(user_lang: UserLanguage) -> FluentLanguageLoader {
        let langs = user_lang
            .preferred_languages()
            .iter()
            .map(|lang| lang.parse().unwrap_or_default())
            .collect::<Vec<LanguageIdentifier>>();

        LANGUAGE_LOADER.select_languages(&langs)
    }

    fn fl_loader_from_string(user_lang: impl Into<String>) -> FluentLanguageLoader {
        LANGUAGE_LOADER.select_languages(&[user_lang
            .into()
            .parse::<LanguageIdentifier>()
            .unwrap_or_default()])
    }

    fn lang(loader: &FluentLanguageLoader) -> String {
        loader
            .current_languages()
            .iter()
            .find_map(|language| {
                if LANGUAGES.contains(language) {
                    Some(language.to_string())
                } else {
                    None
                }
            })
            .unwrap_or(loader.fallback_language().to_string())
    }

    pub fn format_localized<'a, Tz: TimeZone>(&self, dt: &'a DateTime<Tz>, fmt: &'a str) -> String
    where
        Tz::Offset: fmt::Display,
    {
        let locale = match self.lang.as_str() {
            "en" => Locale::en_US,
            "fr" => Locale::fr_FR,
            locale => {
                warn!("{locale} not handle in AppContext.format_localized");

                Locale::en_US
            }
        };

        dt.format_localized(fmt, locale).to_string()
    }
}

impl From<(&EventoContext, String)> for WebContext {
    fn from(value: (&EventoContext, String)) -> Self {
        let config = value.0.extract::<AppConfig>();
        let fl_loader = Self::fl_loader_from_string(&value.1);

        Self {
            config,
            fl_loader: Arc::new(fl_loader),
            lang: value.1,
        }
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for WebContext
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, Html<&'static str>);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Ok(user_lang) = UserLanguage::from_request_parts(parts, state).await else {
            return Err((StatusCode::BAD_REQUEST, Html("Bad Request")));
        };

        let fl_loader = WebContext::fl_loader(user_lang);
        let lang = WebContext::lang(&fl_loader);

        let Extension(state) = parts
            .extract::<Extension<AppState>>()
            .await
            .expect("AppState not configured correctly");

        Ok(Self {
            config: state.config.clone(),
            fl_loader: Arc::new(fl_loader),
            lang,
        })
    }
}

pub fn use_app() -> WebContext {
    use_context().expect("WebContext not configured correctly")
}

#[derive(Deserialize, Debug, Clone)]
pub struct JwtClaims {
    pub sub: String,
}