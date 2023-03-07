use std::sync::Arc;

use actix_cors::Cors;

use actix_middleware_etag::Etag;
use actix_web::middleware::Logger;
use actix_web::{web, App, HttpServer};
use actix_web_opentelemetry::RequestTracing;
use clap::Parser;
use cli::CliArgs;
use dashmap::DashMap;
use futures::future::join_all;
use unleash_edge::builder::build_caches_and_refreshers;
use unleash_edge::persistence::{persist_data, EdgePersistence};
use unleash_edge::types::{EdgeToken, TokenRefresh};
use unleash_types::client_features::ClientFeatures;
use unleash_types::client_metrics::ConnectVia;

use unleash_edge::client_api;
use unleash_edge::edge_api;
use unleash_edge::frontend_api;
use unleash_edge::internal_backstage;
use unleash_edge::metrics::client_metrics::MetricsCache;
use unleash_edge::openapi;
use unleash_edge::prom_metrics;
use unleash_edge::{cli, middleware};
use utoipa_swagger_ui::SwaggerUi;
mod tls;
use utoipa::OpenApi;

#[actix_web::main]
async fn main() -> Result<(), anyhow::Error> {
    dotenv::dotenv().ok();
    let args = CliArgs::parse();
    let schedule_args = args.clone();
    let mode_arg = args.clone().mode;
    let http_args = args.clone().http;
    let (metrics_handler, request_metrics) = prom_metrics::instantiate(None);
    let connect_via = ConnectVia {
        app_name: args.clone().app_name,
        instance_id: args.clone().instance_id,
    };
    let (
        (token_cache, features_cache, engine_cache),
        token_validator,
        feature_refresher,
        persistence,
    ) = build_caches_and_refreshers(args).await.unwrap();

    let lazy_feature_cache = features_cache.clone();
    let lazy_token_cache = token_cache.clone();

    let metrics_cache = Arc::new(MetricsCache::default());
    let metrics_cache_clone = metrics_cache.clone();

    let openapi = openapi::ApiDoc::openapi();
    let refresher_for_app_data = feature_refresher.clone();
    let server = HttpServer::new(move || {
        let cors_middleware = Cors::default()
            .allow_any_origin()
            .send_wildcard()
            .allow_any_header()
            .allow_any_method();
        let mut app = App::new()
            .app_data(web::Data::new(mode_arg.clone()))
            .app_data(web::Data::new(connect_via.clone()))
            .app_data(web::Data::new(metrics_cache.clone()))
            .app_data(web::Data::from(token_cache.clone()))
            .app_data(web::Data::from(features_cache.clone()))
            .app_data(web::Data::from(engine_cache.clone()));
        app = match token_validator.clone() {
            Some(v) => app.app_data(web::Data::from(v)),
            None => app,
        };
        app = match refresher_for_app_data.clone() {
            Some(refresher) => app.app_data(web::Data::from(refresher)),
            None => app,
        };
        app.wrap(Etag::default())
            .wrap(cors_middleware)
            .wrap(RequestTracing::new())
            .wrap(request_metrics.clone())
            .wrap(Logger::default())
            .service(web::scope("/internal-backstage").configure(|service_cfg| {
                internal_backstage::configure_internal_backstage(
                    service_cfg,
                    metrics_handler.clone(),
                )
            }))
            .service(
                web::scope("/api")
                    .wrap(middleware::as_async_middleware::as_async_middleware(
                        middleware::validate_token::validate_token,
                    ))
                    .configure(client_api::configure_client_api)
                    .configure(frontend_api::configure_frontend_api),
            )
            .service(web::scope("/edge").configure(edge_api::configure_edge_api))
            .service(
                SwaggerUi::new("/swagger-ui/{_:.*}").url("/api-doc/openapi.json", openapi.clone()),
            )
    });
    let server = if http_args.tls.tls_enable {
        let config = tls::config(http_args.clone().tls)
            .expect("Was expecting to succeed in configuring TLS");
        server
            .bind_rustls(http_args.https_server_tuple(), config)?
            .bind(http_args.http_server_tuple())
    } else {
        server.bind(http_args.http_server_tuple())
    };
    let server = server?.workers(http_args.workers).shutdown_timeout(5);

    match schedule_args.mode {
        crate::cli::EdgeMode::Edge(edge) => {
            let refresher = feature_refresher.clone().unwrap();
            tokio::select! {
                _ = server.run() => {
                    tracing::info!("Actix is shutting down. Persisting data");
                    clean_shutdown(persistence.clone(), lazy_feature_cache.clone(), lazy_token_cache.clone(), refresher.tokens_to_refresh.clone()).await;
                    tracing::info!("Actix was shutdown properly");
                },
                _ = refresher.refresh_features() => {
                    tracing::info!("Feature refresher unexpectedly shut down");
                }
                _ = unleash_edge::http::background_send_metrics::send_metrics_task(metrics_cache_clone.clone(), refresher.unleash_client.clone(), edge.metrics_interval_seconds) => {
                    tracing::info!("Metrics poster unexpectedly shut down");
                }
                _ = persist_data(persistence.clone(), lazy_token_cache.clone(), lazy_feature_cache.clone(), refresher.tokens_to_refresh.clone()) => {
                    tracing::info!("Persister was unexpectedly shut down");
                }
            }
        }
        _ => tokio::select! {
            _ = server.run() => {
                tracing::info!("Actix is shutting down. Persisting data");
                let refresher = feature_refresher.clone().unwrap();
                clean_shutdown(persistence, lazy_feature_cache.clone(), lazy_token_cache.clone(), refresher.tokens_to_refresh.clone()).await;
                tracing::info!("Actix was shutdown properly");

            }
        },
    };

    Ok(())
}

async fn clean_shutdown(
    persistence: Option<Arc<dyn EdgePersistence>>,
    feature_cache: Arc<DashMap<String, ClientFeatures>>,
    token_cache: Arc<DashMap<String, EdgeToken>>,
    refresh_target_cache: Arc<DashMap<String, TokenRefresh>>,
) {
    let tokens: Vec<EdgeToken> = token_cache
        .iter()
        .map(|entry| entry.value().clone())
        .collect();

    let refresh_targets: Vec<TokenRefresh> = refresh_target_cache
        .iter()
        .map(|entry| entry.value().clone())
        .collect();

    let features: Vec<(String, ClientFeatures)> = feature_cache
        .iter()
        .map(|entry| (entry.key().clone(), entry.value().clone()))
        .collect();

    if let Some(persistence) = persistence {
        let res = join_all(vec![
            persistence.save_tokens(tokens),
            persistence.save_features(features),
            persistence.save_refresh_targets(refresh_targets),
        ])
        .await;
        if res.iter().all(|save| save.is_ok()) {
            tracing::info!("Successfully persisted data");
        } else {
            res.iter()
                .filter(|save| save.is_err())
                .for_each(|failed_save| tracing::error!("Failed backing up: {failed_save:?}"));
        }
    }
}
