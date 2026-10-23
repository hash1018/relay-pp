//! Who may do what: the manager, by the API token the configuration names;
//! players, as users the configuration lists or by a token the manager
//! signs.
//!
//! A play token is a JSON Web Token signed with HS256 by the secret the
//! configuration names — any JWT library makes one — whose claims say until
//! when it holds and which channels it lets its holder play live or watch
//! the recordings of:
//!
//! ```json
//! {"exp": 1792735200, "play": ["cam1", "floor2-*"], "recordings": ["cam1"]}
//! ```
//!
//! A channel is named outright or by a pattern, `*` standing for anything.
//! A player presents a token as a `Bearer` (WHEP, HTTP), or as `?token=` in
//! its URL: an RTSP player's — `rtsp://host:8554/cam1?token=…`, a signed
//! token being longer than some players keep of a URL's user name — or an
//! HLS player's that sets no header. A user presents its name and password:
//! Basic over HTTP, Digest or Basic over RTSP. The API token lets its holder do
//! anything, playing included.
//!
//! No secret — token, password — is ever logged or put in an answer.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

use media_pp::elements::Credentials;

use crate::config::AuthConfig;

/// What a player may do with a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Right {
    /// Play it live, over RTSP or WebRTC.
    Play,
    /// Watch and list its recordings.
    Recordings,
    /// Publish into it, over RTSP, RTMP or WHIP.
    Publish,
}

/// What a client presented to say who it is.
pub(crate) enum Presented<'a> {
    None,
    /// A token: the API's, or a play token.
    Token(&'a str),
    /// A user's name and password.
    Password {
        user: &'a str,
        password: &'a str,
    },
    /// An RTSP player's Digest response: a user's name, which may be a play
    /// token, and a hash of the password.
    Digest(&'a media_pp::elements::RtspDigest),
}

/// Who may do what, as the configuration says.
pub(crate) struct Auth {
    api_token: String,
    secret: Option<Vec<u8>>,
    users: Vec<User>,
}

struct User {
    name: String,
    password: String,
    play: Vec<String>,
    recordings: Vec<String>,
    publish: Vec<String>,
}

/// What a play token says.
#[derive(Deserialize)]
struct Claims {
    /// Until when it holds, in seconds since the Unix epoch: a token without
    /// one holds nothing.
    exp: u64,
    #[serde(default)]
    play: Vec<String>,
    #[serde(default)]
    recordings: Vec<String>,
    #[serde(default)]
    publish: Vec<String>,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
}

impl Auth {
    pub(crate) fn new(config: &AuthConfig) -> Self {
        Self {
            api_token: config.api_token.clone(),
            secret: config
                .token_secret
                .as_ref()
                .map(|secret| secret.as_bytes().to_vec()),
            users: config
                .users
                .iter()
                .map(|user| User {
                    name: user.name.clone(),
                    password: user.password.clone(),
                    play: user.play.clone(),
                    recordings: user.recordings.clone(),
                    publish: user.publish.clone(),
                })
                .collect(),
        }
    }

    /// Whether `presented` is the manager's: the API token.
    pub(crate) fn is_manager(&self, presented: &Presented<'_>) -> bool {
        matches!(presented, Presented::Token(token) if same(token, &self.api_token))
    }

    /// Whether `presented` lets its holder do `right` with `channel`: the
    /// API token does anything; a play token what its claims say, until it
    /// expires; a user what its patterns say, its password right.
    pub(crate) fn allows(&self, presented: &Presented<'_>, right: Right, channel: &str) -> bool {
        if self.is_manager(presented) {
            return true;
        }
        match presented {
            Presented::None => false,
            Presented::Token(token) => self.token_allows(token, right, channel),
            Presented::Password { user, password } => {
                self.token_allows(user, right, channel)
                    || self.user(user).is_some_and(|known| {
                        same(password, &known.password) && known.covers(right, channel)
                    })
            }
            Presented::Digest(digest) => {
                self.token_allows(digest.user(), right, channel)
                    || self.user(digest.user()).is_some_and(|known| {
                        digest.verifies(&known.password) && known.covers(right, channel)
                    })
            }
        }
    }

    fn user(&self, name: &str) -> Option<&User> {
        self.users.iter().find(|user| user.name == name)
    }

    fn token_allows(&self, token: &str, right: Right, channel: &str) -> bool {
        self.claims(token).is_some_and(|claims| {
            let patterns = match right {
                Right::Play => &claims.play,
                Right::Recordings => &claims.recordings,
                Right::Publish => &claims.publish,
            };
            covers(patterns, channel)
        })
    }

    /// What `token` says, where it is a play token signed with the secret
    /// and not expired.
    fn claims(&self, token: &str) -> Option<Claims> {
        let secret = self.secret.as_ref()?;
        let (signed, signature) = token.rsplit_once('.')?;
        let (header, payload) = signed.split_once('.')?;
        let header: Header = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).ok()?).ok()?;
        if header.alg != "HS256" {
            return None;
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).ok()?;
        mac.update(signed.as_bytes());
        mac.verify_slice(&URL_SAFE_NO_PAD.decode(signature).ok()?)
            .ok()?;
        let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        (claims.exp > now).then_some(claims)
    }
}

impl User {
    fn covers(&self, right: Right, channel: &str) -> bool {
        match right {
            Right::Play => covers(&self.play, channel),
            Right::Recordings => covers(&self.recordings, channel),
            Right::Publish => covers(&self.publish, channel),
        }
    }
}

