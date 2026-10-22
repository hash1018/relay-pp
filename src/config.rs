//! The configuration file: what relay-pp listens on — see `docs/design.md`,
//! "Configuration". Channels are not in it: they are made and removed over
//! the API.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use serde::Deserialize;

/// Why a configuration could not be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file.
        path: String,
        /// Why.
        source: std::io::Error,
    },
    /// The file is not TOML this understands.
    #[error("{0}")]
    Parse(#[from] toml::de::Error),
    /// A WebRTC candidate is neither an address nor an IP.
    #[error("webrtc.candidates: {0:?} is neither an IP nor an IP and port")]
    Candidate(String),
}

/// Everything the file says.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// RTSP players.
    #[serde(default)]
    pub rtsp: RtspConfig,
    /// WebRTC players: the UDP side; offers come in over [`Config::http`].
    #[serde(default)]
    pub webrtc: WebRtcConfig,
    /// The API, and WHEP.
    #[serde(default)]
    pub http: HttpConfig,
}

/// `[rtsp]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RtspConfig {
    /// Where RTSP players connect.
    #[serde(default = "RtspConfig::default_listen")]
    pub listen: SocketAddr,
    /// Whether players may ask for RTP over UDP as well as over their
    /// connection.
    #[serde(default = "yes")]
    pub udp: bool,
}

impl RtspConfig {
    fn default_listen() -> SocketAddr {
        SocketAddr::from(([0, 0, 0, 0], 8554))
    }
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self {
            listen: Self::default_listen(),
            udp: true,
        }
    }
}

/// `[webrtc]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebRtcConfig {
    /// The UDP address WebRTC media goes through.
    #[serde(default = "WebRtcConfig::default_listen")]
    pub listen: SocketAddr,
    /// Addresses players are told to send to in place of `listen`'s — a
    /// router's public address, the machine's own where `listen` is every
    /// interface. An IP alone keeps `listen`'s port.
    #[serde(default)]
    pub candidates: Vec<String>,
    /// How many threads sessions are spread over, each with a UDP port of
    /// its own from `listen`'s on.
    pub threads: Option<usize>,
}

impl WebRtcConfig {
    fn default_listen() -> SocketAddr {
        SocketAddr::from(([0, 0, 0, 0], 8189))
    }

    /// [`Self::candidates`] as addresses.
    pub fn candidate_addrs(&self) -> Result<Vec<SocketAddr>, ConfigError> {
        self.candidates
            .iter()
            .map(|text| {
                text.parse()
                    .ok()
                    .or_else(|| text.parse::<IpAddr>().ok().map(|ip| SocketAddr::new(ip, 0)))
                    .ok_or_else(|| ConfigError::Candidate(text.clone()))
            })
            .collect()
    }
}

impl Default for WebRtcConfig {
    fn default() -> Self {
        Self {
            listen: Self::default_listen(),
            candidates: Vec::new(),
            threads: None,
        }
    }
}

/// `[http]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// Where the API and WHEP are served.
    #[serde(default = "HttpConfig::default_listen")]
    pub listen: SocketAddr,
}

impl HttpConfig {
    fn default_listen() -> SocketAddr {
        SocketAddr::from(([0, 0, 0, 0], 8888))
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen: Self::default_listen(),
        }
    }
}

impl Config {
    /// The configuration in the file at `path`.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    /// The configuration `text` says, checked.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text)?;
        config.webrtc.candidate_addrs()?;
        Ok(config)
    }
}

fn yes() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_file_is_read_and_defaults_fill_the_rest() {
        let config = Config::parse(
            r#"
            [rtsp]
            listen = "127.0.0.1:18554"

            [webrtc]
            candidates = ["192.168.0.10", "203.0.113.7:8189"]
            "#,
        )
        .unwrap();
        assert_eq!(config.rtsp.listen, "127.0.0.1:18554".parse().unwrap());
        assert!(config.rtsp.udp);
        assert_eq!(config.http.listen, "0.0.0.0:8888".parse().unwrap());
        assert_eq!(
            config.webrtc.candidate_addrs().unwrap(),
            [
                "192.168.0.10:0".parse().unwrap(),
                "203.0.113.7:8189".parse().unwrap()
            ]
        );
    }

    #[test]
    fn what_cannot_be_used_is_refused() {
        assert!(Config::parse("[rtsp]\nport = 1").is_err(), "an unknown key");
        assert!(Config::parse("[webrtc]\ncandidates = [\"a host\"]").is_err());
        assert!(
            Config::parse("[channels.cam1]\nsource = \"rtsp://h/\"").is_err(),
            "channels are made over the API"
        );
    }
}
