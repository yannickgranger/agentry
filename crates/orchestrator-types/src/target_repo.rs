//! Canonical routing-key value object.
//!
//! [`TargetRepo`] is the Published Language of the Briefing context (see
//! `specs/concepts/target_repo.md`). It is the routing key for the daemon,
//! the cfdb keyspace selector, the credential-scope key, the workspace-mount
//! target, and the forge-URL source. Construction is monopolistic via
//! [`TargetRepo::from_str`].

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

/// Placeholder forge identifier bound at parse time when the input does not
/// carry an explicit `forge:` prefix. Brief 1b / a follow-up Configuration
/// council will replace this with real default-forge resolution from
/// `ForgeConfig::default_host`.
const PLACEHOLDER_FORGE: &str = "agency-default";

/// Maximum total byte length of a target-repo input string.
const MAX_TOTAL_LEN: usize = 200;

/// Maximum byte length of an individual segment (owner or repo).
const MAX_SEGMENT_LEN: usize = 64;

/// The canonical routing-key value object — see `specs/concepts/target_repo.md`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TargetRepo {
    forge: String,
    owner: String,
    repo: String,
}

impl TargetRepo {
    /// The resolved forge identifier (placeholder default when the input
    /// carried no `forge:` prefix).
    #[must_use]
    pub fn forge(&self) -> &str {
        &self.forge
    }

    /// The owner segment (non-empty, charset-validated).
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The repo segment (non-empty, charset-validated).
    #[must_use]
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// Filesystem- and keyspace-safe slug.
    ///
    /// Collision-resistant derivation: every literal `_` in the input is
    /// first doubled (`_` → `__`), then `<owner>/<repo>` is concatenated
    /// and any non-alphanumeric/`_` byte is replaced with `_`. The
    /// pre-encoding makes the function injective over the allowed
    /// charset — distinct `(owner, repo)` tuples produce distinct slugs.
    #[must_use]
    pub fn slug(&self) -> String {
        let owner_encoded = self.owner.replace('_', "__");
        let repo_encoded = self.repo.replace('_', "__");
        let temp = format!("{owner_encoded}/{repo_encoded}");
        temp.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// The cfdb keyspace name.
    ///
    /// Delegates to [`Self::slug`] today; the method exists so future
    /// divergence between slug and keyspace can land in one place.
    #[must_use]
    pub fn cfdb_keyspace(&self) -> String {
        self.slug()
    }

    /// Canonical clone-URL builder. The SOLE legitimate site for composing
    /// a `target_repo` into a `https://.../<owner>/<repo>.git` string.
    #[must_use]
    pub fn clone_url(&self, forge_host: &str) -> String {
        format!("https://{forge_host}/{}/{}.git", self.owner, self.repo)
    }

    /// Self-contained `forge:owner/repo` string for new internal call sites
    /// that want the qualified form. Existing wire callers stay on
    /// [`fmt::Display`].
    #[must_use]
    pub fn display_qualified(&self) -> String {
        format!("{}:{}/{}", self.forge, self.owner, self.repo)
    }
}

/// Typed parse error for [`TargetRepo::from_str`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetRepoParseError {
    Empty,
    MissingOwner,
    MissingRepo,
    OwnerInvalidChars,
    RepoInvalidChars,
    OwnerStartsWithDotOrDash,
    RepoStartsWithDotOrDash,
    TooLong,
    UnknownForgePrefix,
}

impl fmt::Display for TargetRepoParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            Self::Empty => "target_repo is empty",
            Self::MissingOwner => "target_repo missing owner segment",
            Self::MissingRepo => "target_repo missing repo segment",
            Self::OwnerInvalidChars => "target_repo owner contains invalid characters",
            Self::RepoInvalidChars => "target_repo repo contains invalid characters",
            Self::OwnerStartsWithDotOrDash => "target_repo owner starts with `.` or `-`",
            Self::RepoStartsWithDotOrDash => "target_repo repo starts with `.` or `-`",
            Self::TooLong => "target_repo exceeds maximum length",
            Self::UnknownForgePrefix => "target_repo has unknown forge prefix",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for TargetRepoParseError {}

impl FromStr for TargetRepo {
    type Err = TargetRepoParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() {
            return Err(TargetRepoParseError::Empty);
        }
        if s.len() > MAX_TOTAL_LEN {
            return Err(TargetRepoParseError::TooLong);
        }

        let (forge, body) = match s.split_once(':') {
            Some((prefix, rest)) => {
                if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_alphanumeric()) {
                    return Err(TargetRepoParseError::UnknownForgePrefix);
                }
                (prefix.to_string(), rest)
            }
            None => (PLACEHOLDER_FORGE.to_string(), s),
        };

        let (owner, repo) = body
            .split_once('/')
            .ok_or(TargetRepoParseError::MissingRepo)?;

        if owner.is_empty() {
            return Err(TargetRepoParseError::MissingOwner);
        }
        if repo.is_empty() {
            return Err(TargetRepoParseError::MissingRepo);
        }
        if repo.contains('/') {
            return Err(TargetRepoParseError::RepoInvalidChars);
        }

        validate_segment(owner, Segment::Owner)?;
        validate_segment(repo, Segment::Repo)?;

        Ok(Self {
            forge,
            owner: owner.to_string(),
            repo: repo.to_string(),
        })
    }
}

#[derive(Clone, Copy)]
enum Segment {
    Owner,
    Repo,
}

fn validate_segment(seg: &str, kind: Segment) -> Result<(), TargetRepoParseError> {
    if seg.len() > MAX_SEGMENT_LEN {
        return Err(TargetRepoParseError::TooLong);
    }
    let first = seg.as_bytes()[0];
    if first == b'.' || first == b'-' {
        return Err(match kind {
            Segment::Owner => TargetRepoParseError::OwnerStartsWithDotOrDash,
            Segment::Repo => TargetRepoParseError::RepoStartsWithDotOrDash,
        });
    }
    let valid = seg
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
    if !valid {
        return Err(match kind {
            Segment::Owner => TargetRepoParseError::OwnerInvalidChars,
            Segment::Repo => TargetRepoParseError::RepoInvalidChars,
        });
    }
    Ok(())
}

impl fmt::Display for TargetRepo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.repo)
    }
}

impl Serialize for TargetRepo {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&format_args!("{}/{}", self.owner, self.repo))
    }
}

impl<'de> Deserialize<'de> for TargetRepo {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = <String as Deserialize>::deserialize(deserializer)?;
        TargetRepo::from_str(&s).map_err(de::Error::custom)
    }
}
