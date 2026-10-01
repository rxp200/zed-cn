//! 统一的多语言（i18n）运行时。
//!
//! 文案的唯一事实来源是仓库根目录 `locales/` 下的 JSON 文件，编译期由 `build.rs`
//! 生成 `&'static str` 查找表，因此：
//!
//! - 查找零堆分配、零启动开销，可直接嵌入任何接受 `&'static str` 或
//!   `impl Into<SharedString>` 的 GPUI API；
//! - 新增语言只需在 `locales/` 放一个 `<locale-id>.json` 并重新编译，无需改代码；
//! - 缺 key 时回退英文，再回退 key 本身（由 `script/i18n_check` 在 CI 拦截）。
//!
//! 用法：
//!
//! ```ignore
//! Label::new(i18n::t!("2cd0f3be8738a86c"))
//! Tooltip::text(i18n::t!("a3f9c21b6d0e4f12"))
//! i18n::t!("b91c...", name = path)          // 命名参数 {name}
//! i18n::t_args!("c2d4...", count)           // 位置参数 {}
//! i18n::t_mix!("e5f6..."; count; name = n)  // 同时含 {} 与 {name}
//! ```

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU8, Ordering};

include!(concat!(env!("OUT_DIR"), "/generated.rs"));

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// 当前界面语言。渲染期间频繁读取，代价是一次 Relaxed 原子读。
#[inline]
pub fn current_locale() -> Locale {
    Locale::from_index(CURRENT.load(Ordering::Relaxed))
}

/// 设置界面语言。`zed` 在启动与设置变化时调用。
pub fn set_locale(locale: Locale) {
    CURRENT.store(locale.index(), Ordering::Relaxed);
}

