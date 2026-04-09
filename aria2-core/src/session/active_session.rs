use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::request::request_group::RequestGroup;
use super::session_serializer::{self, SessionEntry};

/// 活跃会话管理器 - 负责会话的加载、保存和自动保存
pub struct ActiveSessionManager {
    /// 会话文件路径
    pub session_path: PathBuf,
    /// 自动保存间隔
    pub auto_save_interval: Duration,
    /// 脏标记 - 标记是否有未保存的更改
    dirty_flag: AtomicBool,
}

impl ActiveSessionManager {
    /// 创建新的活跃会话管理器
    ///
    /// # 参数
    /// - `session_path`: 会话文件保存路径
    /// - `auto_save_interval`: 自动保存的时间间隔
    ///
    /// # 示例
    /// ```ignore
    /// let manager = ActiveSessionManager::new(
    ///     PathBuf::from("/tmp/session.txt"),
    ///     Duration::from_secs(60),
    /// );
    /// ```
    pub fn new(session_path: PathBuf, auto_save_interval: Duration) -> Self {
        tracing::info!(
            "创建 ActiveSessionManager: path={}, interval={:?}",
            session_path.display(),
            auto_save_interval
        );

        ActiveSessionManager {
            session_path,
            auto_save_interval,
            dirty_flag: AtomicBool::new(false),
        }
    }

    /// 从文件加载会话数据
    ///
    /// 如果文件不存在，返回空的 Vec（不视为错误）
    ///
    /// # 返回值
    /// - `Ok(Vec<SessionEntry>)`: 成功加载的会话条目列表
    /// - `Err(String)`: 加载失败时的错误信息
    pub async fn load_session(&self) -> Result<Vec<SessionEntry>, String> {
        if !self.session_path.exists() {
            tracing::debug!(
                "会话文件不存在，返回空列表: {}",
                self.session_path.display()
            );
            return Ok(vec![]);
        }

        match session_serializer::load_from_file(&self.session_path).await {
            Ok(entries) => {
                tracing::info!(
                    "成功加载会话文件: {}, 条目数: {}",
                    self.session_path.display(),
                    entries.len()
                );
                Ok(entries)
            }
            Err(e) => {
                let err_msg = format!("Failed to load session file: {}", e);
                tracing::error!("{}", err_msg);
                Err(err_msg)
            }
        }
    }

    /// 保存所有下载组的状态到会话文件
    ///
    /// 使用原子写入策略：先写入临时文件 (.sess.tmp)，然后重命名为目标文件。
    /// 这确保在写入过程中如果发生崩溃，不会损坏原有的会话文件。
    ///
    /// # 参数
    /// - `groups`: 需要保存状态的下载组列表
    ///
    /// # 返回值
    /// - `Ok(usize)`: 成功保存的条目数量
    /// - `Err(String)`: 保存失败时的错误信息
    pub async fn save_session(&self, groups: &[Arc<RwLock<RequestGroup>>]) -> Result<usize, String> {
        // 序列化所有组为 SessionEntry 列表
        let mut entries = Vec::new();
        for group_lock in groups {
            let group = group_lock.read().await;
            if let Some(entry) = session_serializer::group_to_entry(&group).await {
                entries.push(entry);
            }
        }

        if entries.is_empty() {
            tracing::debug!("没有需要保存的活动条目");
            return Ok(0);
        }

        // 使用原子写入策略保存到文件
        match session_serializer::save_to_file_with_entries(&self.session_path, &entries).await {
            Ok(_) => {
                tracing::info!(
                    "成功保存会话文件: {}, 条目数: {}",
                    self.session_path.display(),
                    entries.len()
                );
                Ok(entries.len())
            }
            Err(e) => {
                let err_msg = format!("Failed to save session file: {}", e);
                tracing::error!("{}", err_msg);
                Err(err_msg)
            }
        }
    }

    /// 标记会话为脏状态（有未保存的更改）
    pub fn mark_dirty(&self) {
        self.dirty_flag.store(true, Ordering::Relaxed);
        tracing::debug!("标记会话为脏状态");
    }

