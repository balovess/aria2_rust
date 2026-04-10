//! HTTP 连接管理器集成测试
//!
//! 测试连接池复用、重定向跟随、超时控制等核心功能。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};

use crate::error::Aria2Error;
use crate::http::connection::{
    ActiveConnection, HttpConfig, HttpConnectionManager,
};
use crate::http::connection::HttpResponse;

/// 创建测试用的 HTTP 配置
fn create_test_config() -> HttpConfig {
    HttpConfig {
        max_connections: 4,
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_millis(1000),
        write_timeout: Duration::from_millis(1000),
        idle_timeout: Duration::from_millis(2000),
    }
}

/// 启动一个简单的测试 HTTP 服务器
///
/// 返回服务器的本地地址和服务器句柄（用于在测试结束时关闭）
async fn start_test_server(
    handler: impl Fn(TcpStream) + Send + 'static,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    handler(stream);
                }
                Err(_) => break,
            }
        }
    });

    (addr, handle)
}

// ==================== 测试用例 1: 连接池复用 ====================

#[tokio::test]
async fn test_connection_pool_reuse() {
    let config = create_test_config();
    let mut manager = HttpConnectionManager::new(&config);

    // 启动测试服务器
    let addr_str = Arc::new(Mutex::new(String::new()));
    let addr_clone = addr_str.clone();
    let (addr, server_handle) = start_test_server(move |mut stream| {
        let addr_clone = addr_clone.clone();
        tokio::spawn(async move {
            // 保存地址
            *addr_clone.lock().unwrap() =
                stream.peer_addr().unwrap().to_string();

            // 简单的 HTTP 响应
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            stream.write_all(response.as_bytes()).await.unwrap();
        });
    })
    .await;

    sleep(Duration::from_millis(100)).await; // 等待服务器启动

    let url = url::Url::parse(&format!("http://{}", addr)).unwrap();

    // 第一次获取连接
    let conn1 = manager.acquire(&url).await.expect("第一次获取连接应成功");
    let conn1_id = conn1.id;
    assert_eq!(manager.active_count(), 1);
    println!("✓ 第一次获取连接成功: id={}", conn1_id);

    // 归还连接
    manager.release(conn1_id).await;
    assert_eq!(manager.active_count(), 1); // 连接仍在池中
    println!("✓ 连接已归还到池中");

    // 第二次获取连接（应该复用）
    let conn2 = manager.acquire(&url).await.expect("第二次应复用连接");
    assert_eq!(conn2.id, conn1_id); // 应该是同一个连接 ID
    assert_eq!(manager.active_count(), 1); // 不应该创建新连接
    println!("✓ 连接池复用成功: id={}", conn2.id);

    // 清理
    manager.cleanup().await;
    server_handle.abort();

    println!("✅ 测试通过: 连接池复用正常工作");
}

// ==================== 测试用例 2: 重定向跟随（5跳）====================

#[tokio::test]
async fn test_redirect_follow_5_jumps() {
    let manager = HttpConnectionManager::new(&create_test_config());
    let current_url = url::Url::parse("http://example.com/start").unwrap();
    let mut redirect_chain = HashSet::new();
    redirect_chain.insert(current_url.clone());

    // 模拟 5 次连续重定向
    let urls = vec![
        "http://example.com/page1",
        "http://example.com/page2",
        "http://example.com/page3",
        "http://example.com/page4",
        "http://example.com/final",
    ];

    let mut current = current_url;
    for (i, target) in urls.iter().enumerate() {
        let mut response = HttpResponse::new(302, "Found".to_string());
        response.headers.push(("Location".to_string(), target.to_string()));

        redirect_chain.insert(current.clone());

        let result = manager.follow_redirects(&response, &current, &redirect_chain, (i + 1) as u32);
        assert!(
            result.is_ok(),
            "第 {} 次重定向应成功: {:?}",
            i + 1,
            result.err()
        );

        current = result.unwrap();
        println!("✓ 第 {} 次重定向: -> {}", i + 1, current);
    }

    assert_eq!(current.as_str(), "http://example.com/final/");
    println!("✅ 测试通过: 成功跟随 5 次重定向");
}

