use super::super::App;

impl App {
    pub(in crate::app) fn looks_like_session_file(path: &str) -> bool {
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };

        // save-session may use gzip compression; ActiveSessionManager owns
        // decompression, so the magic header is enough for classification.
        if bytes.starts_with(&[0x1f, 0x8b]) {
            return true;
        }
        let content = String::from_utf8_lossy(&bytes);

        content.lines().any(|line| {
            let property = line.trim_start();
            line.starts_with([' ', '\t']) && property.starts_with("GID=")
        })
    }

    /// Load configuration from environment variables.
    pub async fn load_env(&mut self) {
        let mut conf = self.config.write().await;
        conf.load_env().await;
    }

    /// Load environment and file configuration according to CLI startup
    /// precedence. `--no-conf` suppresses both the default and explicit file.
    pub(in crate::app) async fn load_startup_config(
        &mut self,
        no_conf: bool,
        path: Option<&str>,
    ) -> std::result::Result<(), String> {
        if !no_conf {
            self.load_config_file(path).await?;
        }
        self.load_env().await;
        Ok(())
    }

    /// Load configuration from a file.
    ///
    /// If no path is provided, looks for ~/.aria2/aria2.conf
    /// Matching original aria2 behavior (option_processing.cc):
    /// - `HOME` first on all platforms, then USERPROFILE, then HOMEDRIVE+HOMEPATH.
    /// - When `--conf-path` is explicitly given and file not found → error.
    /// - When default path is not found → silently skip (graceful fallback).
    pub async fn load_config_file(
        &mut self,
        path: Option<&str>,
    ) -> std::result::Result<(), String> {
        let conf_path = if let Some(p) = path {
            // --conf-path explicitly given: error if file doesn't exist
            // (matches original aria2 option_processing.cc lines 254-260)
            if !std::path::Path::new(p).exists() {
                let msg = format!("Config file not found: {}", p);
                eprintln!("[-] {}", msg);
                return Err(msg);
            }
            p.to_string()
        } else {
            // Home resolution matching original aria2 util.cc getHomeDir():
            // 1. HOME (primary on all platforms)
            // 2. USERPROFILE (Windows fallback)
            // 3. HOMEDRIVE+HOMEPATH (last resort Windows fallback)
            // 4. "." (fallback if nothing works)
            let candidates = crate::app::paths::default_config_candidates();
            let Some(candidate) = candidates.into_iter().find(|candidate| candidate.exists())
            else {
                return Ok(());
            };
            candidate.to_string_lossy().into_owned()
        };

        let mut conf = self.config.write().await;
        conf.load_file(&conf_path).await;
        if conf.has_errors() {
            let details = conf
                .errors()
                .iter()
                .enumerate()
                .map(|(index, error)| {
                    if let Some(context) = conf.parser().error_context(index) {
                        format!(
                            "{}:{}: {} -> {}",
                            conf_path, context.line, context.content, error
                        )
                    } else {
                        error.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!(
                "Failed to parse config file '{}': {}",
                conf_path, details
            ));
        }
        Ok(())
    }
}
