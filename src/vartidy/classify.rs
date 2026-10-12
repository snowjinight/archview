//! var 文件名分类规则（移植自 VarTidying/class_var.py，命名约定与分类逐条对齐）。
//!
//! 合法命名：`作者.内容.版本.var` —— 至少四段、版本号纯数字、作者与内容 strip 后非空。
//! 分类结果直接决定 VarTidy 的落盘行为：合法 → 仓库/作者/；非法 → 只进报告不落盘。

/// 分类结果
pub enum ClassifyResult {
    /// 合法：(作者, 内容, 版本)——目标路径 = 仓库/作者/文件名
    Valid {
        author: String,
        #[allow(dead_code)] // 版本号当前仅参与命名校验，保留以便报告/未来去重策略使用
        content: String,
        #[allow(dead_code)]
        version: String,
    },
    Invalid(InvalidKind),
}

/// 非法类别（标签与 class_var.py 的 invalid_files.txt 前缀一致）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidKind {
    /// 非 .var 文件（图片、txt 等杂项）
    NotVar,
    /// 段数不足四段（作者.内容.版本.var）
    Format,
    /// 版本号非纯数字
    Version,
    /// 作者名为空
    Author,
    /// 内容部分为空
    Content,
}

impl InvalidKind {
    pub fn label(&self) -> &'static str {
        match self {
            InvalidKind::NotVar => "非VAR文件",
            InvalidKind::Format => "格式错误",
            InvalidKind::Version => "版本号错误",
            InvalidKind::Author => "作者名缺失",
            InvalidKind::Content => "内容缺失",
        }
    }
}

/// 对文件名（不含路径）做 var 命名分类。
/// 与 class_var.py 保持一致的细节：
/// - `.var` 后缀判断大小写敏感（VaM 生态约定小写 .var，`.VAR` 按"非VAR文件"记录）
/// - 版本号 isdigit：ASCII 数字且非空
/// - 作者/内容 strip 首尾空白后须非空
pub fn classify_file_name(file_name: &str) -> ClassifyResult {
    if !file_name.ends_with(".var") {
        return ClassifyResult::Invalid(InvalidKind::NotVar);
    }
    let parts: Vec<&str> = file_name.split('.').collect();
    // endswith(".var") 保证末段为 var，这里只需段数校验
    if parts.len() < 4 {
        return ClassifyResult::Invalid(InvalidKind::Format);
    }
    let version = parts[parts.len() - 2];
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit()) {
        return ClassifyResult::Invalid(InvalidKind::Version);
    }
    let author = parts[0].trim();
    if author.is_empty() {
        return ClassifyResult::Invalid(InvalidKind::Author);
    }
    // 内容部分可含点（作者.多段.内容.版本.var），合并中段
    let content = parts[1..parts.len() - 2].join(".").trim().to_string();
    if content.is_empty() {
        return ClassifyResult::Invalid(InvalidKind::Content);
    }
    ClassifyResult::Valid {
        author: author.to_string(),
        content,
        version: version.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid(name: &str) -> String {
        match classify_file_name(name) {
            ClassifyResult::Valid { author, .. } => author,
            ClassifyResult::Invalid(k) => panic!("[{name}] 应合法，却判为 {}", k.label()),
        }
    }

    fn invalid(name: &str) -> InvalidKind {
        match classify_file_name(name) {
            ClassifyResult::Valid { .. } => panic!("[{name}] 应非法，却判为合法"),
            ClassifyResult::Invalid(k) => k,
        }
    }

    #[test]
    fn classify_valid_names() {
        assert_eq!(valid("Alice.look1.1.var"), "Alice");
        // 内容含点：中段整体合并
        assert_eq!(valid("Bob.hair.pack.v2.10.var"), "Bob");
        // 首尾空白 strip
        assert_eq!(valid(" Alice .look1.1.var"), "Alice");
    }

    #[test]
    fn classify_invalid_kinds() {
        // 五类非法（与 class_var.py 的记录前缀一一对应）
        assert_eq!(invalid("readme.txt"), InvalidKind::NotVar);
        assert_eq!(invalid("preview.jpg"), InvalidKind::NotVar);
        assert_eq!(invalid("no_ext"), InvalidKind::NotVar);
        assert_eq!(invalid("Alice.look1.var"), InvalidKind::Format); // 仅三段
        assert_eq!(invalid("Alice.var"), InvalidKind::Format);
        assert_eq!(invalid("Alice.look1.v2.var"), InvalidKind::Version); // 版本非纯数字
        assert_eq!(invalid("Alice.look1..var"), InvalidKind::Version); // 版本为空
        assert_eq!(invalid(".look1.1.var"), InvalidKind::Author); // 作者为空
        assert_eq!(invalid("Alice..1.var"), InvalidKind::Content); // 内容为空
    }

    #[test]
    fn classify_var_suffix_case_sensitive() {
        // class_var.py 的 endswith('.var') 大小写敏感：.VAR 记为非VAR文件
        assert_eq!(invalid("Alice.look1.1.VAR"), InvalidKind::NotVar);
        assert_eq!(invalid("Alice.look1.1.Var"), InvalidKind::NotVar);
    }
}
