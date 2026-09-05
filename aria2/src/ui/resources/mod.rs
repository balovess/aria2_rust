//! TOML-backed localized TUI resources.

use aria2_core::request::request_group::DownloadStatus;
use serde::Deserialize;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locale {
    English,
    SimplifiedChinese,
    TraditionalChinese,
    Japanese,
    Spanish,
    Russian,
    Hindi,
    Bengali,
    Tamil,
    Vietnamese,
    Thai,
    Indonesian,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Texts {
    pub title: String,
    pub empty: String,
    pub footer: String,
    pub add_prompt: String,
    pub filter_prompt: String,
    pub filtered: String,
    pub details: String,
    pub headers: Vec<String>,
    pub remote_headers: Vec<String>,
    pub detail_labels: Vec<String>,
    pub statuses: Vec<String>,
    pub page: String,
    pub error: String,
}

static ENGLISH: OnceLock<Texts> = OnceLock::new();
static SIMPLIFIED_CHINESE: OnceLock<Texts> = OnceLock::new();
static TRADITIONAL_CHINESE: OnceLock<Texts> = OnceLock::new();
static JAPANESE: OnceLock<Texts> = OnceLock::new();
static SPANISH: OnceLock<Texts> = OnceLock::new();
static RUSSIAN: OnceLock<Texts> = OnceLock::new();
static HINDI: OnceLock<Texts> = OnceLock::new();
static BENGALI: OnceLock<Texts> = OnceLock::new();
static TAMIL: OnceLock<Texts> = OnceLock::new();
static VIETNAMESE: OnceLock<Texts> = OnceLock::new();
static THAI: OnceLock<Texts> = OnceLock::new();
static INDONESIAN: OnceLock<Texts> = OnceLock::new();

fn load(slot: &'static OnceLock<Texts>, source: &'static str) -> &'static Texts {
    slot.get_or_init(|| toml::from_str(source).expect("embedded TUI resource must be valid TOML"))
}

impl Locale {
    pub fn from_arg_or_environment(value: Option<&str>) -> Self {
        let value = value
            .map(str::to_owned)
            .or_else(|| std::env::var("LC_ALL").ok())
            .or_else(|| std::env::var("LANG").ok())
            .unwrap_or_else(|| "en-US".to_string())
            .to_ascii_lowercase()
            .replace('_', "-");
        if value.starts_with("zh-tw") || value.starts_with("zh-hk") {
            Self::TraditionalChinese
        } else if value.starts_with("zh") {
            Self::SimplifiedChinese
        } else if value.starts_with("ja") {
            Self::Japanese
        } else if value.starts_with("es") {
            Self::Spanish
        } else if value.starts_with("ru") {
            Self::Russian
        } else if value.starts_with("hi") {
            Self::Hindi
        } else if value.starts_with("bn") {
            Self::Bengali
        } else if value.starts_with("ta") {
            Self::Tamil
        } else if value.starts_with("vi") {
            Self::Vietnamese
        } else if value.starts_with("th") {
            Self::Thai
        } else if value.starts_with("id") || value.starts_with("in") {
            Self::Indonesian
        } else {
            Self::English
        }
    }

    fn texts(self) -> &'static Texts {
        match self {
            Self::English => load(&ENGLISH, include_str!("english.toml")),
            Self::SimplifiedChinese => {
                load(&SIMPLIFIED_CHINESE, include_str!("simplified_chinese.toml"))
            }
            Self::TraditionalChinese => load(
                &TRADITIONAL_CHINESE,
                include_str!("traditional_chinese.toml"),
            ),
            Self::Japanese => load(&JAPANESE, include_str!("japanese.toml")),
            Self::Spanish => load(&SPANISH, include_str!("spanish.toml")),
            Self::Russian => load(&RUSSIAN, include_str!("russian.toml")),
            Self::Hindi => load(&HINDI, include_str!("hindi.toml")),
            Self::Bengali => load(&BENGALI, include_str!("bengali.toml")),
            Self::Tamil => load(&TAMIL, include_str!("tamil.toml")),
            Self::Vietnamese => load(&VIETNAMESE, include_str!("vietnamese.toml")),
            Self::Thai => load(&THAI, include_str!("thai.toml")),
            Self::Indonesian => load(&INDONESIAN, include_str!("indonesian.toml")),
        }
    }
    pub fn title(self) -> &'static str {
        &self.texts().title
    }
    pub fn empty(self) -> &'static str {
        &self.texts().empty
    }
    pub fn footer(self) -> &'static str {
        &self.texts().footer
    }
    pub fn add_prompt(self) -> &'static str {
        &self.texts().add_prompt
    }
    pub fn filter_prompt(self) -> &'static str {
        &self.texts().filter_prompt
    }
    pub fn filtered(self) -> &'static str {
        &self.texts().filtered
    }
    pub fn details(self) -> &'static str {
        &self.texts().details
    }
    pub fn headers(self) -> Vec<&'static str> {
        self.texts().headers.iter().map(String::as_str).collect()
    }
    pub fn remote_headers(self) -> Vec<&'static str> {
        self.texts()
            .remote_headers
            .iter()
            .map(String::as_str)
            .collect()
    }
    pub fn detail_labels(
        self,
    ) -> (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    ) {
        let x = &self.texts().detail_labels;
        (&x[0], &x[1], &x[2], &x[3], &x[4])
    }
    fn status_index(status: &str) -> usize {
        match status {
            "active" => 1,
            "paused" => 2,
            "complete" => 4,
            "error" => 3,
            "removed" => 5,
            _ => 0,
        }
    }
    pub fn status(self, status: &DownloadStatus) -> String {
        let i = match status {
            DownloadStatus::Waiting => 0,
            DownloadStatus::Active => 1,
            DownloadStatus::Paused => 2,
            DownloadStatus::Error(_) => 3,
            DownloadStatus::Complete => 4,
            DownloadStatus::Removed => 5,
        };
        self.texts().statuses[i].clone()
    }
    pub fn remote_status(self, status: &str) -> &'static str {
        &self.texts().statuses[Self::status_index(status)]
    }
    pub fn page(self, page: usize, has_next: bool) -> String {
        self.texts()
            .page
            .replace("{page}", &page.to_string())
            .replace("{next}", if has_next { "+" } else { "" })
    }
    pub fn error(self, message: &str) -> String {
        self.texts().error.replace("{message}", message)
    }
}

#[cfg(test)]
mod tests {
    use super::Locale;
    #[test]
    fn every_embedded_resource_is_complete() {
        let locales = [
            Locale::English,
            Locale::SimplifiedChinese,
            Locale::TraditionalChinese,
            Locale::Japanese,
            Locale::Spanish,
            Locale::Russian,
            Locale::Hindi,
            Locale::Bengali,
            Locale::Tamil,
            Locale::Vietnamese,
            Locale::Thai,
            Locale::Indonesian,
        ];
        for locale in locales {
            assert!(!locale.title().is_empty());
            assert!(!locale.empty().is_empty());
            assert_eq!(locale.headers().len(), 6);
            assert_eq!(locale.remote_headers().len(), 5);
            assert_eq!(locale.texts().detail_labels.len(), 5);
            assert_eq!(locale.texts().statuses.len(), 6);
            assert!(!locale.page(1, true).contains("{page}"));
            assert!(!locale.error("test").contains("{message}"));
        }
    }
}