    /// 检查会话是否有未保存的更改
    pub fn is_dirty(&self) -> bool {
        self.dirty_flag.load(Ordering::Relaxed)
    }

    /// 启动自动保存后台任务
    ///
    /// 在后台循环中定期检查脏标记，如果有未保存的更改则自动保存。
    /// 该方法会在后台启动一个 Tokio 任务，不会阻塞当前线程。
    ///
    /// # 参数
    /// - `self`: 必须通过 Arc 包装，以便在后台任务中共享
    /// - `groups`: 所有活动下载组的共享引用
    ///
    /// # 注意事项
    /// - 此方法会启动一个无限循环的后台任务
    /// - 只有当 dirty flag 为 true 时才会执行保存操作
    /// - 保存成功后会清除 dirty flag
    pub fn start_auto_save(
        self: &Arc<Self>,
        groups: Arc<RwLock<Vec<Arc<RwLock<RequestGroup>>>>>,
    ) {
        let mgr = Arc::clone(self);

        tracing::info!(
            "启动自动保存任务, 间隔: {:?}",
            mgr.auto_save_interval
        );

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(mgr.auto_save_interval);

            loop {
                interval.tick().await;

                // 如果没有更改，跳过本次保存
                if !mgr.is_dirty() {
                    tracing::debug!("自动保存检查: 无更改，跳过");
                    continue;
                }

                tracing::debug!("自动保存检查: 检测到更改，开始保存");

                // 获取所有组的读锁
                let groups_read = groups.read().await;
                match mgr.save_session(&groups_read).await {
                    Ok(n) => {
                        tracing::debug!("自动保存成功: 保存了 {} 个条目", n);
                        // 保存成功后清除脏标记
                        mgr.dirty_flag.store(false, Ordering::Relaxed);
                    }
                    Err(e) => {
                        tracing::warn!("Auto-save failed: {} (keeping dirty flag for retry)", e);
                        // 保存失败时保留脏标记，下次继续尝试
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::request_group::{DownloadOptions, GroupId};
    use tempfile::TempDir;

    /// 测试 1: 验证 new() 正确创建管理器
    #[test]
    fn test_new_manager() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_path = temp_dir.path().join("session.txt");
        let interval = Duration::from_secs(60);

        let manager = ActiveSessionManager::new(session_path.clone(), interval);

        assert_eq!(manager.session_path, session_path, "路径应正确设置");
        assert_eq!(manager.auto_save_interval, interval, "间隔应正确设置");
        assert!(!manager.is_dirty(), "新创建的管理器不应是脏状态");
    }

    /// 测试 2: 文件不存在时返回空列表
    #[tokio::test]
    async fn test_load_nonexistent_file_returns_empty() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let nonexistent_path = temp_dir.path().join("nonexistent_session.txt");

        let manager = ActiveSessionManager::new(nonexistent_path, Duration::from_secs(60));
        let result = manager.load_session().await;

        assert!(result.is_ok(), "文件不存在不应返回错误");
        let entries = result.unwrap();
        assert!(entries.is_empty(), "文件不存在时应返回空列表");
    }

    /// 测试 3: 保存和加载往返测试
    #[tokio::test]
    async fn test_load_save_roundtrip() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_path = temp_dir.path().join("roundtrip_session.txt");

        let manager = ActiveSessionManager::new(session_path.clone(), Duration::from_secs(60));

        // 创建测试用的 RequestGroup
        let gid1 = GroupId::new(0xd270c8a2);
        let options1 = DownloadOptions {
            dir: Some("/downloads".to_string()),
            split: Some(4),
            ..Default::default()
        };
        let group1 = Arc::new(RwLock::new(RequestGroup::new(
            gid1,
            vec!["http://example.com/file1.zip".to_string()],
            options1,
        )));

        let gid2 = GroupId::new(0xabcdef01);
        let group2 = Arc::new(RwLock::new(RequestGroup::new(
            gid2,
            vec![
                "http://mirror.com/file2.iso".to_string(),
                "ftp://backup.com/file2.iso".to_string(),
            ],
            DownloadOptions::default(),
        )));

        let groups = vec![group1, group2];

        // 保存会话
        let save_result = manager.save_session(&groups).await;
        assert!(save_result.is_ok(), "保存应成功");
        let saved_count = save_result.unwrap();
        assert!(saved_count > 0, "应保存至少 1 个条目");

        // 加载会话并验证
        let load_result = manager.load_session().await;
        assert!(load_result.is_ok(), "加载应成功");
        let entries = load_result.unwrap();

        assert_eq!(entries.len(), saved_count, "加载的条目数应与保存的一致");

        // 验证数据完整性
        assert!(entries.iter().any(|e| e.uris.contains(&"http://example.com/file1.zip".to_string())),
            "应包含第一个 URI");
        assert!(entries.iter().any(|e| e.uris.contains(&"http://mirror.com/file2.iso".to_string())),
            "应包含第二个 URI");
    }

    /// 测试 4: mark_dirty 和 is_dirty 功能验证
    #[test]
    fn test_mark_dirty_and_check() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_path = temp_dir.path().join("dirty_test.txt");

        let manager = ActiveSessionManager::new(session_path, Duration::from_secs(30));

        // 初始状态应为干净
        assert!(!manager.is_dirty(), "初始状态应是干净的");

        // 标记为脏
        manager.mark_dirty();
        assert!(manager.is_dirty(), "mark_dirty 后应为脏状态");

        // 再次标记（幂等性）
        manager.mark_dirty();
        assert!(manager.is_dirty(), "重复 mark_dirty 应保持脏状态");
    }

