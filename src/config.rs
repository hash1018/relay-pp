//! The configuration file: what relay-pp listens on — see `docs/design.md`,
//! "Configuration". Channels are not in it: they are made and removed over
//! the API.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

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
    /// `[record]`'s lengths do not go together.
    #[error("record: {0}")]
    Record(&'static str),
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
    /// Recording, for the channels made with `"record": true`.
    #[serde(default)]
    pub record: RecordConfig,
    /// What is converted for a way out that cannot carry what a camera
    /// sends, unless a channel says otherwise.
    #[serde(default)]
    pub convert: ConvertConfig,
}

/// `[convert]`, and a channel's own say over it: what is converted for a
/// way out — WebRTC players, a recording — that cannot carry a track.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvertConfig {
    #[serde(default)]
    pub sound: SoundConversion,
    #[serde(default)]
    pub video: VideoConversion,
}

/// Whether sound a way out cannot carry is converted for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SoundConversion {
    /// Converted — to Opus for WebRTC, AAC for a recording — on the
    /// processor, which it costs little of.
    #[default]
    Auto,
    /// Left out of that way out: WebRTC players of a camera's AAC are sent
    /// its pictures alone.
    Off,
}

/// Whether pictures a way out cannot carry are converted for it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoConversion {
    /// Converted where a GPU encoder takes them, and refused where none
    /// does — none there, or each of its sessions taken — rather than
    /// encoded on the processor, a core or two a camera.
    #[default]
    Gpu,
    /// Converted on the GPU where one takes them, and on the processor
    /// where none does.
    Any,
    /// Never converted: that way out is refused.
    Off,
}

/// What a channel says of its conversions, over `[convert]`: each left out
/// is the server's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvertChoice {
    #[serde(default)]
    pub sound: Option<SoundConversion>,
    #[serde(default)]
    pub video: Option<VideoConversion>,
}

impl ConvertChoice {
    /// What it comes to, over `server`'s.
    pub fn over(self, server: ConvertConfig) -> ConvertConfig {
        ConvertConfig {
            sound: self.sound.unwrap_or(server.sound),
            video: self.video.unwrap_or(server.video),
        }
    }
}

/// `[record]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordConfig {
    /// Where recordings go: `<dir>/<channel>/<YYYY-MM-DD>/<HH-MM-SS>/`, in
    /// UTC.
    #[serde(default = "RecordConfig::default_dir")]
    pub dir: PathBuf,
    /// How long a segment is — what a player seeks to and starts at — cut
    /// at the next keyframe after it: `"4s"`.
    #[serde(default = "RecordConfig::default_segment", with = "duration")]
    pub segment: Duration,
    /// How long one recording directory runs before the next begins, at
    /// whole multiples of it in UTC — and what retention removes at a time:
    /// `"1h"`. A camera dropping, or coming back, begins one too.
    #[serde(default = "RecordConfig::default_chunk", with = "duration")]
    pub chunk: Duration,
    /// How long a recording is kept, from its last write: `"30d"`. Kept
    /// for ever where neither this nor `max_size` is set.
    #[serde(default, with = "optional_duration")]
    pub keep: Option<Duration>,
    /// How much every recording together may take, the oldest removed
    /// first: `"500GB"` (or `"GiB"`, 1024-based).
    #[serde(default, with = "optional_size")]
    pub max_size: Option<u64>,
}

impl RecordConfig {
    fn default_dir() -> PathBuf {
        PathBuf::from("recordings")
    }

    fn default_segment() -> Duration {
        Duration::from_secs(4)
    }

    fn default_chunk() -> Duration {
        Duration::from_secs(3600)
    }
}

impl Default for RecordConfig {
    fn default() -> Self {
        Self {
            dir: Self::default_dir(),
            segment: Self::default_segment(),
            chunk: Self::default_chunk(),
            keep: None,
            max_size: None,
        }
    }
}

/// `"500ms"`, `"4s"`, `"90m"`, `"1h"`, `"30d"`.
fn parse_duration(text: &str) -> Option<Duration> {
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().ok()?;
    let unit = match unit {
        "ms" => Duration::from_millis(1),
        "s" => Duration::from_secs(1),
        "m" => Duration::from_secs(60),
        "h" => Duration::from_secs(3600),
        "d" => Duration::from_secs(86_400),
        _ => return None,
    };
    unit.checked_mul(u32::try_from(number).ok()?)
}

/// `"500GB"`, `"2TiB"`, or a number of bytes.
fn parse_size(text: &str) -> Option<u64> {
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().ok()?;
    let unit: u64 = match unit {
        "" | "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "TB" => 1_000_000_000_000,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        "TiB" => 1 << 40,
        _ => return None,
    };
    number.checked_mul(unit)
}

mod duration {
    use serde::{Deserialize, Deserializer, de::Error};
    use std::time::Duration;

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let text = String::deserialize(d)?;
        super::parse_duration(&text)
            .ok_or_else(|| D::Error::custom(format!("{text:?} is not a duration like \"4s\"")))
    }
}

mod optional_duration {
    use serde::Deserializer;
    use std::time::Duration;

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Duration>, D::Error> {
        super::duration::deserialize(d).map(Some)
    }
}

mod optional_size {
    use serde::{Deserialize, Deserializer, de::Error};

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
        let text = String::deserialize(d)?;
        super::parse_size(&text)
            .map(Some)
            .ok_or_else(|| D::Error::custom(format!("{text:?} is not a size like \"500GB\"")))
    }
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
    /// What the URL `POST /api/v1/channels/{name}/play` answers with
    /// begins with — `"rtsp://cctv.example.com:8554"` — where players reach
    /// this machine by another address than the API's callers: behind a
    /// router, say. Where it is not set, the host the request came to and
    /// `listen`'s port.
    pub public_url: Option<String>,
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
            public_url: None,
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
        let record = &config.record;
        if record.segment.is_zero() {
            return Err(ConfigError::Record("a segment has a length"));
        }
        if record.chunk < record.segment {
            return Err(ConfigError::Record("a chunk is at least a segment long"));
        }
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
    fn recording_lengths_and_sizes_are_read_with_their_units() {
        let config = Config::parse(
            "[record]\ndir = \"/srv/rec\"\nsegment = \"2s\"\nchunk = \"30m\"\nkeep = \"30d\"\nmax_size = \"2TiB\"",
        )
        .unwrap();
        let record = &config.record;
        assert_eq!(record.dir, PathBuf::from("/srv/rec"));
        assert_eq!(record.segment, Duration::from_secs(2));
        assert_eq!(record.chunk, Duration::from_secs(1800));
        assert_eq!(record.keep, Some(Duration::from_secs(30 * 86_400)));
        assert_eq!(record.max_size, Some(2 << 40));
        let defaults = Config::parse("").unwrap().record;
        assert_eq!(
            (
                defaults.segment,
                defaults.chunk,
                defaults.keep,
                defaults.max_size
            ),
            (
                Duration::from_secs(4),
                Duration::from_secs(3600),
                None,
                None
            )
        );
        assert_eq!(parse_size("500GB"), Some(500_000_000_000));
        assert_eq!(parse_duration("500ms"), Some(Duration::from_millis(500)));
        for text in ["4", "4 s", "s", "4w", "-4s"] {
            assert_eq!(parse_duration(text), None, "{text:?}");
        }
        for text in [
            "[record]\nsegment = \"0s\"",
            "[record]\nchunk = \"1s\"",
            "[record]\nkeep = \"soon\"",
        ] {
            assert!(Config::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn the_example_file_is_read() {
        Config::parse(include_str!("../relay-pp.example.toml")).unwrap();
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
