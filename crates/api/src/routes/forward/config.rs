//! Forwarder settings: where cloud-platform's edge door is and the shared
//! secret the forwarder sends so the edge can recognise cloud-api traffic.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use database::cp_client::config::check_base_url;

const URL_VAR: &str = "CLOUD_PLATFORM_EDGE_URL";
const SECRET_FILE_VAR: &str = "CLOUD_PLATFORM_FORWARDER_SECRET_FILE";

/// Settings for the forwarder. Absent (`None`) when neither variable is set.
#[derive(Clone)]
pub struct ForwarderConfig {
    pub base_url: String,
    pub secret: String,
    /// Fixed in code; tests shorten it.
    pub connect_timeout: Duration,
    /// Above the 30 s limit cloud-platform applies itself, so its own 504
    /// reaches the caller. Fixed in code; tests shorten it.
    pub total_timeout: Duration,
    /// Longest the caller may take to send the request body. Fixed in code;
    /// tests shorten it.
    pub body_read_timeout: Duration,
}

impl fmt::Debug for ForwarderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            base_url,
            secret: _,
            connect_timeout,
            total_timeout,
            body_read_timeout,
        } = self;
        f.debug_struct("ForwarderConfig")
            .field("base_url", base_url)
            .field("secret", &"<redacted>")
            .field("connect_timeout", connect_timeout)
            .field("total_timeout", total_timeout)
            .field("body_read_timeout", body_read_timeout)
            .finish()
    }
}

impl ForwarderConfig {
    pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
    pub const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(35);
    pub const DEFAULT_BODY_READ_TIMEOUT: Duration = Duration::from_secs(10);

    /// `Ok(None)` when neither variable is set. Setting exactly one is an
    /// error naming the missing one; errors never include a value.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let read = |name: &str| {
            get(name)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let (base_url, secret_file) = match (read(URL_VAR), read(SECRET_FILE_VAR)) {
            (None, None) => return Ok(None),
            (Some(_), None) => {
                return Err(format!(
                    "{SECRET_FILE_VAR} is required when {URL_VAR} is set"
                ))
            }
            (None, Some(_)) => {
                return Err(format!(
                    "{URL_VAR} is required when {SECRET_FILE_VAR} is set"
                ))
            }
            (Some(url), Some(file)) => (url, file),
        };
        check_base_url(&base_url).map_err(|e| e.replace("CLOUD_PLATFORM_URL", URL_VAR))?;
        let secret = read_secret_file(Path::new(&secret_file)).map_err(|e| e.to_string())?;
        Ok(Some(Self {
            base_url,
            secret,
            connect_timeout: Self::DEFAULT_CONNECT_TIMEOUT,
            total_timeout: Self::DEFAULT_TOTAL_TIMEOUT,
            body_read_timeout: Self::DEFAULT_BODY_READ_TIMEOUT,
        }))
    }
}

/// Read the forwarder secret, refusing a file that group or others can read.
/// The file is opened once and the mode is checked on that handle.
fn read_secret_file(path: &Path) -> anyhow::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .map_err(|_| anyhow::anyhow!("{SECRET_FILE_VAR} is not readable"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file
            .metadata()
            .map_err(|_| anyhow::anyhow!("{SECRET_FILE_VAR} is not readable"))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            anyhow::bail!("{SECRET_FILE_VAR} must be owner-only (0400 or 0600)");
        }
    }
    let mut secret = String::new();
    file.read_to_string(&mut secret)
        .map_err(|_| anyhow::anyhow!("{SECRET_FILE_VAR} is not readable"))?;
    let secret = secret.trim().to_string();
    if secret.is_empty() {
        anyhow::bail!("{SECRET_FILE_VAR} is empty");
    }
    Ok(secret)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[cfg(unix)]
    fn secret_file(contents: &str, mode: u32) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fwd-secret-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("secret");
        std::fs::write(&file, contents).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
        (dir, file)
    }

    #[test]
    fn both_unset_is_none() {
        assert!(ForwarderConfig::from_lookup(lookup(&[])).unwrap().is_none());
        // Blank counts as unset.
        let blank = lookup(&[(URL_VAR, "  "), (SECRET_FILE_VAR, "")]);
        assert!(ForwarderConfig::from_lookup(blank).unwrap().is_none());
    }

    #[test]
    fn one_set_is_an_error_naming_the_other() {
        let err =
            ForwarderConfig::from_lookup(lookup(&[(URL_VAR, "https://cp.example")])).unwrap_err();
        assert!(err.contains(SECRET_FILE_VAR));
        let err =
            ForwarderConfig::from_lookup(lookup(&[(SECRET_FILE_VAR, "/x/secret")])).unwrap_err();
        assert!(err.contains(URL_VAR));
    }

    #[test]
    fn url_must_be_https_or_loopback() {
        let err = ForwarderConfig::from_lookup(lookup(&[
            (URL_VAR, "http://cp.example"),
            (SECRET_FILE_VAR, "/x/secret"),
        ]))
        .unwrap_err();
        assert!(err.contains(URL_VAR));
        assert!(!err.contains("cp.example"));
    }

    #[cfg(unix)]
    #[test]
    fn loads_secret_and_defaults() {
        let (dir, file) = secret_file("s3cret\n", 0o600);
        let config = ForwarderConfig::from_lookup(lookup(&[
            (URL_VAR, "http://127.0.0.1:9"),
            (SECRET_FILE_VAR, file.to_str().unwrap()),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(config.secret, "s3cret");
        assert_eq!(config.connect_timeout, Duration::from_secs(2));
        assert_eq!(config.total_timeout, Duration::from_secs(35));
        assert_eq!(config.body_read_timeout, Duration::from_secs(10));
        assert!(!format!("{config:?}").contains("s3cret"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn secret_file_must_be_private_and_non_empty() {
        let (dir, file) = secret_file("s3cret", 0o644);
        assert!(read_secret_file(&file).is_err());
        let (dir2, empty) = secret_file(" \n", 0o600);
        assert!(read_secret_file(&empty).is_err());
        let err = read_secret_file(&dir.join("missing"))
            .unwrap_err()
            .to_string();
        assert!(err.contains(SECRET_FILE_VAR));
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&dir2).unwrap();
    }
}