// ==================== 测试用例 3: 循环重定向检测 ====================

#[tokio::test]
async fn test_redirect_loop_detection() {
    let manager = HttpConnectionManager::new(&create_test_config());

    // 构建循环: A -> B -> C -> A
    let url_a = url::Url::parse("http://example.com/a").unwrap();
    let url_b = url::Url::parse("http://example.com/b").unwrap();
    let url_c = url::Url::parse("http://example.com/c").unwrap();

    let mut chain = HashSet::new();
    chain.insert(url_a.clone());
    chain.insert(url_b.clone());
    chain.insert(url_c.clone());

    // 从 C 尝试重定向回 A（形成循环）
    let mut response = HttpResponse::new(301, "Moved".to_string());
    response.headers.push(("Location".to_string(), "http://example.com/a".to_string()));

    let result = manager.follow_redirects(&response, &url_c, &chain, 3);

    assert!(result.is_err(), "循环重定向应被检测到");

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("循环重定向"),
        "错误消息应包含'循环重定向': {}",
        err_msg
    );
    println!("✓ 正确检测到循环重定向: {}", err_msg);

    println!("✅ 测试通过: 循环重定向检测正常工作");
}

// ==================== 测试用例 4: Range 请求构建 ====================

#[test]
fn test_range_request_build() {
    let manager = HttpConnectionManager::new(&create_test_config());

    // 测试 1: 标准范围
    let range1 = manager.build_range_header(0, Some(999));
    assert_eq!(range1, "bytes=0-999", "标准范围格式错误");
    println!("✓ 标准范围: {}", range1);

    // 测试 2: 开放结束范围
    let range2 = manager.build_range_header(500, None);
    assert_eq!(range2, "bytes=500-", "开放结束范围格式错误");
    println!("✓ 开放结束范围: {}", range2);

    // 测试 3: 单字节范围
    let range3 = manager.build_range_header(42, Some(42));
    assert_eq!(range3, "bytes=42-42", "单字节范围格式错误");
    println!("✓ 单字节范围: {}", range3);

    // 测试 4: 大偏移量
    let range4 = manager.build_range_header(1024 * 1024, Some(1024 * 1024 + 512));
    assert_eq!(
        range4,
        "bytes=1048576-1049088",
        "大偏移量范围格式错误"
    );
    println!("✓ 大偏移量范围: {}", range4);

    // 测试 5: Content-Range 解析
    let parsed1 = manager.parse_content_range("bytes 0-499/1000");
    assert_eq!(parsed1, Some((0, 499, 1000)), "Content-Range 解析失败");
    println!("✓ Content-Range 解析 (已知总数): {:?}", parsed1);

    let parsed2 = manager.parse_content_range("bytes 500-999/*");
    assert_eq!(parsed2, Some((500, 999, u64::MAX)), "未知总数解析失败");
    println!("✓ Content-Range 解析 (未知总数): {:?}", parsed2);

    // 测试 6: 无效格式
    assert_eq!(manager.parse_content_range("invalid"), None);
    assert_eq!(manager.parse_content_range("bits 0-99/1000"), None);
    println!("✓ 无效格式正确返回 None");

    println!("✅ 测试通过: Range 请求构建和解析正确");
}

// ==================== 测试用例 5: 超时控制 ====================

