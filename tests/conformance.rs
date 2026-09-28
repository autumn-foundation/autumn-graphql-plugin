//! The plugin passes Autumn's own plugin conformance harness, and declares
//! exactly the routes it mounts.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use autumn_plugin_graphql::PLUGIN_NAME;
use autumn_web::plugin::Plugin;
use autumn_web::plugin_conformance::{ConformanceConfig, run_conformance};
use autumn_web::route_listing::RouteSource;
use common::plugin;

/// The manifest `declare_plugin_routes` records: the plugin's declared
/// routes, attributed to its name. (autumn-web 0.7.0 has no public accessor
/// for the builder's manifest; `main` adds `plugin_route_infos`, exercised
/// under the `plugin-contract` feature below.)
fn manifest<E: async_graphql::Executor>(
    plugin: &autumn_plugin_graphql::GraphqlPlugin<E>,
) -> Vec<autumn_web::route_listing::RouteInfo> {
    let name = plugin.name().into_owned();
    plugin
        .route_infos()
        .into_iter()
        .map(|mut route| {
            route.source = RouteSource::Plugin(name.clone());
            route
        })
        .collect()
}

fn declared(routes: &[autumn_web::route_listing::RouteInfo], plugin_name: &str) -> Vec<String> {
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

    let routes = manifest(&plugin);
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
    let routes = manifest(&plugin);
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

#[cfg(feature = "plugin-contract")]
#[test]
fn declares_a_contract_and_the_builder_records_the_routes() {
    let plugin = plugin();
    let contract = plugin.contract().expect("contract");
    assert_eq!(contract.plugin, PLUGIN_NAME);
    let name = plugin.name().into_owned();
    let expected = manifest(&plugin);
    let routes = autumn_web::app()
        .plugin(plugin)
        .plugin_route_infos()
        .expect("route manifest");
    assert_eq!(declared(&routes, &name), declared(&expected, &name));
    let report = run_conformance(
        &ConformanceConfig::new(&name)
            .prefix("/graphql")
            .contract(contract),
        &routes,
    );
    assert!(report.passed(), "{}", report.to_text_report());
}
