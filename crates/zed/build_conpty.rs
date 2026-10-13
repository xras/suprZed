use std::io::Read;
use std::path::Path;
use std::process::Command;

// Keep the package identity in the cache key so upgrades cannot reuse old binaries.
const PACKAGE_URL: &str = "https://github.com/microsoft/terminal/releases/download/v1.25.1912.0/Microsoft.Windows.Console.ConPTY.1.25.260710002-preview.nupkg";

fn is_executable(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut signature = [0; 2];
    file.read_exact(&mut signature).is_ok()
        && signature == *b"MZ"
        && file.metadata().is_ok_and(|metadata| metadata.len() > 64)
}

fn cache_matches(marker: &Path, binaries: &[&Path]) -> bool {
    std::fs::read_to_string(marker).is_ok_and(|value| value == PACKAGE_URL)
        && binaries.iter().all(|path| is_executable(path))
}

fn powershell_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

fn run_powershell(script: &str) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "ConPTY preparation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

fn copy_if_changed(source: &Path, destination: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(source)?;
    match std::fs::read(destination) {
        Ok(existing) if existing == bytes => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::copy(source, destination)?;
    Ok(())
}

pub fn prepare(out_dir: &Path, architecture: &str) -> Result<(), Box<dyn std::error::Error>> {
    let architecture = match architecture {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return Ok(()),
    };
    let target_dir = out_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or("cannot locate the console runtime destination")?;
    let extract_dir = out_dir.join("conpty");
    let marker = out_dir.join("conpty-package-url.txt");
    let conpty = extract_dir.join(format!("runtimes/win-{architecture}/native/conpty.dll"));
    let console = extract_dir.join(format!(
        "build/native/runtimes/{architecture}/OpenConsole.exe"
    ));
    if !cache_matches(&marker, &[&conpty, &console]) {
        match std::fs::remove_file(&marker) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let archive = out_dir.join("conpty-download.zip");
        run_powershell(&format!(
            "$ErrorActionPreference = 'Stop'; $ProgressPreference = 'SilentlyContinue'; \
             [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; \
             Invoke-WebRequest -Uri '{PACKAGE_URL}' -OutFile {}; \
             Expand-Archive -LiteralPath {} -DestinationPath {} -Force",
            powershell_literal(&archive),
            powershell_literal(&archive),
            powershell_literal(&extract_dir),
        ))?;
        if !is_executable(&conpty) || !is_executable(&console) {
            return Err("ConPTY package did not contain valid console runtime binaries".into());
        }
        std::fs::write(&marker, PACKAGE_URL)?;
        std::fs::remove_file(archive)?;
        println!("Downloaded and cached ConPTY runtime");
    } else {
        println!("Reusing cached ConPTY runtime");
    }
    copy_if_changed(&conpty, &target_dir.join("conpty.dll"))?;
    copy_if_changed(&console, &target_dir.join("OpenConsole.exe"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory() -> std::io::Result<std::path::PathBuf> {
        let directory = std::env::temp_dir().join(format!(
            "suprzed-conpty-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory)?;
        Ok(directory)
    }

    #[test]
    fn missing_truncated_or_different_package_invalidates_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = temporary_directory()?;
        let marker = directory.join("package");
        let binary = directory.join("runtime");
        std::fs::write(&marker, PACKAGE_URL)?;
        assert!(!cache_matches(&marker, &[&binary]));
        std::fs::write(&binary, b"MZ")?;
        assert!(!cache_matches(&marker, &[&binary]));
        let executable = [b"MZ".as_slice(), &[0; 100]].concat();
        std::fs::write(&binary, executable)?;
        assert!(cache_matches(&marker, &[&binary]));
        std::fs::write(&marker, "older-package")?;
        assert!(!cache_matches(&marker, &[&binary]));
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn unchanged_runtime_preserves_destination_timestamp() -> std::io::Result<()> {
        let directory = temporary_directory()?;
        let source = directory.join("source");
        let destination = directory.join("destination");
        std::fs::write(&source, b"runtime")?;
        copy_if_changed(&source, &destination)?;
        std::fs::File::options()
            .write(true)
            .open(&destination)?
            .set_times(std::fs::FileTimes::new().set_modified(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000),
            ))?;
        let modified = std::fs::metadata(&destination)?.modified()?;
        copy_if_changed(&source, &destination)?;
        assert_eq!(modified, std::fs::metadata(&destination)?.modified()?);
        std::fs::write(&source, b"updated-runtime")?;
        copy_if_changed(&source, &destination)?;
        assert_eq!(std::fs::read(&destination)?, b"updated-runtime");
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn powershell_paths_escape_apostrophes() {
        assert_eq!(
            powershell_literal(Path::new("C:/developer's cache/conpty.zip")),
            "'C:/developer''s cache/conpty.zip'"
        );
    }
}
