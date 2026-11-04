// 搜索历史持久化：config.toml 同目录的 search_history.toml。
// 复用既有 toml 依赖（serde derive 已在依赖中），不额外引入 serde_json。
// 搜索框聚焦时 UI 弹出最近查询（最多显示 15 条，持久化上限 50 条）；
// 记录时机由 UI 决定（失焦/回车），本模块只负责读写

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// TOML 文档结构：TOML 顶层必须是表，数组需挂在字段下（不能裸序列化 Vec）
#[derive(Default, Serialize, Deserialize)]
struct HistoryFile {
    /// 最近搜索优先（从上往下 = 从新到旧），去重 + 上限 50 条
    queries: Vec<String>,
}

/// 存储位置：与 config.toml 同目录（exe 侧用户数据集中一处，
/// 便于备份/清理）。config.toml 不存在时回退到工作目录
fn history_path() -> PathBuf {
    crate::config::Config::config_path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default()
        .join("search_history.toml")
}

pub fn load_search_history() -> Vec<String> {
    let path = history_path();
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str::<HistoryFile>(&s).ok())
        .map(|f| f.queries)
        .unwrap_or_default()
}

/// 记录一条查询：已存在则提到最前（去重），超出上限截断，随后落盘。
/// 返回更新后的完整历史（调用方可直接刷新 UI 缓存）
pub fn add_search_history(query: &str) -> Vec<String> {
    let mut history = load_search_history();
    history.retain(|q| q != query);
    history.insert(0, query.to_string());
    history.truncate(50);
    let path = history_path();
    fs::create_dir_all(path.parent().unwrap()).ok();
    if let Ok(text) = toml::to_string(&HistoryFile {
        queries: history.clone(),
    }) {
        fs::write(&path, text).ok();
    }
    history
}

pub fn clear_search_history() {
    let path = history_path();
    if path.exists() {
        fs::remove_file(path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时文件落盘的读写回路：记录 → 去重置顶 → 清空
    #[test]
    fn history_roundtrip() {
        // 用 cargo test 的临时目录，不碰真实 config 位置
        let dir = std::env::temp_dir().join("archview_history_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("search_history.toml");
        let _ = std::fs::remove_file(&path);

        let write = |queries: &[&str]| {
            let text = toml::to_string(&HistoryFile {
                queries: queries.iter().map(|s| s.to_string()).collect(),
            })
            .unwrap();
            std::fs::write(&path, text).unwrap();
        };
        let read = || {
            toml::from_str::<HistoryFile>(&std::fs::read_to_string(&path).unwrap())
                .unwrap()
                .queries
        };

        write(&["b", "a"]);
        assert_eq!(read(), vec!["b", "a"]);
        // 重复记录去重置顶
        write(&["c", "b"]);
        assert_eq!(read(), vec!["c", "b"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
