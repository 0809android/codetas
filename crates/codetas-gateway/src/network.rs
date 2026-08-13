use crate::config::{is_private_ip, ProviderDefinition};
use reqwest::{redirect::Policy, Client};
use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use tokio::net::lookup_host;
use url::Url;

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
    let mut builder = Client::builder()
        // Resolution is part of CODETAS's SSRF boundary. An ambient proxy would
        // resolve the host again and bypass the address set validated below.
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_millis(provider.limits.connect_timeout_ms))
        .timeout(Duration::from_millis(provider.limits.request_timeout_ms))
        .user_agent(user_agent);

    if let Ok(address) = host.parse::<IpAddr>() {
        if !provider.allow_private_network && is_private_ip(address) {
            return Err("provider endpoint is a private or local address".into());
        }
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
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    builder
        .build()
        .map_err(|_| "provider HTTP client could not be created".to_string())
}
