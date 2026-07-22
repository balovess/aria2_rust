use aria2_core::config::{
    ConfigManager, OptionCategory, OptionDef, OptionRegistry, OptionType, OptionValue,
};
use colored::Colorize;

fn main() {
    println!("{}", "=== Custom Configuration Example ===".cyan().bold());
    println!();

    println!("--- 1. Using built-in registry ---");
    let registry = OptionRegistry::new();
    println!("Registered options: {}", registry.count());

    let general = registry.by_category(OptionCategory::General);
    println!("General options: {} items", general.len());
    for def in general.iter().take(5) {
        println!(
            "  - {}: {} (default: {})",
            def.name(),
            def.opt_type(),
            def.default_value()
        );
    }

    println!("\n--- 2. Creating custom option registry ---");
    let mut custom_reg = OptionRegistry::new();
    custom_reg.register(OptionDef {
        name: "custom-cache-dir".into(),
        opt_type: OptionType::String,
        short_name: Some('C'),
        default_value: OptionValue::Str("/var/cache/aria2".into()),
        description: "Custom cache directory".into(),
        category: OptionCategory::Advanced,
        ..Default::default()
    });
    custom_reg.register(OptionDef {
        name: "max-retry-delay".into(),
        opt_type: OptionType::Integer,
        default_value: OptionValue::Int(300),
        description: "Maximum retry delay (seconds)".into(),
        min: Some(0),
        max: Some(3600),
        ..Default::default()
    });

    println!("Custom registry size: {}", custom_reg.count());
    assert!(custom_reg.contains("custom-cache-dir"));
    assert!(custom_reg.contains("max-retry-delay"));

    println!("\n--- 3. ConfigManager multi-source loading ---");
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

        println!("\n--- 4. Change event subscription ---");
        let mut subscriber = mgr.subscribe_changes();

        mgr.set_global_option("quiet", OptionValue::Bool(true))
            .await
            .unwrap();

        if let Ok(event) = subscriber.try_recv() {
            println!("Received change event: {} → {:?}", event.key, event.new_value);
        }

        println!("\n--- 5. JSON export ---");
        let json = mgr.get_all_global_options_json().await;
        println!(
            "Full config (JSON):\n{}",
            serde_json::to_string_pretty(&json).unwrap_or_default()
        );

        println!("\n{} Custom configuration example complete!", "✓".green().bold());
    });
}
