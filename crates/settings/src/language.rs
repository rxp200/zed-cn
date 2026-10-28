//! `language` 顶层设置到界面语言的桥接。
//!
//! 设置在 `settings_content::SettingsContent::language`，本模块在
//! `settings::init` 时应用，并跟随设置文件变化，使整个进程的语言切换即时生效。

use crate::{Settings, SettingsStore, UiLanguage};
use gpui::App;
use i18n::{self, Locale};

/// `language` 顶层设置的结构化视图。
#[derive(Clone, Debug)]
pub struct LanguageSettings {
    pub language: Option<UiLanguage>,
}

impl Settings for LanguageSettings {
    fn from_settings(content: &settings_content::SettingsContent) -> Self {
        Self {
            language: content.language.clone(),
        }
    }
}

/// 在 `settings::init` 中调用：按当前设置应用语言并观察后续变化。
///
/// 测试模式跳过：测试应用默认使用英文（见 `gpui::TestAppContext`），
/// 避免默认设置里的 `language` 值改变测试可见的文案。
pub fn init(cx: &mut App) {
    LanguageSettings::register(cx);
    if cx.is_test() {
        return;
    }
    apply(cx);
    cx.observe_global::<SettingsStore>(|cx| apply(cx)).detach();
}

fn apply(cx: &mut App) {
    let language = LanguageSettings::get_global(cx)
        .language
        .as_ref()
        .map(UiLanguage::as_str);
    let locale = resolve_locale(language);
    if i18n::current_locale() != locale {
        i18n::set_locale(locale);
        cx.refresh_windows();
    }
}

fn resolve_locale(language: Option<&str>) -> Locale {
    language
        .and_then(Locale::from_id)
        .unwrap_or(Locale::DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn language_settings(language: Option<&str>) -> LanguageSettings {
        let content = settings_content::SettingsContent {
            language: language.map(|id| UiLanguage(id.to_string())),
            ..Default::default()
        };
        LanguageSettings::from_settings(&content)
    }

    #[test]
    fn unset_and_unknown_languages_fall_back_to_chinese() {
        for language in [None, Some("xx-YY"), Some("")] {
            let settings = language_settings(language);
            let resolved = resolve_locale(settings.language.as_ref().map(UiLanguage::as_str));
            assert_eq!(resolved, Locale::ZhHans);
        }
    }

    #[test]
    fn known_language_ids_resolve() {
        for (id, expected) in [("zh-Hans", Locale::ZhHans), ("en", Locale::En)] {
            let settings = language_settings(Some(id));
            let resolved = resolve_locale(settings.language.as_ref().map(UiLanguage::as_str));
            assert_eq!(resolved, expected);
        }
    }
}
