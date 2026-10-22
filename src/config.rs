//! The configuration file: what relay-pp listens on and which channels it
//! serves — see `docs/design.md`, "Configuration".

use std::collections::BTreeMap;
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
    /// A channel's name is not one a URL carries as it is: it is what
    /// players ask for, `rtsp://host/<name>` and `/<name>/whep`.
    #[error(
        "channel {0:?}: a name is letters, digits, `-`, `_`, `.` and `~`, in parts between `/`"
    )]
    Name(String),
    /// A channel's source is not one relay-pp takes yet.
    #[error("channel {channel}: source {url:?} is not an rtsp:// or rtsps:// URL")]
    Source {
        /// The channel.
        channel: String,
        /// Its source, with any credentials cut out.
        url: String,
    },
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
    /// Each channel by its name, which is what players ask for.
    #[serde(default)]
    pub channels: BTreeMap<String, ChannelConfig>,
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

/// `[channels.<name>]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    /// Where the stream comes from: an `rtsp://` or `rtsps://` URL, pulled
    /// for as long as relay-pp runs.
    pub source: String,
    /// How the stream is pulled: over the RTSP connection (`"tcp"`, the
    /// default, which loses nothing) or `"udp"`.
    #[serde(default)]
    pub transport: Transport,
}

/// How a camera is pulled.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// RTP interleaved on the RTSP connection.
    #[default]
    Tcp,
    /// RTP on UDP ports of its own.
    Udp,
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
        for (name, channel) in &config.channels {
            if !valid_name(name) {
                return Err(ConfigError::Name(name.clone()));
            }
            let scheme = channel.source.split("://").next().unwrap_or_default();
            if !channel.source.contains("://") || !matches!(scheme, "rtsp" | "rtsps") {
                return Err(ConfigError::Source {
                    channel: name.clone(),
                    url: redact(&channel.source),
                });
            }
        }
        config.webrtc.candidate_addrs()?;
        Ok(config)
    }
}

fn yes() -> bool {
    true
}

/// Whether `name` goes into a URL as it is: parts of letters, digits, `-`,
/// `_`, `.` and `~`, between single `/`.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('/').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
        })
}

/// `url` with any user name and password cut out, for a log line or an
/// answer.
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_owned(),
    }
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

            [channels.cam1]
            source = "rtsp://user:secret@10.0.0.21/stream1"

            [channels."floor2/cam3"]
            source = "rtsp://10.0.0.23/stream1"
            transport = "udp"
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
        assert_eq!(config.channels["cam1"].transport, Transport::Tcp);
        assert_eq!(config.channels["floor2/cam3"].transport, Transport::Udp);
    }

    #[test]
    fn what_cannot_be_used_is_refused_saying_what_and_no_secret() {
        let error = Config::parse("[channels.cam1]\nsource = \"http://user:secret@10.0.0.21/\"")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("cam1") && !error.contains("secret"),
            "{error}"
        );
        assert!(Config::parse("[rtsp]\nport = 1").is_err(), "an unknown key");
        assert!(Config::parse("[webrtc]\ncandidates = [\"a host\"]").is_err());
        for name in ["front door", "/cam", "cam//1", "cam?1", ""] {
            let text = format!("[channels.\"{name}\"]\nsource = \"rtsp://10.0.0.1/\"");
            assert!(
                matches!(Config::parse(&text), Err(ConfigError::Name(_))),
                "{name:?} goes into no URL as it is"
            );
        }
        assert!(Config::parse("[channels.\"floor2/cam-3_a.b~c\"]\nsource = \"rtsp://h/\"").is_ok());
    }

    #[test]
    fn credentials_are_cut_out_of_a_url() {
        assert_eq!(
            redact("rtsp://admin:p@ss@10.0.0.1:554/live"),
            "rtsp://10.0.0.1:554/live"
        );
        assert_eq!(redact("rtsp://10.0.0.1/live"), "rtsp://10.0.0.1/live");
        assert_eq!(redact("not a url"), "not a url");
    }
}
