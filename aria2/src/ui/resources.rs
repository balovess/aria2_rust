//! Localized TUI resources.

use aria2_core::request::request_group::DownloadStatus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locale {
    English,
    SimplifiedChinese,
    Japanese,
    Spanish,
}

impl Locale {
    pub fn from_arg_or_environment(value: Option<&str>) -> Self {
        let value = value
            .map(str::to_owned)
            .or_else(|| std::env::var("LC_ALL").ok())
            .or_else(|| std::env::var("LANG").ok())
            .unwrap_or_else(|| "en-US".to_string())
            .to_ascii_lowercase();
        if value.starts_with("zh") {
            Self::SimplifiedChinese
        } else if value.starts_with("ja") {
            Self::Japanese
        } else if value.starts_with("es") {
            Self::Spanish
        } else {
            Self::English
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::English => "aria2c TUI",
            Self::SimplifiedChinese => "aria2c 终端界面",
            Self::Japanese => "aria2c ターミナル画面",
            Self::Spanish => "aria2c TUI",
        }
    }
    pub fn empty(self) -> &'static str {
        match self {
            Self::English => "No downloads.",
            Self::SimplifiedChinese => "暂无下载任务。",
            Self::Japanese => "ダウンロードはありません。",
            Self::Spanish => "No hay descargas.",
        }
    }
    pub fn footer(self) -> &'static str {
        match self {
            Self::English => {
                "↑/↓ Select  a Add  p Pause/Resume  r Remove  d Details  / Filter  q Quit"
            }
            Self::SimplifiedChinese => {
                "↑/↓ 选择  a 添加  p 暂停/继续  r 删除  d 详情  / 筛选  q 退出"
            }
            Self::Japanese => {
                "↑/↓ 選択  a 追加  p 一時停止/再開  r 削除  d 詳細  / 絞り込み  q 終了"
            }
            Self::Spanish => {
                "↑/↓ Seleccionar  a Añadir  p Pausar/Reanudar  r Eliminar  d Detalles  / Filtrar  q Salir"
            }
        }
    }
    pub fn add_prompt(self) -> &'static str {
        match self {
            Self::English => "URL (Enter to add, Esc to cancel)",
            Self::SimplifiedChinese => "URL（回车添加，Esc 取消）",
            Self::Japanese => "URL（Enter で追加、Esc でキャンセル）",
            Self::Spanish => "URL (Enter para añadir, Esc para cancelar)",
        }
    }
    pub fn filter_prompt(self) -> &'static str {
        match self {
            Self::English => "Filter (Enter to apply, Esc to cancel)",
            Self::SimplifiedChinese => "筛选（回车应用，Esc 取消）",
            Self::Japanese => "絞り込み（Enter で適用、Esc でキャンセル）",
            Self::Spanish => "Filtro (Enter para aplicar, Esc para cancelar)",
        }
    }
    pub fn filtered(self) -> &'static str {
        match self {
            Self::English => "Filter",
            Self::SimplifiedChinese => "筛选",
            Self::Japanese => "絞り込み",
            Self::Spanish => "Filtro",
        }
    }
    pub fn details(self) -> &'static str {
        match self {
            Self::English => "Task details",
            Self::SimplifiedChinese => "任务详情",
            Self::Japanese => "タスク詳細",
            Self::Spanish => "Detalles de la tarea",
        }
    }

    pub fn headers(self) -> [&'static str; 6] {
        match self {
            Self::English => ["GID", "Status", "Progress", "Speed", "Connections", "Input"],
            Self::SimplifiedChinese => ["GID", "状态", "进度", "速度", "连接数", "输入"],
            Self::Japanese => ["GID", "状態", "進捗", "速度", "接続数", "入力"],
            Self::Spanish => [
                "GID",
                "Estado",
                "Progreso",
                "Velocidad",
                "Conexiones",
                "Entrada",
            ],
        }
    }

    pub fn remote_headers(self) -> [&'static str; 5] {
        match self {
            Self::English => ["GID", "Status", "Progress", "Speed", "Input"],
            Self::SimplifiedChinese => ["GID", "状态", "进度", "速度", "输入"],
            Self::Japanese => ["GID", "状態", "進捗", "速度", "入力"],
            Self::Spanish => ["GID", "Estado", "Progreso", "Velocidad", "Entrada"],
        }
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
        match self {
            Self::English => ("GID", "Status", "Completed", "Download speed", "Input"),
            Self::SimplifiedChinese => ("GID", "状态", "已完成", "下载速度", "输入"),
            Self::Japanese => ("GID", "状態", "完了", "ダウンロード速度", "入力"),
            Self::Spanish => (
                "GID",
                "Estado",
                "Completado",
                "Velocidad de descarga",
                "Entrada",
            ),
        }
    }
    pub fn status(self, status: &DownloadStatus) -> String {
        match self {
            Self::English => status.to_string(),
            Self::SimplifiedChinese => match status {
                DownloadStatus::Waiting => "等待".into(),
                DownloadStatus::Active => "下载中".into(),
                DownloadStatus::Paused => "已暂停".into(),
                DownloadStatus::Error(_) => "错误".into(),
                DownloadStatus::Complete => "完成".into(),
                DownloadStatus::Removed => "已删除".into(),
            },
            Self::Japanese => status.to_string(),
            Self::Spanish => status.to_string(),
        }
    }

    pub fn remote_status(self, status: &str) -> &'static str {
        match self {
            Self::English => match status {
                "active" => "active",
                "waiting" => "waiting",
                "paused" => "paused",
                "complete" => "complete",
                "error" => "error",
                "removed" => "removed",
                _ => "unknown",
            },
            Self::SimplifiedChinese => match status {
                "active" => "下载中",
                "waiting" => "等待",
                "paused" => "已暂停",
                "complete" => "完成",
                "error" => "错误",
                "removed" => "已删除",
                _ => "未知",
            },
            Self::Japanese => match status {
                "active" => "ダウンロード中",
                "waiting" => "待機中",
                "paused" => "一時停止",
                "complete" => "完了",
                "error" => "エラー",
                "removed" => "削除済み",
                _ => "不明",
            },
            Self::Spanish => match status {
                "active" => "activo",
                "waiting" => "en espera",
                "paused" => "pausado",
                "complete" => "completado",
                "error" => "error",
                "removed" => "eliminado",
                _ => "desconocido",
            },
        }
    }

    pub fn page(self, page: usize, has_next: bool) -> String {
        match self {
            Self::English => format!(
                "Page {page}{}  [/] Previous/Next  PgUp/PgDn Jump",
                if has_next { "+" } else { "" }
            ),
            Self::SimplifiedChinese => format!(
                "第 {page} 页{}  [/] 上一页/下一页  PgUp/PgDn 快速翻页",
                if has_next { "+" } else { "" }
            ),
            Self::Japanese => format!(
                "{page} ページ{}  [/] 前/次  PgUp/PgDn ページ移動",
                if has_next { "+" } else { "" }
            ),
            Self::Spanish => format!(
                "Página {page}{}  [/] Anterior/Siguiente  PgUp/PgDn Saltar",
                if has_next { "+" } else { "" }
            ),
        }
    }

    pub fn error(self, message: &str) -> String {
        match self {
            Self::English => format!("RPC error: {message} (retrying)"),
            Self::SimplifiedChinese => format!("RPC 错误：{message}（正在重试）"),
            Self::Japanese => format!("RPC エラー: {message}（再試行中）"),
            Self::Spanish => format!("Error RPC: {message} (reintentando)"),
        }
    }
}
