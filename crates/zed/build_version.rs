use std::path::Path;
use std::process::Command;

pub struct BuildVersion {
    pub version: String,
    pub origin: &'static str,
    pub commit: String,
    pub source_status: String,
}

pub fn parse_version(value: &str) -> Result<[u64; 3], String> {
    let components = value.split('.').collect::<Vec<_>>();
    if components.len() != 3 {
        return Err("expected three numeric version components".into());
    }
    let mut numbers = [0; 3];
    for (number, component) in numbers.iter_mut().zip(components) {
        if component.is_empty()
            || !component.bytes().all(|byte| byte.is_ascii_digit())
            || (component.len() > 1 && component.starts_with('0'))
        {
            return Err("version components must be canonical unsigned integers".into());
        }
        *number = component
            .parse()
            .map_err(|_| "version component exceeds u64")?;
    }
    Ok(numbers)
}

fn git(source: &Path, arguments: &[&str]) -> Option<String> {
    let repository = Command::new("git")
        .current_dir(source)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !repository.status.success()
        || std::fs::canonicalize(String::from_utf8_lossy(&repository.stdout).trim()).ok()?
            != std::fs::canonicalize(source).ok()?
    {
        return None;
    }
    let output = Command::new("git")
        .current_dir(source)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(arguments)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub fn load(source: &Path) -> Result<BuildVersion, String> {
    let baseline = std::fs::read_to_string(source.join("SUPRZED_VERSION"))
        .map_err(|error| format!("read SUPRZED_VERSION: {error}"))?;
    let baseline = baseline.trim();
    let series = parse_version(baseline)?;
    let commit = git(source, &["rev-parse", "HEAD"])
        .filter(|commit| commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".into());
    let source_status = git(
        source,
        &["status", "--porcelain", "--untracked-files=normal"],
    )
    .map(|status| if status.is_empty() { "clean" } else { "dirty" })
    .unwrap_or("unknown")
    .to_string();
    let environment = |name| match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(format!("invalid {name}: {error}")),
    };
    let managed_version = environment("SUPRZED_BUILD_VERSION")?;
    let origin = environment("SUPRZED_BUILD_ORIGIN")?;
    let (version, origin) = match (managed_version, origin) {
        (None, None) => {
            let short_commit = commit.chars().take(10).collect::<String>();
            let suffix = if source_status == "dirty" { ".dirty" } else { "" };
            (format!("{baseline}+local.{short_commit}{suffix}"), "local")
        }
        (Some(version), Some(origin)) if origin == "managed" => {
            let managed = parse_version(&version)?;
            if managed[..2] != series[..2] {
                return Err("managed version does not match SUPRZED_VERSION series".into());
            }
            (version, "managed")
        }
        _ => return Err("managed builds require SUPRZED_BUILD_VERSION and SUPRZED_BUILD_ORIGIN=managed together".into()),
    };
    Ok(BuildVersion {
        version,
        origin,
        commit,
        source_status,
    })
}

pub fn watch_git(source: &Path) {
    for arguments in [
        vec!["rev-parse", "--git-path", "HEAD"],
        vec!["rev-parse", "--git-path", "logs/HEAD"],
    ] {
        if let Some(path) = git(source, &arguments) {
            let path = source.join(path);
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if let Some(reference) = git(source, &["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git(source, &["rev-parse", "--git-path", &reference])
    {
        println!("cargo:rerun-if-changed={}", source.join(path).display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_versions_and_large_counters() {
        assert_eq!(parse_version("0.1.1935").unwrap(), [0, 1, 1935]);
        assert_eq!(
            parse_version("0.1.18446744073709551615").unwrap(),
            [0, 1, u64::MAX]
        );
        for value in [
            "0.1",
            "0.1.0.0",
            "0.01.0",
            "0.1.-1",
            "0.1.0+local",
            "0.1.18446744073709551616",
        ] {
            assert!(parse_version(value).is_err(), "{value}");
        }
    }
}
