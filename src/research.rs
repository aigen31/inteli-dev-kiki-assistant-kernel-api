//! Ad-hoc DeepSeek Harness research jobs.
//!
//! A research job is a free-form task for the harness sidecar: open a page,
//! compare sources, collect facts. Unlike a site study it is not persisted —
//! the caller starts a run, polls it and cancels it when the answer arrived.

use crate::error::{ApiError, ApiResult};
use crate::study::validate_site_url;

/// Задача должна быть содержательной, но не превращаться в простыню.
pub const MIN_TASK_CHARS: usize = 8;
pub const MAX_TASK_CHARS: usize = 2000;

/// Проверяет и нормализует текст задачи.
pub fn validate_task(input: &str) -> ApiResult<String> {
    let trimmed = input.trim();

    if trimmed.chars().count() < MIN_TASK_CHARS {
        return Err(ApiError::bad_request(format!(
            "task must be at least {MIN_TASK_CHARS} characters"
        )));
    }

    if trimmed.chars().count() > MAX_TASK_CHARS {
        return Err(ApiError::bad_request(format!(
            "task must be at most {MAX_TASK_CHARS} characters"
        )));
    }

    Ok(trimmed.to_string())
}

/// Собирает промпт исследователя и проверяет URL тем же SSRF-фильтром,
/// что и изучение сайта.
pub fn task_prompt(task: &str, url: Option<&str>) -> ApiResult<String> {
    let task = validate_task(task)?;

    let url = match url.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => Some(validate_site_url(value)?),
        None => None,
    };

    let mut prompt = String::from(
        "Ты — исследовательский агент. Работай только с публичными источниками и фактами, \
которые видишь сам: не додумывай цифры, названия и цитаты.\n",
    );

    if let Some(url) = url {
        prompt.push_str(&format!(
            "\nОсновной источник: {url}\nОткрой его и опирайся на содержимое страницы; при необходимости \
перейди по ссылкам с неё.\n"
        ));
    }

    prompt.push_str("\nЗадача:\n");
    prompt.push_str(&task);
    prompt.push_str(
        "\n\nФормат ответа: 3–7 коротких пунктов по делу, без вступлений. \
В конце отдельной строкой «Источники:» перечисли URL, которые реально открыл. \
Если данных не хватает — так и напиши и укажи, что именно нужно уточнить.\n",
    );

    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_short_tasks() {
        assert!(validate_task("коро").is_err());
    }

    #[test]
    fn trims_task_and_keeps_url_out_of_prompt_when_absent() {
        let prompt = task_prompt("  Собери цены на тарифы  ", None).expect("prompt");

        assert!(prompt.contains("Собери цены на тарифы"));
        assert!(!prompt.contains("Основной источник"));
    }

    #[test]
    fn rejects_private_urls() {
        assert!(task_prompt("Проверь сайт", Some("http://localhost:8080/admin")).is_err());
        assert!(task_prompt("Проверь сайт", Some("ftp://example.com")).is_err());
    }

    #[test]
    fn includes_valid_url() {
        let prompt = task_prompt("Что нового на главной?", Some("https://koreatur.ru")).expect("prompt");

        assert!(prompt.contains("https://koreatur.ru"));
    }
}
