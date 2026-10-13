use crate::ReleaseChannel;
use gpui::{App, Global};
use semver::Version;
use std::{fmt, str::FromStr};

/// Whether the application was numbered by the managed wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildOrigin {
    /// A numbered application build.
    Managed,
    /// An ordinary Cargo build.
    Local,
}

impl fmt::Display for BuildOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Managed => "managed",
            Self::Local => "local",
        })
    }
}

impl FromStr for BuildOrigin {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "managed" => Ok(Self::Managed),
            "local" => Ok(Self::Local),
            _ => Err(format!("invalid suprZed build origin: {value}")),
        }
    }
}

/// Git checkout status sampled when the build script runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceStatus {
    /// No uncommitted changes were observed.
    Clean,
    /// Uncommitted changes were observed.
    Dirty,
    /// No source repository could be identified.
    Unknown,
}

impl fmt::Display for SourceStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Clean => "clean",
            Self::Dirty => "dirty",
            Self::Unknown => "unknown",
        })
    }
}

impl FromStr for SourceStatus {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "clean" => Ok(Self::Clean),
            "dirty" => Ok(Self::Dirty),
            "unknown" => Ok(Self::Unknown),
            _ => Err(format!("invalid suprZed source status: {value}")),
        }
    }
}

/// The fork's embedded identity, independent of Zed's version and update channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuprZedVersion {
    version: Version,
    origin: BuildOrigin,
    commit: String,
    source_status: SourceStatus,
}

impl Global for SuprZedVersion {}

impl SuprZedVersion {
    /// Validates the compile-time metadata supplied by the application crate.
    pub fn new(
        version: &str,
        origin: &str,
        commit: &str,
        source_status: &str,
    ) -> Result<Self, String> {
        let version = Version::parse(version).map_err(|error| error.to_string())?;
        let origin = origin.parse::<BuildOrigin>()?;
        let source_status = source_status.parse::<SourceStatus>()?;
        if !version.pre.is_empty()
            || (commit != "unknown"
                && (commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())))
        {
            return Err("invalid suprZed source metadata".into());
        }
        if origin == BuildOrigin::Managed && !version.build.is_empty() {
            return Err("managed versions must have three numeric components".into());
        }
        if origin == BuildOrigin::Local {
            let short_commit = commit.chars().take(10).collect::<String>();
            let suffix = if source_status == SourceStatus::Dirty {
                ".dirty"
            } else {
                ""
            };
            if version.build.as_str() != format!("local.{short_commit}{suffix}") {
                return Err("local version metadata does not match its source".into());
            }
        }
        Ok(Self {
            version,
            origin,
            commit: commit.into(),
            source_status,
        })
    }

    /// Returns the fork version, including local build metadata when present.
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Returns the managed or local build origin.
    pub fn origin(&self) -> BuildOrigin {
        self.origin
    }

    /// Returns the source commit, or `unknown` for a source archive.
    pub fn commit(&self) -> &str {
        &self.commit
    }

    /// Returns the clean, dirty or unknown source status captured at build time.
    pub fn source_status(&self) -> SourceStatus {
        self.source_status
    }

    /// Returns the application identity if initialized by the app.
    pub fn try_global(cx: &App) -> Option<Self> {
        cx.try_global::<Self>().cloned()
    }

    /// Formats the build origin and source status for all diagnostic surfaces.
    pub fn build_summary(&self) -> String {
        format!("{} ({})", self.origin, self.source_status)
    }

    /// Formats the additive fork rows shown in About and copied details.
    pub fn about_details(&self) -> String {
        format!("suprZed {}\nBuild: {}", self.version, self.build_summary())
    }

    /// Formats fork and upstream identity for startup and CLI diagnostics.
    pub fn details(&self, upstream: &Version, channel: ReleaseChannel) -> String {
        format!(
            "suprZed {}\nBased on Zed {}\nCommit: {}\nChannel: {}\nBuild: {}",
            self.version,
            upstream,
            self.commit,
            channel.dev_name(),
            self.build_summary()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_metadata_preserves_upstream_version_and_channel() {
        let upstream = semver::Version::parse("1.25.0+dev.abcdef").unwrap();
        let fork = SuprZedVersion::new("0.1.1935", "managed", "unknown", "unknown").unwrap();
        assert_eq!(fork.version().patch, 1935);
        assert_eq!(fork.origin(), BuildOrigin::Managed);
        assert_eq!(fork.source_status(), SourceStatus::Unknown);
        assert_eq!(
            fork.about_details(),
            "suprZed 0.1.1935\nBuild: managed (unknown)"
        );
        assert_eq!(
            fork.details(&upstream, ReleaseChannel::Dev),
            "suprZed 0.1.1935\nBased on Zed 1.25.0+dev.abcdef\nCommit: unknown\nChannel: dev\nBuild: managed (unknown)"
        );
        assert_eq!(upstream.to_string(), "1.25.0+dev.abcdef");
        assert_eq!(ReleaseChannel::Dev.dev_name(), "dev");
    }

    #[test]
    fn local_and_unknown_source_metadata() {
        let commit = "abcdef1234567890abcdef1234567890abcdef12";
        for (version, commit, status) in [
            ("0.1.0+local.abcdef1234", commit, "clean"),
            ("0.1.0+local.abcdef1234.dirty", commit, "dirty"),
            ("0.1.0+local.unknown", "unknown", "unknown"),
        ] {
            let fork = SuprZedVersion::new(version, "local", commit, status).unwrap();
            assert_eq!(fork.version().to_string(), version);
            assert_eq!(fork.origin(), BuildOrigin::Local);
            assert_eq!(
                fork.source_status(),
                status.parse::<SourceStatus>().unwrap()
            );
        }
    }

    #[test]
    fn invalid_fork_metadata_is_rejected() {
        for (version, origin, commit, status) in [
            ("0.1.0+local.unknown", "managed", "unknown", "unknown"),
            ("0.1.0", "local", "unknown", "unknown"),
            ("0.1.0-rc.1", "managed", "unknown", "unknown"),
            ("0.1.0", "official", "unknown", "unknown"),
            ("0.1.0", "managed", "bad-sha", "clean"),
            ("0.1.0", "managed", "unknown", "bad-status"),
            ("0.1.18446744073709551616", "managed", "unknown", "unknown"),
        ] {
            assert!(SuprZedVersion::new(version, origin, commit, status).is_err());
        }
        let smaller = SuprZedVersion::new("0.1.9", "managed", "unknown", "unknown").unwrap();
        let larger = SuprZedVersion::new("0.1.1935", "managed", "unknown", "unknown").unwrap();
        assert!(larger.version() > smaller.version());
    }
}