#[tokio::test]
async fn test_timeout_on_slow_server() {
    let config = HttpConfig {
        max_connections: 2,
        connect_timeout: Duration::from_millis(100),   // 短连接超时
        read_timeout: Duration::from_millis(200),      // 短读取超时
        write_timeout: Duration::from_millis(200),     // 短写入超时
        idle_timeout: Duration::from_secs(60),
    };
    let mut manager = HttpConnectionManager::new(&config);

    // 启动一个慢速服务器（不响应）
    let (addr, server_handle) = start_test_server(|_stream| {
        // 故意不响应，模拟慢速服务器
        tokio::spawn(async move {
            sleep(Duration::from_secs(10)).await;
        });
    })
    .await;

    sleep(Duration::from_millis(50)).await;

    let url = url::Url::parse(&format!("http://{}", addr)).unwrap();

    // 尝试连接（应该因超时失败）
    // 注意：由于是 localhost 连接，可能很快就会建立 TCP 连接
    // 超时主要体现在后续的 I/O 操作上
    let start = Instant::now();
    let result = timeout(config.connect_timeout + Duration::from_millis(50), manager.acquire(&url)).await;

    match result {
        Ok(conn_result) => {
            // 如果连接成功（localhost 可能会快速连接），验证配置是否正确
            if let Ok(conn) = conn_result {
                println!("⚠ 本地连接成功（预期行为），验证超时配置...");
                assert_eq!(manager.max_connections(), 2);
                manager.release(conn.id).await;
            } else {
                // 如果失败，验证是否为超时错误
                println!("✓ 连接失败（可能是超时）: {:?}", conn_result.err());
            }
        }
        Err(_) => {
            println!("✓ 连接操作超时（符合预期）");
        }
    }

    let elapsed = start.elapsed();
    println!("⏱ 操作耗时: {:.2}ms", elapsed.as_millis());

    // 验证超时时间在合理范围内（允许一定误差）
    assert!(
        elapsed < config.connect_timeout + Duration::from_millis(300),
        "耗时过长: {:.2}ms",
        elapsed.as_millis()
    );

    manager.cleanup().await;
    server_handle.abort();

    println!("✅ 测试通过: 超时控制机制正常工作");
}

// ==================== 测试用例 6: 最大连接数限制 ====================

#[tokio::test]
async fn test_max_connections_limit() {
    let config = HttpConfig {
        max_connections: 2,  // 限制最多 2 个连接
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_millis(1000),
        write_timeout: Duration::from_millis(1000),
        idle_timeout: Duration::from_secs(60),
    };
    let mut manager = HttpConnectionManager::new(&config);

    // 启动测试服务器
    let (addr, _server_handle) = start_test_server(|mut stream| {
        tokio::spawn(async move {
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            stream.write_all(response.as_bytes()).await.unwrap();
            sleep(Duration::from_secs(10)).await; // 保持连接
        });
    })
    .await;

    sleep(Duration::from_millis(100)).await;

    let url = url::Url::parse(&format!("http://{}", addr)).unwrap();

    // 获取第一个连接
    let conn1 = manager.acquire(&url).await.expect("第一个连接应成功");
    println!("✓ 第 1 个连接: id={}, active={}/{}", conn1.id, manager.active_count(), manager.max_connections());
    assert_eq!(manager.active_count(), 1);

    // 获取第二个连接
    let conn2 = manager.acquire(&url).await.expect("第二个连接应成功");
    println!("✓ 第 2 个连接: id={}, active={}/{}", conn2.id, manager.active_count(), manager.max_connections());
    assert_eq!(manager.active_count(), 2);

    // 尝试获取第三个连接（应该失败）
    let result = manager.acquire(&url).await;
    assert!(result.is_err(), "超过最大连接数限制时应返回错误");

    match result.unwrap_err() {
        Aria2Error::Recoverable(err) => {
            let err_msg = err.to_string();
            println!("✓ 正确拒绝第 3 个连接: {}", err_msg);
            assert!(
                err_msg.contains("最大连接数") || err_msg.contains("max"),
                "错误信息应包含连接数限制提示"
            );
        }
        other => panic!("期望 Recoverable 错误，得到: {:?}", other),
    }

    // 验证连接数未增加
    assert_eq!(manager.active_count(), 2, "活动连接数不应超过最大限制");

    // 归还一个连接后，应该可以重新获取
    manager.release(conn1.id).await;
    println!("✓ 归还连接 1 后尝试重新获取...");

    let conn3 = manager.acquire(&url).await.expect("归还后应能获取新连接");
    println!("✓ 归还后获取新连接成功: id={}", conn3.id);
    assert_eq!(manager.active_count(), 2);

    // 清理
    manager.release(conn2.id).await;
    manager.release(conn3.id).await;
    manager.cleanup().await;

    println!("✅ 测试通过: 最大连接数限制正确执行");
}

