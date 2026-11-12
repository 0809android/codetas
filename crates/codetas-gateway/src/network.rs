use crate::config::{is_private_ip, ProviderDefinition};
use reqwest::{redirect::Policy, Client};
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::{Mutex, OnceLock},
    time::Duration,
};
use tokio::net::lookup_host;
use url::Url;

/// Upper bound on reusable pinned clients. Reaching it drops them all; the next
/// requests build fresh ones, as every request did before the cache existed.
const MAX_CACHED_PINNED_CLIENTS: usize = 64;

/// Everything a pinned client is built from. Two requests share a client only when
/// they validated to the same address set with the same settings, so reuse never
/// widens what one validation allowed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PinnedClientKey {
    host: String,
    /// Sorted. Empty when the endpoint host is an IP literal, which needs no pin.
    addresses: Vec<SocketAddr>,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
    user_agent: String,
}

fn pinned_client_cache() -> &'static Mutex<HashMap<PinnedClientKey, Client>> {
    static CACHE: OnceLock<Mutex<HashMap<PinnedClientKey, Client>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A client whose connections go only to the addresses validated for this request.
///
/// Resolution and the private-address check run on every call. A client built for
/// the same validated address set is reused, so consecutive requests keep their
/// pooled TCP/TLS connections instead of paying a new handshake each turn.
pub(crate) async fn pinned_client(
    endpoint: &str,
    provider: &ProviderDefinition,
    user_agent: &str,
) -> Result<Client, String> {
    let url = Url::parse(endpoint).map_err(|_| "provider endpoint URL is invalid".to_string())?;
    let host = url
        .host_str()
        .ok_or_else(|| "provider endpoint has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "provider endpoint has no usable port".to_string())?;

    let addresses = if let Ok(address) = host.parse::<IpAddr>() {
        if !provider.allow_private_network && is_private_ip(address) {
            return Err("provider endpoint is a private or local address".into());
        }
        Vec::new()
    } else {
        let resolved = lookup_host((host, port))
            .await
            .map_err(|_| "provider hostname could not be resolved".to_string())?;
        let mut seen = HashSet::new();
        let addresses = resolved
            .filter(|address| seen.insert(address.ip()))
            .collect::<Vec<SocketAddr>>();
        if addresses.is_empty() {
            return Err("provider hostname resolved to no addresses".into());
        }
        if !provider.allow_private_network
            && addresses.iter().any(|address| is_private_ip(address.ip()))
        {
            return Err("provider hostname resolved to a private or local address".into());
        }
        addresses
    };

    let mut sorted = addresses.clone();
    sorted.sort();
    let key = PinnedClientKey {
        host: host.to_string(),
        addresses: sorted,
        connect_timeout_ms: provider.limits.connect_timeout_ms,
        request_timeout_ms: provider.limits.request_timeout_ms,
        user_agent: user_agent.to_string(),
    };
    if let Some(client) = pinned_client_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&key)
    {
        return Ok(client.clone());
    }

    // The resolver's order is kept for the connection attempts; only the key is sorted.
    let client = build_pinned_client(&key, &addresses)?;
    let mut cache = pinned_client_cache()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if cache.len() >= MAX_CACHED_PINNED_CLIENTS && !cache.contains_key(&key) {
        cache.clear();
    }
    Ok(cache.entry(key).or_insert(client).clone())
}

fn build_pinned_client(key: &PinnedClientKey, addresses: &[SocketAddr]) -> Result<Client, String> {
    let mut builder = Client::builder()
        // Resolution is part of CODETAS's SSRF boundary. An ambient proxy would
        // resolve the host again and bypass the address set validated above.
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_millis(key.connect_timeout_ms))
        .timeout(Duration::from_millis(key.request_timeout_ms))
        .user_agent(key.user_agent.as_str());
    if !addresses.is_empty() {
        builder = builder.resolve_to_addrs(&key.host, addresses);
    }
    builder
        .build()
        .map_err(|_| "provider HTTP client could not be created".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(addresses: &[&str]) -> PinnedClientKey {
        PinnedClientKey {
            host: "api.example".into(),
            addresses: addresses
                .iter()
                .map(|address| address.parse().unwrap())
                .collect(),
            connect_timeout_ms: 1_000,
            request_timeout_ms: 2_000,
            user_agent: "CODETAS-Test/0.1".into(),
        }
    }

    #[test]
    fn a_different_validated_address_set_never_shares_a_client() {
        assert_eq!(key(&["93.184.216.34:443"]), key(&["93.184.216.34:443"]));
        assert_ne!(key(&["93.184.216.34:443"]), key(&["93.184.216.35:443"]));
        assert_ne!(
            key(&["93.184.216.34:443"]),
            key(&["93.184.216.34:443", "93.184.216.35:443"])
        );
        let mut other_timeout = key(&["93.184.216.34:443"]);
        other_timeout.request_timeout_ms += 1;
        assert_ne!(key(&["93.184.216.34:443"]), other_timeout);
    }

    #[test]
    fn pinned_clients_skip_ambient_proxies_and_redirects() {
        let client = build_pinned_client(
            &key(&["93.184.216.34:443"]),
            &["93.184.216.34:443".parse().unwrap()],
        )
        .unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("proxies"), "{debug}");
        assert!(debug.contains("redirect_policy"), "{debug}");
    }
}
