//! The plugin passes Autumn's own plugin conformance harness, and declares
//! exactly the routes it mounts.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use autumn_plugin_graphql::{PLUGIN_NAME, SUPPORTED_AUTUMN_WEB};
use autumn_web::plugin::Plugin;
use autumn_web::plugin_conformance::{ConformanceConfig, run_conformance};
use autumn_web::plugin_contract::{AUTUMN_WEB_VERSION, ContractVerdict, evaluate};
use autumn_web::route_listing::{RouteInfo, RouteSource};
use common::plugin;

/// The route manifest the app builder records for `plugin` — what
/// `autumn routes` and `autumn plugin-check` see. Mounting the plugin also
/// runs Autumn's contract gate, so an incompatible contract panics here.
fn manifest<E: async_graphql::Executor>(
    plugin: autumn_plugin_graphql::GraphqlPlugin<E>,
) -> Vec<RouteInfo> {
    autumn_web::app()
        .plugin(plugin)
        .plugin_route_infos()
        .expect("route manifest")
}

fn declared(routes: &[RouteInfo], plugin_name: &str) -> Vec<String> {
    let mut declared: Vec<String> = routes
        .iter()
        .filter(|r| matches!(&r.source, RouteSource::Plugin(n) if n == plugin_name))
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    declared.sort();
    declared
}

#[test]
fn passes_the_framework_conformance_harness() {
    let plugin = plugin();
    let name = plugin.name().into_owned();
    assert_eq!(name, format!("{PLUGIN_NAME}@/graphql"));

    let routes = manifest(plugin);
    let report = run_conformance(&ConformanceConfig::new(&name).prefix("/graphql"), &routes);
    assert!(report.passed(), "{}", report.to_text_report());
    assert_eq!(
        declared(&routes, &name),
        [
            "GET /graphql",
            "GET /graphql/sdl",
            "GET /graphql/ws",
            "POST /graphql"
        ]
    );
}

#[test]
fn declarations_follow_the_configuration() {
    let plugin = plugin().path("/api/graphql").without_sdl().configure(|c| {
        c.allow_get = false;
        c.subscriptions.websocket = false.into();
        c.subscriptions.sse = false.into();
    });
    let name = plugin.name().into_owned();
    let routes = manifest(plugin);
    assert_eq!(declared(&routes, &name), ["POST /api/graphql"]);
    let report = run_conformance(
        &ConformanceConfig::new(&name).prefix("/api/graphql"),
        &routes,
    );
    assert!(report.passed(), "{}", report.to_text_report());
}

#[test]
fn the_plugin_declares_its_config_section() {
    let app = autumn_web::app().plugin(autumn_plugin_graphql::GraphqlPlugin::new(common::schema()));
    assert!(app.has_config_section("graphql"));
    let app = autumn_web::app().plugin(
        autumn_plugin_graphql::GraphqlPlugin::new(common::schema()).config_section("graphql_admin"),
    );
    assert!(app.has_config_section("graphql_admin"));
}

#[test]
fn declares_a_contract_and_the_builder_records_the_declared_routes() {
    let plugin = plugin();
    let contract = plugin.contract().expect("contract");
    assert_eq!(contract.plugin, PLUGIN_NAME);
    let name = plugin.name().into_owned();
    let declared_by_plugin: Vec<RouteInfo> = plugin
        .route_infos()
        .into_iter()
        .map(|mut route| {
            route.source = RouteSource::Plugin(name.clone());
            route
        })
        .collect();
    let routes = manifest(plugin);
    assert_eq!(
        declared(&routes, &name),
        declared(&declared_by_plugin, &name)
    );
    let report = run_conformance(
        &ConformanceConfig::new(&name)
            .prefix("/graphql")
            .contract(contract),
        &routes,
    );
    assert!(report.passed(), "{}", report.to_text_report());
}

#[test]
fn the_contract_admits_the_linked_autumn_web_and_no_other_series() {
    let contract = plugin().contract().expect("contract");
    assert_eq!(contract.autumn_web.as_deref(), Some(SUPPORTED_AUTUMN_WEB));
    assert_eq!(
        evaluate(&contract, AUTUMN_WEB_VERSION),
        ContractVerdict::Compatible
    );
    for other in ["0.7.0", "0.9.0", "1.0.0"] {
        assert!(
            matches!(evaluate(&contract, other), ContractVerdict::Incompatible(_)),
            "contract must refuse autumn-web {other}"
        );
    }
}
