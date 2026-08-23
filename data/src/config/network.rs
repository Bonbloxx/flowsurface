use serde::{Deserialize, Serialize};

/// Combined network configuration.
///
/// Both settings take effect after a restart of the application.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Network {
    pub proxy: Option<exchange::proxy::Proxy>,
    pub server_url: Option<String>,
    /// Bearer token for the market-data server.
    /// Stored in the system keychain, never persisted to JSON.
    #[serde(skip)]
    pub server_auth_token: Option<String>,
    /// Optional always-on one-minute open-interest history service.
    #[serde(default)]
    pub oi_history_url: Option<String>,
    /// Bearer token for the open-interest history service.
    /// Stored in the system keychain, never persisted to JSON.
    #[serde(skip)]
    pub oi_history_auth_token: Option<String>,
    pub trade_fetch_mode: TradeFetchMode,
}

impl Network {
    /// Return a copy suitable for disk persistence (proxy auth stripped).
    /// Auth credentials are stored separately in the system keychain.
    pub fn for_persistence(&self) -> Self {
        Self {
            proxy: self.proxy.clone().map(|p| p.without_auth()),
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_network_config_defaults_open_interest_history_to_off() {
        let network: Network = serde_json::from_str(
            r#"{"proxy":null,"server_url":null,"trade_fetch_mode":{"type":"Off"}}"#,
        )
        .expect("old network config");

        assert_eq!(network.oi_history_url, None);
        assert_eq!(network.oi_history_auth_token, None);
    }

    #[test]
    fn persistence_never_contains_open_interest_history_token() {
        let network = Network {
            oi_history_url: Some("https://oi.example".to_string()),
            oi_history_auth_token: Some("secret".to_string()),
            ..Network::default()
        };

        let json = serde_json::to_string(&network.for_persistence()).expect("network json");
        assert!(json.contains("https://oi.example"));
        assert!(!json.contains("secret"));
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TradeFetchMode {
    #[default]
    Off,
    /// Direct exchange API only (Binance spot/linear/inverse).
    Exchange,
    /// Remote Arrow IPC market-data server.
    Server,
}

impl std::fmt::Display for TradeFetchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => write!(f, "Off"),
            Self::Exchange => write!(f, "Exchange"),
            Self::Server => write!(f, "Server"),
        }
    }
}
