//! The published client contract describes exactly the production route table.
use std::collections::BTreeMap;

use nddev_device_sync_cache::domain::Routes;

fn contract() -> serde_json::Value {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/contracts/clients.json"
    ))
    .unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn client_contract_matches_the_production_routes() {
    let routes = Routes::production();
    let served: BTreeMap<&str, (&str, &str)> = routes
        .routes()
        .iter()
        .map(|route| (route.id, (route.prefix, route.upstream.as_str())))
        .collect();
    let contract = contract();
    let published: BTreeMap<&str, (&str, &str)> = contract["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|route| {
            (
                route["id"].as_str().unwrap(),
                (
                    route["prefix"].as_str().unwrap(),
                    route["upstream"].as_str().unwrap(),
                ),
            )
        })
        .collect();
    assert_eq!(published, served);
}

#[test]
fn client_settings_stay_inside_their_route() {
    let contract = contract();
    for route in contract["routes"].as_array().unwrap() {
        let mount = format!("{{base}}{}/", route["prefix"].as_str().unwrap());
        for client in route["clients"].as_array().unwrap() {
            for (name, value) in client["setting"].as_object().unwrap() {
                let value = value.as_str().unwrap();
                if !value.contains("{base}") {
                    // The name of a tool-local source that another setting defines.
                    assert!(
                        value
                            .bytes()
                            .all(|byte| byte.is_ascii_lowercase() || byte == b'-'),
                        "{name} = {value} is neither under the base nor a source name"
                    );
                    continue;
                }
                let value = value.strip_prefix("sparse+").unwrap_or(value);
                assert!(
                    value == mount.trim_end_matches('/') || value.starts_with(&mount),
                    "{name} = {value} leaves {mount}"
                );
            }
        }
    }
}

#[test]
fn a_route_reached_through_another_names_a_route_with_clients() {
    let contract = contract();
    let routes = contract["routes"].as_array().unwrap();
    for route in routes {
        let Some(through) = route["through"].as_str() else {
            continue;
        };
        let parent = routes
            .iter()
            .find(|candidate| candidate["id"] == through)
            .unwrap_or_else(|| panic!("{through} is not a route"));
        assert!(
            parent["through"].is_null(),
            "{through} is itself reached through another"
        );
        assert!(!parent["clients"].as_array().unwrap().is_empty());
        assert!(
            route["prefix"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{}/", parent["prefix"].as_str().unwrap())),
            "{} is not under {through}",
            route["id"]
        );
    }
}
