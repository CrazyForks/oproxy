use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AssistantActionRisk {
    Mutate,
    Network,
    Destructive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ActionEndpointShape {
    IdBackedCollection,
    Singleton,
    DnsOverrides,
}

#[derive(Debug, Clone, Copy)]
struct StaticActionEndpointContract {
    base_path: &'static str,
    kind: &'static str,
    risk: AssistantActionRisk,
    refreshed_resource: &'static str,
    shape: ActionEndpointShape,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AssistantActionRouteContract {
    pub(super) base_path: String,
    pub(super) endpoint: String,
    pub(super) method: String,
    pub(super) kind: String,
    pub(super) risk: AssistantActionRisk,
    pub(super) refreshed_resources: Vec<String>,
    pub(super) shape: ActionEndpointShape,
    pub(super) item_id: Option<String>,
}

const ACTION_ENDPOINT_CONTRACTS: &[StaticActionEndpointContract] = &[
    id_collection("/admin/rule-sets", "rule-sets", "rule_sets"),
    id_collection("/admin/map-remote-rules", "map-remote-rules", "map_remote"),
    id_collection("/admin/map-local-rules", "map-local-rules", "map_local"),
    id_collection("/admin/access-rules", "access-rules", "access"),
    id_collection("/admin/breakpoints", "breakpoints", "breakpoints"),
    id_collection("/admin/mock/rules", "mock", "mock"),
    id_collection("/admin/scripts", "scripts", "scripts"),
    StaticActionEndpointContract {
        base_path: "/admin/webhooks",
        kind: "webhooks",
        risk: AssistantActionRisk::Network,
        refreshed_resource: "webhooks",
        shape: ActionEndpointShape::IdBackedCollection,
    },
    singleton("/admin/throttling", "throttling", "throttling"),
    singleton("/admin/capture-filter", "capture-filter", "capture_filter"),
    singleton("/admin/upstream-proxy", "upstream-proxy", "upstream_proxy"),
    StaticActionEndpointContract {
        base_path: "/admin/dns",
        kind: "dns",
        risk: AssistantActionRisk::Mutate,
        refreshed_resource: "dns",
        shape: ActionEndpointShape::DnsOverrides,
    },
    StaticActionEndpointContract {
        base_path: "/admin/forward",
        kind: "forward",
        risk: AssistantActionRisk::Network,
        refreshed_resource: "sessions",
        shape: ActionEndpointShape::Singleton,
    },
    StaticActionEndpointContract {
        base_path: "/admin/playback",
        kind: "playback",
        risk: AssistantActionRisk::Network,
        refreshed_resource: "sessions",
        shape: ActionEndpointShape::Singleton,
    },
    StaticActionEndpointContract {
        base_path: "/admin/sessions",
        kind: "sessions",
        risk: AssistantActionRisk::Destructive,
        refreshed_resource: "sessions",
        shape: ActionEndpointShape::Singleton,
    },
];

const fn id_collection(
    base_path: &'static str,
    kind: &'static str,
    refreshed_resource: &'static str,
) -> StaticActionEndpointContract {
    StaticActionEndpointContract {
        base_path,
        kind,
        risk: AssistantActionRisk::Mutate,
        refreshed_resource,
        shape: ActionEndpointShape::IdBackedCollection,
    }
}

const fn singleton(
    base_path: &'static str,
    kind: &'static str,
    refreshed_resource: &'static str,
) -> StaticActionEndpointContract {
    StaticActionEndpointContract {
        base_path,
        kind,
        risk: AssistantActionRisk::Mutate,
        refreshed_resource,
        shape: ActionEndpointShape::Singleton,
    }
}

pub(super) fn action_route_contract(
    method: &str,
    endpoint: &str,
) -> Result<AssistantActionRouteContract, String> {
    let method = method.to_ascii_uppercase();
    validate_route_syntax(&method, endpoint)?;
    let Some((contract, item_id)) = ACTION_ENDPOINT_CONTRACTS
        .iter()
        .filter_map(|contract| match_endpoint(contract, endpoint).map(|id| (*contract, id)))
        .next()
    else {
        return Err(format!(
            "assistant action endpoint '{endpoint}' is not allowlisted"
        ));
    };
    validate_method_for_shape(&method, &contract, item_id.as_deref())?;
    Ok(AssistantActionRouteContract {
        base_path: contract.base_path.to_string(),
        endpoint: endpoint.to_string(),
        method: method.clone(),
        kind: contract.kind.to_string(),
        risk: if method == "DELETE" {
            AssistantActionRisk::Destructive
        } else {
            contract.risk
        },
        refreshed_resources: vec![contract.refreshed_resource.to_string()],
        shape: contract.shape,
        item_id,
    })
}

pub(super) fn id_backed_collection_bases() -> Vec<&'static str> {
    ACTION_ENDPOINT_CONTRACTS
        .iter()
        .filter(|contract| contract.shape == ActionEndpointShape::IdBackedCollection)
        .map(|contract| contract.base_path)
        .collect()
}

pub(super) fn refreshed_resources_for_action(method: &str, endpoint: &str) -> Vec<String> {
    action_route_contract(method, endpoint)
        .map(|contract| contract.refreshed_resources)
        .unwrap_or_default()
}

fn validate_route_syntax(method: &str, endpoint: &str) -> Result<(), String> {
    if !matches!(method, "POST" | "PUT" | "DELETE") {
        return Err("assistant actions only support POST, PUT, and DELETE".to_string());
    }
    if !endpoint.starts_with("/admin/")
        || endpoint.contains("..")
        || endpoint.contains('?')
        || endpoint.contains('#')
    {
        return Err("assistant action endpoint must be a clean admin path".to_string());
    }
    Ok(())
}

fn match_endpoint(
    contract: &StaticActionEndpointContract,
    endpoint: &str,
) -> Option<Option<String>> {
    if endpoint == contract.base_path {
        return Some(None);
    }
    let id = endpoint
        .strip_prefix(contract.base_path)?
        .strip_prefix('/')?
        .to_string();
    if id.is_empty() || id.contains('/') {
        None
    } else {
        Some(Some(id))
    }
}

fn validate_method_for_shape(
    method: &str,
    contract: &StaticActionEndpointContract,
    item_id: Option<&str>,
) -> Result<(), String> {
    match (contract.shape, item_id, method) {
        (ActionEndpointShape::IdBackedCollection, None, "POST") => Ok(()),
        (ActionEndpointShape::IdBackedCollection, Some(_), "PUT" | "DELETE") => Ok(()),
        (ActionEndpointShape::DnsOverrides, None, "POST") => Ok(()),
        (ActionEndpointShape::DnsOverrides, Some(_), "DELETE") => Ok(()),
        (ActionEndpointShape::Singleton, None, "POST")
            if contract.base_path != "/admin/sessions" =>
        {
            Ok(())
        }
        (ActionEndpointShape::Singleton, None, "DELETE")
            if contract.base_path == "/admin/sessions" =>
        {
            Ok(())
        }
        _ => Err(format!(
            "assistant action endpoint '{}' does not support {}{}",
            contract.base_path,
            method,
            if item_id.is_some() {
                " with an item id"
            } else {
                ""
            }
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_contract_accepts_supported_collection_routes() {
        let create = action_route_contract("POST", "/admin/map-remote-rules").expect("create");
        assert_eq!(create.kind, "map-remote-rules");
        assert_eq!(create.risk, AssistantActionRisk::Mutate);
        assert_eq!(create.refreshed_resources, vec!["map_remote"]);

        let delete =
            action_route_contract("DELETE", "/admin/map-remote-rules/abc").expect("delete");
        assert_eq!(delete.item_id.as_deref(), Some("abc"));
        assert_eq!(delete.risk, AssistantActionRisk::Destructive);
    }

    #[test]
    fn action_contract_classifies_network_and_destructive_routes() {
        assert_eq!(
            action_route_contract("POST", "/admin/forward")
                .expect("forward")
                .risk,
            AssistantActionRisk::Network
        );
        assert_eq!(
            action_route_contract("POST", "/admin/webhooks")
                .expect("webhook")
                .risk,
            AssistantActionRisk::Network
        );
        assert_eq!(
            action_route_contract("DELETE", "/admin/sessions")
                .expect("clear sessions")
                .risk,
            AssistantActionRisk::Destructive
        );
    }

    #[test]
    fn action_contract_rejects_unknown_or_wrong_method_routes() {
        assert!(action_route_contract("POST", "/admin/unknown").is_err());
        assert!(action_route_contract("GET", "/admin/rule-sets").is_err());
        assert!(action_route_contract("PUT", "/admin/throttling").is_err());
        assert!(action_route_contract("DELETE", "/admin/dns").is_err());
        assert!(action_route_contract("POST", "/admin/rule-sets/abc").is_err());
    }

    #[test]
    fn action_contract_rejects_unclean_paths() {
        assert!(action_route_contract("POST", "/admin/rule-sets/abc/def").is_err());
        assert!(action_route_contract("POST", "/admin/rule-sets?x=1").is_err());
        assert!(action_route_contract("POST", "/admin/../config").is_err());
    }
}
