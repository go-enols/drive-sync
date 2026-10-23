//! 简化 semver（`major.minor.patch[-prerelease][+build]`）解析与比较。
//!
//! 只实现自动更新需要的部分：忽略 build 元数据，prerelease 按 semver §11 规则排序。
//! 允许版本号以 `v`/`V` 开头（Git tag 风格），也允许 `pubspec` 风格的 `0.2.0`。

use std::cmp::Ordering;
use std::fmt;

use crate::UpdateError;

/// 解析后的版本号。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// 预发布标识（`-` 之后、`+` 之前），空表示正式版。
    pub pre: Vec<String>,
}

impl Version {
    /// 解析版本号字符串（允许前缀 `v`，允许缺省 minor/patch 视为 0）。
    pub fn parse(raw: &str) -> Result<Self, VersionError> {
        let trimmed = raw.trim();
        let text = trimmed
            .strip_prefix('v')
            .or_else(|| trimmed.strip_prefix('V'))
            .unwrap_or(trimmed);
        if text.is_empty() {
            return Err(VersionError(format!("版本号为空: {raw:?}")));
        }
        // 丢弃 build 元数据（`+...`）。
        let text = text.split('+').next().unwrap_or(text);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut segments = core.split('.');
        let major = parse_segment(raw, segments.next().unwrap_or_default())?;
        // 允许 `1` 或 `1.2` 这种简写（按缺省 0 处理）。
        let minor = match segments.next() {
            Some(part) => parse_segment(raw, part)?,
            None => 0,
        };
        let patch = match segments.next() {
            Some(part) => parse_segment(raw, part)?,
            None => 0,
        };
        if segments.next().is_some() {
            return Err(VersionError(format!("版本号段过多: {raw:?}")));
        }
        let pre = match pre {
            None => Vec::new(),
            Some("") => {
                return Err(VersionError(format!("预发布标识为空: {raw:?}")));
            }
            Some(pre) => {
                let mut parts = Vec::new();
                for ident in pre.split('.') {
                    if ident.is_empty() {
                        return Err(VersionError(format!("预发布标识为空: {raw:?}")));
                    }
                    if !ident
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    {
                        return Err(VersionError(format!("预发布标识非法: {raw:?}")));
                    }
                    parts.push(ident.to_string());
                }
                parts
            }
        };
        Ok(Self {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// 是否为预发布版本。
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

/// 解析单个数字段（空串/非数字/溢出都算非法）。
fn parse_segment(raw: &str, part: &str) -> Result<u64, VersionError> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(VersionError(format!("版本号段非数字: {raw:?}")));
    }
    part.parse::<u64>()
        .map_err(|error| VersionError(format!("版本号段溢出 {part:?}: {error}")))
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

/// 版本号解析/比较错误。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionError(pub String);

impl fmt::Display for VersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VersionError {}

impl From<VersionError> for UpdateError {
    fn from(error: VersionError) -> Self {
        UpdateError::Version(error.0)
    }
}

/// semver §11 的 prerelease 优先级规则：
/// - 正式版 > 任意预发布版；
/// - 逐段比较：数字段按数值比较且低于字母段；字母段按 ASCII 字典序；前缀相同时段数少的更低。
fn cmp_prerelease(a: &[String], b: &[String]) -> Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        (false, false) => {}
    }
    for (left, right) in a.iter().zip(b.iter()) {
        let left_num = left.bytes().all(|b| b.is_ascii_digit());
        let right_num = right.bytes().all(|b| b.is_ascii_digit());
        let ord = match (left_num, right_num) {
            (true, true) => {
                let l = left.parse::<u64>().unwrap_or(u64::MAX);
                let r = right.parse::<u64>().unwrap_or(u64::MAX);
                l.cmp(&r)
            }
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => left.cmp(right),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.major
            .cmp(&other.major)
            .then_with(|| self.minor.cmp(&other.minor))
            .then_with(|| self.patch.cmp(&other.patch))
            .then_with(|| cmp_prerelease(&self.pre, &other.pre))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// 解析版本号（`Version::parse` 的便捷包装）。
pub fn parse_version(raw: &str) -> Result<Version, UpdateError> {
    Ok(Version::parse(raw)?)
}

/// 比较两个版本号：返回 `Ordering::Less` 表示 `a < b`。
pub fn compare_versions(a: &str, b: &str) -> Result<Ordering, UpdateError> {
    Ok(Version::parse(a)?.cmp(&Version::parse(b)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v_prefix_and_build_metadata() {
        assert_eq!(Version::parse("v0.2.0").unwrap().to_string(), "0.2.0");
        assert_eq!(Version::parse("V1.2.3+abc.1").unwrap().to_string(), "1.2.3");
        assert_eq!(Version::parse(" 0.2.0 ").unwrap().to_string(), "0.2.0");
        assert_eq!(Version::parse("1").unwrap().to_string(), "1.0.0");
        assert_eq!(Version::parse("1.2").unwrap().to_string(), "1.2.0");
    }

    #[test]
    fn rejects_invalid_versions() {
        for bad in [
            "",
            "v",
            "abc",
            "1.2.3.4",
            "1..3",
            "1.2.x",
            "1.2.3-",
            "1.2.3-a..b",
        ] {
            assert!(Version::parse(bad).is_err(), "应拒绝非法版本号 {bad:?}");
        }
    }

    #[test]
    fn orders_releases() {
        assert_eq!(
            compare_versions("0.2.0", "0.1.9").unwrap(),
            Ordering::Greater
        );
        assert_eq!(compare_versions("0.2.0", "0.2.0").unwrap(), Ordering::Equal);
        assert_eq!(
            compare_versions("v0.2.0", "0.2.0").unwrap(),
            Ordering::Equal
        );
        assert_eq!(
            compare_versions("0.2.10", "0.2.9").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("1.0.0", "0.99.99").unwrap(),
            Ordering::Greater
        );
    }

    #[test]
    fn orders_prereleases_per_semver() {
        assert_eq!(
            compare_versions("0.2.0-rc.1", "0.2.0").unwrap(),
            Ordering::Less
        );
        assert_eq!(
            compare_versions("0.2.0-alpha", "0.2.0-beta").unwrap(),
            Ordering::Less
        );
        assert_eq!(
            compare_versions("0.2.0-alpha", "0.2.0-alpha.1").unwrap(),
            Ordering::Less
        );
        assert_eq!(
            compare_versions("0.2.0-alpha.10", "0.2.0-alpha.9").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("0.2.0-1", "0.2.0-alpha").unwrap(),
            Ordering::Less
        );
    }
}
