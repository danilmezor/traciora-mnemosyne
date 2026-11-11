//! Renderer UI-language selection and Moment aliases, recovered from rb/lb/hb.
//! Intl.Collator's default resolved locale is a separate imported VM context.
const SUPPORTED: &[&str] = &[
    "am", "ar", "be", "bg", "bn", "ca", "cs", "da", "de", "el", "en", "en-GB", "es", "fa", "fi",
    "fr", "ga", "gl", "he", "hu", "id", "it", "ja", "ka", "kab", "km", "ko", "lv", "ms", "ne",
    "nl", "no", "pl", "pt", "pt-BR", "ro", "ru", "sa", "si", "sk", "sq", "sr", "sv", "ta", "th",
    "tr", "uk", "uz", "vi", "zh", "zh-TW",
];
/// localStorage language is used when nonempty, including unknown IDs and
/// custom translation paths. Only navigator fallback is constrained to app IDs.
pub fn select(storage: Option<&str>, navigator: &str) -> String {
    if let Some(value) = storage.filter(|value| !value.is_empty()) {
        return value.into();
    }
    if SUPPORTED.contains(&navigator) {
        return navigator.into();
    }
    let base = navigator.split('-').next().unwrap_or("");
    if SUPPORTED.contains(&base) {
        base.into()
    } else {
        "en".into()
    }
}
pub fn moment_id(language: &str) -> String {
    let lower = language.to_lowercase();
    match lower.as_str() {
        "zh" => "zh-cn".into(),
        "cz" => "cs".into(),
        "no" => "nb".into(),
        _ => lower,
    }
}
pub fn supported_languages() -> &'static [&'static str] {
    SUPPORTED
}
