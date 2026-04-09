use aria2_core::config::{
    ConfigManager, OptionCategory, OptionDef, OptionRegistry, OptionType, OptionValue,
};
use colored::Colorize;

fn main() {
    println!("{}", "=== 自定义配置示例 ===".cyan().bold());
    println!();

    println!("--- 1. 使用内置注册表 ---");
    let registry = OptionRegistry::new();
    println!("注册选项数: {}", registry.count());

    let general = registry.by_category(OptionCategory::General);
    println!("通用选项: {} 个", general.len());
    for def in general.iter().take(5) {
        println!(
            "  - {}: {} (默认: {})",
            def.name(),
            def.opt_type(),
            def.default_value()
        );
    }

    println!("\n--- 2. 创建自定义选项注册表 ---");
    let mut custom_reg = OptionRegistry::new();
    custom_reg.register(
        OptionDef::new("custom-cache-dir", OptionType::String)
            .short('C')
            .default(OptionValue::Str("/var/cache/aria2".into()))
            .desc("自定义缓存目录")
            .category(OptionCategory::Advanced),
    );
    custom_reg.register(
        OptionDef::new("max-retry-delay", OptionType::Integer)
            .default(OptionValue::Int(300))
            .desc("最大重试延迟(秒)")
            .range(0, 3600),
    );

    println!("自定义注册表大小: {}", custom_reg.count());
    assert!(custom_reg.contains("custom-cache-dir"));
    assert!(custom_reg.contains("max-retry-delay"));

    println!("\n--- 3. ConfigManager 多源加载 ---");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut mgr = ConfigManager::new_with_registry(custom_reg);

        mgr.set_global_option("dir", OptionValue::Str("/opt/downloads".into()))
            .await
            .unwrap();
        mgr.set_global_option("split", OptionValue::Int(16))
            .await
            .unwrap();
        mgr.set_global_option("custom-cache-dir", OptionValue::Str("/tmp/cache".into()))
            .await
            .unwrap();

        let dir = mgr.get_global_str("dir").await;
        let split = mgr.get_global_i64("split").await;
        let cache_dir = mgr.get_global_str("custom-cache-dir").await;

        println!("dir       = {:?}", dir);
        println!("split     = {:?}", split);
        println!("cache-dir = {:?}", cache_dir);

        println!("\n--- 4. 变更事件订阅 ---");
        let mut subscriber = mgr.subscribe_changes();

        mgr.set_global_option("quiet", OptionValue::Bool(true))
            .await
            .unwrap();

        if let Ok(event) = subscriber.try_recv() {
            println!("收到变更事件: {} → {:?}", event.key, event.new_value);
        }

        println!("\n--- 5. JSON 导出 ---");
        let json = mgr.get_all_global_options_json().await;
        println!(
            "完整配置(JSON):\n{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );

        println!("\n{} 自定义配置示例完成!", "✓".green().bold());
    });
}