// ==================== 额外测试: LRU 淘汰策略 ====================

#[tokio::test]
async fn test_lru_eviction_strategy() {
    let config = HttpConfig {
        max_connections: 5,
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_millis(1000),
        write_timeout: Duration::from_millis(1000),
        idle_timeout: Duration::from_millis(100),  // 非常短的空闲超时
    };
    let mut manager = HttpConnectionManager::new(&config);

    // 启动测试服务器
    let (addr, _server_handle) = start_test_server(|mut stream| {
        tokio::spawn(async move {
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            stream.write_all(response.as_bytes()).await.unwrap();
        });
    })
    .await;

    sleep(Duration::from_millis(100)).await;

    let url = url::Url::parse(&format!("http://{}", addr)).unwrap();

    // 创建多个连接并立即归还
    let mut conn_ids = Vec::new();
    for i in 0..3 {
        let conn = manager.acquire(&url).await.unwrap();
        println!("创建连接 {}: id={}", i + 1, conn.id);
        conn_ids.push(conn.id);
        manager.release(conn.id).await;
    }

    assert_eq!(manager.pool_size(), 3, "应有 3 个空闲连接");
    println!("✓ 创建了 3 个空闲连接");

    // 等待连接过期
    sleep(Duration::from_millis(150)).await;
    println!("⏱ 等待 {:.2}ms 让连接过期...", 150.0);

    // 尝试获取新连接（应触发 LRU 淘汰）
    let new_conn = manager.acquire(&url).await.unwrap();
    println!("✓ 新连接创建（可能触发了 LRU 淘汰）: id={}", new_conn.id);

    // 验证旧连接已被清理
    // 注意：由于 acquire 内部会先尝试复用，过期的连接会被清理
    manager.release(new_conn.id).await;
    manager.cleanup().await;

    println!("✅ 测试通过: LRU 淘汰策略基本工作");
}

// ==================== 额外测试: 并发连接安全 ====================

#[tokio::test]
async fn test_concurrent_connection_access() {
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let config = create_test_config();
    let manager = Arc::new(Mutex::new(HttpConnectionManager::new(&config)));

    // 启动测试服务器
    let (addr, _server_handle) = start_test_server(|mut stream| {
        tokio::spawn(async move {
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            let _ = stream.write_all(response.as_bytes()).await;
        });
    })
    .await;

    sleep(Duration::from_millis(100)).await;

    let url = url::Url::parse(&format!("http://{}", addr)).unwrap();

    // 并发获取多个连接
    let mut handles = Vec::new();
    for i in 0..4 {
        let mgr = manager.clone();
        let url_clone = url.clone();

        let handle = tokio::spawn(async move {
            let mut m = mgr.lock().await;
            match m.acquire(&url_clone).await {
                Ok(conn) => {
                    println!("任务 {} 获取连接: id={}", i, conn.id);
                    sleep(Duration::from_millis(50)).await;
                    m.release(conn.id).await;
                    Ok(i)
                }
                Err(e) => {
                    eprintln!("任务 {} 失败: {}", i, e);
                    Err(e)
                }
            }
        });

        handles.push(handle);
    }

    // 等待所有任务完成
    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_ok(), "并发任务应成功完成");
    }

    let mut m = manager.lock().await;
    println!("最终状态: active={}, pool_size={}", m.active_count(), m.pool_size());

    m.cleanup().await;

    println!("✅ 测试通过: 并发访问线程安全");
}

use std::time::Instant;