/// What an RTSP player presented, as its server puts it.
pub(crate) fn from_credentials(credentials: &Credentials) -> Presented<'_> {
    match credentials {
        Credentials::None => Presented::None,
        Credentials::Bearer(token) => Presented::Token(token),
        Credentials::Basic { user, password } => Presented::Password { user, password },
        Credentials::Digest(digest) => Presented::Digest(digest),
    }
}

/// What an HTTP `Authorization` header presents.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FromHeader {
    /// `Bearer`.
    Token(String),
    /// `Basic`.
    User { name: String, password: String },
}

/// What an HTTP `Authorization` header's value presents; `None` for a
/// scheme taken nowhere here.
pub(crate) fn from_header(value: &str) -> Option<FromHeader> {
    let (scheme, rest) = value.trim().split_once(' ')?;
    let rest = rest.trim();
    if scheme.eq_ignore_ascii_case("bearer") && !rest.is_empty() {
        return Some(FromHeader::Token(rest.to_owned()));
    }
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = String::from_utf8(STANDARD.decode(rest).ok()?).ok()?;
        let (name, password) = decoded.split_once(':')?;
        return Some(FromHeader::User {
            name: name.to_owned(),
            password: password.to_owned(),
        });
    }
    None
}

/// Whether one of `patterns` names `channel`: outright, or with `*`
/// standing for anything, any number of times.
fn covers(patterns: &[String], channel: &str) -> bool {
    patterns.iter().any(|pattern| matches(pattern, channel))
}

fn matches(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        // No `*`: the whole name.
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

/// Whether `a` and `b` are the same, taking as long whichever byte they
/// first differ at: a secret is not given away by how soon it is refused.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |differ, (x, y)| differ | (x ^ y))
            == 0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::UserConfig;

    /// A play token of `claims`, signed with `secret`.
    pub(crate) fn token(secret: &str, claims: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims);
        let signed = format!("{header}.{payload}");
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(signed.as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        format!("{signed}.{signature}")
    }

    fn later() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600
    }

    fn auth() -> Auth {
        Auth::new(&AuthConfig {
            api_token: "manager".to_owned(),
            token_secret: Some("secret".to_owned()),
            users: vec![UserConfig {
                name: "guard".to_owned(),
                password: "pw".to_owned(),
                play: vec!["floor2-*".to_owned()],
                recordings: vec![],
                publish: vec![],
            }],
        })
    }

    #[test]
    fn a_pattern_names_what_it_matches() {
        assert!(matches("cam1", "cam1"));
        assert!(!matches("cam1", "cam10"));
        assert!(matches("*", "anything"));
        assert!(matches("floor2-*", "floor2-cam1"));
        assert!(!matches("floor2-*", "floor3-cam1"));
        assert!(matches("*-cam*", "floor2-cam1"));
        assert!(!matches("*-cam*", "floor2-door"));
    }

    #[test]
    fn the_api_token_does_anything() {
        let auth = auth();
        let manager = Presented::Token("manager");
        assert!(auth.is_manager(&manager));
        assert!(auth.allows(&manager, Right::Recordings, "cam1"));
        assert!(!auth.is_manager(&Presented::Token("managers")));
        assert!(!auth.allows(&Presented::None, Right::Play, "cam1"));
    }

    #[test]
    fn a_play_token_holds_what_it_says_until_it_expires() {
        let auth = auth();
        let claims = format!(r#"{{"exp":{},"play":["cam*"]}}"#, later());
        let signed = token("secret", &claims);
        assert!(auth.allows(&Presented::Token(&signed), Right::Play, "cam1"));
        assert!(!auth.allows(&Presented::Token(&signed), Right::Play, "door1"));
        assert!(!auth.allows(&Presented::Token(&signed), Right::Recordings, "cam1"));
        // As an RTSP player's user name.
        let as_user = Presented::Password {
            user: &signed,
            password: "x",
        };
        assert!(auth.allows(&as_user, Right::Play, "cam1"));

        let forged = token("not the secret", &claims);
        assert!(!auth.allows(&Presented::Token(&forged), Right::Play, "cam1"));
        let expired = token("secret", r#"{"exp":1,"play":["*"]}"#);
        assert!(!auth.allows(&Presented::Token(&expired), Right::Play, "cam1"));
        let unbounded = token("secret", r#"{"play":["*"]}"#);
        assert!(!auth.allows(&Presented::Token(&unbounded), Right::Play, "cam1"));
        assert!(!auth.allows(&Presented::Token("not.a.token"), Right::Play, "cam1"));
    }

    #[test]
    fn a_user_does_what_its_patterns_say_with_its_password() {
        let auth = auth();
        let guard = |password| Presented::Password {
            user: "guard",
            password,
        };
        assert!(auth.allows(&guard("pw"), Right::Play, "floor2-cam1"));
        assert!(!auth.allows(&guard("pw"), Right::Play, "floor3-cam1"));
        assert!(!auth.allows(&guard("pw"), Right::Recordings, "floor2-cam1"));
        assert!(!auth.allows(&guard("wrong"), Right::Play, "floor2-cam1"));
    }

    #[test]
    fn a_header_says_what_it_presents() {
        assert_eq!(
            from_header("Bearer abc.def"),
            Some(FromHeader::Token("abc.def".to_owned()))
        );
        // "guard:p:w" — a password may hold a colon.
        assert_eq!(
            from_header("Basic Z3VhcmQ6cDp3"),
            Some(FromHeader::User {
                name: "guard".to_owned(),
                password: "p:w".to_owned()
            })
        );
        assert_eq!(from_header("Digest username=x"), None);
    }
}