/// 按 key 查找当前语言的文案。
#[inline]
pub fn translate(key: &'static str) -> &'static str {
    translate_in(current_locale(), key)
}

/// 按 key 查找指定语言的文案；缺失时回退英文，再回退 key 本身。
pub fn translate_in(locale: Locale, key: &'static str) -> &'static str {
    lookup(locale, key).unwrap_or_else(|| lookup(Locale::En, key).unwrap_or(key))
}

/// 动态 key 查找（工具与测试用）；命中时返回编译期静态文案。
pub fn try_translate(key: &str) -> Option<&'static str> {
    lookup(current_locale(), key)
}

/// 把占位符原样写回输出（含名字与说明符），用于调用方未提供实参或说明符不受支持时。
fn push_placeholder(out: &mut String, inside: &str) {
    out.push('{');
    out.push_str(inside);
    out.push('}');
}

/// 按格式说明符把值写入输出；说明符不受支持时返回 `false`，由调用方保留占位符原文。
fn write_with_spec(out: &mut String, value: &dyn std::fmt::Display, spec: &str) -> bool {
    let (alternate, rest) = match spec.strip_prefix('#') {
        Some(rest) => (true, rest),
        None => (false, spec),
    };
    if let Some(precision) = rest.strip_prefix('.').and_then(|value| value.parse::<usize>().ok()) {
        let _ = if alternate {
            write!(out, "{value:#.precision$}")
        } else {
            write!(out, "{value:.precision$}")
        };
        true
    } else if rest.is_empty() {
        let _ = if alternate {
            write!(out, "{value:#}")
        } else {
            write!(out, "{value}")
        };
        true
    } else {
        false
    }
}

/// 把翻译模板中的 `{}`（位置参数）与 `{name}`（命名参数）替换为实际值。
///
/// Rust 的 `format!` 只接受字面量模板，而翻译文本来自编译期生成的静态表，
/// 因此这里用等价的轻量插值实现；未提供的参数原样保留，便于发现遗漏。
/// `{{` 按字面左花括号处理。
///
/// `{name:spec}` 中的格式说明符同样受支持，但值以 `&dyn Display` 传入，
/// 因此只处理 Display 系列（`#`、`.N` 及其组合）；`?` 等 Debug 说明符
/// 由调用点在传入前自行 `format!`，否则保留原文并由 `script/i18n_check.py` 拦截。
pub fn interpolate(
    template: &str,
    positional: &[&dyn std::fmt::Display],
    named: &[(&str, &dyn std::fmt::Display)],
) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    let mut next_positional = 0usize;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        if let Some(escaped) = after.strip_prefix('{') {
            out.push('{');
            rest = escaped;
            continue;
        }
        match after.find('}') {
            Some(close) => {
                let inside = &after[..close];
                let (name, spec) = match inside.split_once(':') {
                    Some((name, spec)) => (name, spec),
                    None => (inside, ""),
                };
                if name.is_empty() {
                    match positional.get(next_positional) {
                        Some(value) if write_with_spec(&mut out, *value, spec) => {}
                        Some(_) => push_placeholder(&mut out, inside),
                        None => out.push_str("{}"),
                    }
                    next_positional += 1;
                } else if let Some((_, value)) = named.iter().find(|(key, _)| *key == name) {
                    if !write_with_spec(&mut out, *value, spec) {
                        push_placeholder(&mut out, inside);
                    }
                } else {
                    push_placeholder(&mut out, inside);
                }
                rest = &after[close + 1..];
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 翻译宏：无参数时返回 `&'static str`，带命名参数时返回 `String`。
#[macro_export]
macro_rules! t {
    ($key:literal $(,)?) => {
        $crate::translate($key)
    };
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::interpolate(
            $crate::translate($key),
            &[],
            &[$( (stringify!($name), &$value as &dyn std::fmt::Display) ),+],
        )
    };
}

/// 带位置参数的翻译宏，返回 `String`。
#[macro_export]
macro_rules! t_args {
    ($key:literal, $($value:expr),+ $(,)?) => {
        $crate::interpolate($crate::translate($key), &[$(&$value as &dyn std::fmt::Display),+], &[])
    };
}

/// 同时包含位置占位符 `{}` 与命名占位符 `{name}` 的模板。
///
/// 位置实参与命名实参用分号分成两组，顺序无关：`interpolate` 会按模板中的
/// 出现顺序消费位置实参，并按名字查找命名实参。
#[macro_export]
macro_rules! t_mix {
    ($key:literal; $($positional:expr),* ; $($name:ident = $value:expr),* $(,)?) => {
        $crate::interpolate(
            $crate::translate($key),
            &[$(&$positional as &dyn std::fmt::Display),*],
            &[$( (stringify!($name), &$value as &dyn std::fmt::Display) ),*],
        )
    };
}

/// 与 `t!` 相同，但显式指定语言（测试与预览用）。
#[macro_export]
macro_rules! t_in {
    ($locale:expr, $key:literal $(,)?) => {
        $crate::translate_in($locale, $key)
    };
    ($locale:expr, $key:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::interpolate(
            $crate::translate_in($locale, $key),
            &[],
            &[$( (stringify!($name), &$value as &dyn std::fmt::Display) ),+],
        )
    };
}

/// 从用户设置解析界面语言；未设置或不可识别时用默认语言（中文）。
pub fn locale_from_settings(value: Option<&str>) -> Locale {
    value.and_then(Locale::from_id).unwrap_or(Locale::DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// locales/ 中的自测条目，由 script/i18n 约定维护
    const SELF_TEST_KEY: &str = "0ffe5dce7899a794";

    #[test]
    fn default_locale_is_chinese() {
        assert_eq!(Locale::DEFAULT, Locale::ZhHans);
    }

    #[test]
    fn locale_round_trip_by_id() {
        for locale in Locale::ALL {
            assert_eq!(Locale::from_id(locale.id()), Some(*locale));
        }
    }

    #[test]
    fn unknown_id_and_index_are_rejected() {
        assert_eq!(Locale::from_id("xx-YY"), None);
        assert_eq!(Locale::from_index(200), Locale::DEFAULT);
    }

    #[test]
    fn missing_key_falls_back_to_key_itself() {
        assert_eq!(
            translate_in(Locale::ZhHans, "definitely-missing-key"),
            "definitely-missing-key"
        );
    }

    #[test]
    fn every_locale_has_entries() {
        assert_eq!(translate_in(Locale::En, SELF_TEST_KEY), "i18n self test");
        assert_eq!(translate_in(Locale::ZhHans, SELF_TEST_KEY), "i18n 自测");
    }

    #[test]
    fn locales_expose_native_display_names() {
        assert_eq!(Locale::ZhHans.display_name(), "简体中文");
        assert_eq!(Locale::En.display_name(), "English");
        let names = language_names();
        for locale in Locale::ALL {
            assert!(!locale.display_name().is_empty());
            assert_eq!(
                names
                    .iter()
                    .find(|(id, _)| *id == locale.id())
                    .map(|(_, name)| *name),
                Some(locale.display_name())
            );
        }
    }

    /// 语言是进程级全局状态，因此所有会切换语言的断言必须集中在同一个测试里，
    /// 否则并行运行的测试会互相覆盖当前语言。
    #[test]
    fn locale_switching_and_macros() {
        set_locale(Locale::En);
        assert_eq!(current_locale(), Locale::En);
        assert_eq!(translate(SELF_TEST_KEY), "i18n self test");
        let text: &str = t!("0ffe5dce7899a794");
        assert_eq!(text, "i18n self test");
        let rendered: String = t!("5d78ca02ae0f71d2", value = 42);
        assert_eq!(rendered, "i18n self test 42");

        set_locale(Locale::ZhHans);
        assert_eq!(translate(SELF_TEST_KEY), "i18n 自测");
        assert_eq!(current_locale(), Locale::DEFAULT);
    }

    #[test]
    fn interpolate_supports_positional_and_escapes() {
        assert_eq!(
            interpolate("克隆 {} 到 {}", &[&"a", &"b"], &[]),
            "克隆 a 到 b"
        );
        assert_eq!(
            interpolate(
                "{name} 有 {count} 项",
                &[],
                &[("name", &"列表"), ("count", &3)]
            ),
            "列表 有 3 项"
        );
        assert_eq!(interpolate("{{", &[], &[]), "{");
        assert_eq!(
            interpolate("{name}：{} 项", &[&3], &[("name", &"列表")]),
            "列表：3 项"
        );
        assert_eq!(interpolate("{missing}", &[], &[]), "{missing}");
    }

    #[test]
    fn interpolate_supports_display_format_specs() {
        assert_eq!(
            interpolate("进度：{percentage:.0}%", &[], &[("percentage", &42.6_f32)]),
            "进度：43%"
        );
        assert_eq!(
            interpolate("{} 个文件 · {:.2} MiB", &[&3, &1.5_f64], &[]),
            "3 个文件 · 1.50 MiB"
        );
        assert_eq!(
            interpolate("信任 {:} 文件夹", &[&"/tmp"], &[]),
            "信任 /tmp 文件夹"
        );
        assert_eq!(
            interpolate("失败：{error:#}", &[], &[("error", &"外层：内层")]),
            "失败：外层：内层"
        );
    }

    #[test]
    fn interpolate_keeps_unsupported_and_missing_placeholders() {
        // Debug 说明符无法用 `&dyn Display` 渲染，保留原文以便检查工具发现。
        assert_eq!(
            interpolate("从 {source:?} 复制", &[], &[("source", &"a")]),
            "从 {source:?} 复制"
        );
        assert_eq!(interpolate("{}", &[], &[]), "{}");
    }
}
