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
                assert!(
                    value == mount.trim_end_matches('/') || value.starts_with(&mount),
                    "{name} = {value} leaves {mount}"
                );
            }
        }
    }
}