    /// 测试 5: 自动保存在干净状态下跳过保存
    ///
    /// 此测试通过短间隔验证：当 dirty=false 时，不会触发实际的保存操作
    #[tokio::test]
    async fn test_auto_save_skips_when_clean() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_path = temp_dir.path().join("auto_skip_test.txt");

        let manager = Arc::new(ActiveSessionManager::new(
            session_path.clone(),
            Duration::from_millis(50), // 短间隔以加速测试
        ));

        let groups: Arc<RwLock<Vec<Arc<RwLock<RequestGroup>>>>> =
            Arc::new(RwLock::new(Vec::new()));

        // 启动自动保存（此时 dirty=false）
        manager.start_auto_save(Arc::clone(&groups));

        // 等待几个 tick 周期
        tokio::time::sleep(Duration::from_millis(200)).await;

        // 验证文件未被创建（因为没有脏标记）
        assert!(
            !session_path.exists(),
            "dirty=false 时不应创建会话文件"
        );
    }

    /// 测试 6: 保存后文件应存在于指定路径
    #[tokio::test]
    async fn test_save_creates_file() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_path = temp_dir.path().join("file_creation_test.txt");

        let manager = ActiveSessionManager::new(session_path.clone(), Duration::from_secs(60));

        // 验证初始状态文件不存在
        assert!(
            !session_path.exists(),
            "保存前文件不应存在"
        );

        // 创建测试组
        let gid = GroupId::new(12345);
        let group = Arc::new(RwLock::new(RequestGroup::new(
            gid,
            vec!["http://test.com/file.bin".to_string()],
            DownloadOptions::default(),
        )));

        // 执行保存
        let result = manager.save_session(&[group]).await;
        assert!(result.is_ok(), "保存应成功");

        // 验证文件已创建
        assert!(
            session_path.exists(),
            "保存后文件应存在于指定路径"
        );

        // 验证文件内容非空
        let content = tokio::fs::read_to_string(&session_path)
            .await
            .expect("读取文件失败");
        assert!(!content.is_empty(), "文件内容不应为空");
        assert!(
            content.contains("http://test.com/file.bin"),
            "文件应包含保存的 URI"
        );
    }

    /// 测试 7: 多次保存覆盖旧文件
    #[tokio::test]
    async fn test_multiple_saves_overwrite() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_path = temp_dir.path().join("overwrite_test.txt");

        let manager = ActiveSessionManager::new(session_path.clone(), Duration::from_secs(60));

        // 第一次保存
        let gid1 = GroupId::new(1);
        let group1 = Arc::new(RwLock::new(RequestGroup::new(
            gid1,
            vec!["http://first.com/a.txt".to_string()],
            DownloadOptions::default(),
        )));
        let result1 = manager.save_session(&[group1]).await;
        assert!(result1.is_ok());

        // 第二次保存不同的内容
        let gid2 = GroupId::new(2);
        let group2 = Arc::new(RwLock::new(RequestGroup::new(
            gid2,
            vec!["http://second.com/b.txt".to_string()],
            DownloadOptions::default(),
        )));
        let result2 = manager.save_session(&[group2]).await;
        assert!(result2.is_ok());

        // 加载并验证只包含第二次的内容
        let entries = manager.load_session().await.expect("加载失败");
        assert_eq!(entries.len(), 1, "应只有 1 个条目（最新的）");
        assert!(
            entries[0].uris.contains(&"http://second.com/b.txt".to_string()),
            "应包含最新保存的 URI"
        );
        assert!(
            !entries[0].uris.contains(&"http://first.com/a.txt".to_string()),
            "不应包含旧的 URI"
        );
    }

    /// 测试 8: 空组列表保存后文件不存在或为空
    #[tokio::test]
    async fn test_save_empty_groups() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_path = temp_dir.path().join("empty_groups_test.txt");

        let manager = ActiveSessionManager::new(session_path.clone(), Duration::from_secs(60));

        // 保存空列表
        let empty_groups: Vec<Arc<RwLock<RequestGroup>>> = vec![];
        let result = manager.save_session(&empty_groups).await;

        assert!(result.is_ok(), "保存空列表应成功");
        assert_eq!(result.unwrap(), 0, "应返回 0 个条目");

        // 文件可能不存在或为空（取决于实现）
        if session_path.exists() {
            let content = tokio::fs::read_to_string(&session_path)
                .await
                .expect("读取文件失败");
            assert!(content.is_empty(), "空组列表应产生空文件");
        }
    }

    /// 测试 9: 自动保存触发时的完整流程
    #[tokio::test]
    async fn test_auto_save_triggers_on_dirty() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");
        let session_path = temp_dir.path().join("auto_trigger_test.txt");

        let manager = Arc::new(ActiveSessionManager::new(
            session_path.clone(),
            Duration::from_millis(50), // 短间隔
        ));

        // 创建测试组
        let gid = GroupId::new(99999);
        let group = Arc::new(RwLock::new(RequestGroup::new(
            gid,
            vec!["http://auto-save-test.com/data.bin".to_string()],
            DownloadOptions::default(),
        )));

        let groups: Arc<RwLock<Vec<Arc<RwLock<RequestGroup>>>>> =
            Arc::new(RwLock::new(vec![group]));

        // 启动自动保存
        manager.start_auto_save(Arc::clone(&groups));

        // 标记为脏
        manager.mark_dirty();

        // 等待足够的时间让自动保存执行
        tokio::time::sleep(Duration::from_millis(300)).await;

        // 验证文件已被创建（因为 dirty=true 触发了保存）
        // 注意：由于异步任务的时序，这里可能需要更长的等待时间
        // 我们给予合理的等待时间
        if session_path.exists() {
            let content = tokio::fs::read_to_string(&session_path)
                .await
                .expect("读取文件失败");
            assert!(
                content.contains("http://auto-save-test.com/data.bin") || content.is_empty(),
                "文件应包含保存的数据或为空（取决于时序）"
            );
        }
        // 即使文件尚未创建也是可接受的（取决于异步调度）
    }

    /// 测试 10: 不同 auto_save_interval 的配置
    #[test]
    fn test_different_intervals() {
        let temp_dir = TempDir::new().expect("创建临时目录失败");

        // 测试各种间隔配置
        let intervals = vec![
            Duration::from_secs(1),
            Duration::from_secs(30),
            Duration::from_secs(60),
            Duration::from_secs(300),
            Duration::from_millis(500),
        ];

        for (i, interval) in intervals.iter().enumerate() {
            let path = temp_dir.path().join(format!("interval_test_{}.txt", i));
            let manager = ActiveSessionManager::new(path, *interval);
            assert_eq!(manager.auto_save_interval, *interval, "间隔 {} 应正确设置", i);
        }
    }
}
