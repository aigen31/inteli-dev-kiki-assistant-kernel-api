//! Site-study orchestration: URL guard, task prompt and workspace seed.
//!
//! The heavy lifting happens in the harness sidecar. These helpers only shape
//! the job so a study stays inside a small request and token budget.

use serde_json::Value;

use crate::error::{ApiError, ApiResult};

/// Reject anything that is not a public http(s) site. The sidecar runs with
/// network access, so an unchecked URL would be an SSRF primitive.
pub fn validate_site_url(input: &str) -> ApiResult<String> {
    let trimmed = input.trim();

    if trimmed.is_empty() || trimmed.len() > 500 {
        return Err(ApiError::bad_request("site_url must be 1..500 characters"));
    }

    let (scheme, rest) = if let Some(rest) = trimmed.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(ApiError::bad_request("site_url must start with http:// or https://"));
    };

    let authority = rest.split('/').next().unwrap_or("").trim();
    let host = authority
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if host.is_empty() || host.contains('@') {
        return Err(ApiError::bad_request("site_url has no host"));
    }

    if host == "localhost" || host.ends_with(".local") || host.ends_with(".internal") {
        return Err(ApiError::bad_request("site_url must point at a public host"));
    }

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        let private = match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
            }
            std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
        };

        if private {
            return Err(ApiError::bad_request("site_url must point at a public host"));
        }
    }

    Ok(format!("{scheme}://{rest}"))
}

/// Workspace files the agent sees before it starts. Keeping the brief in a file
/// (instead of the prompt) lets the agent re-read it without re-sending tokens.
pub fn workspace_seed(url: &str, agents_md: Option<&str>, instructions: Option<&str>) -> Value {
    let mut seed = serde_json::Map::new();

    let brief = format!(
        "# Бриф: изучение сайта {url}\n\n\
         Цель: собрать факты о проекте, чтобы ассистент CRM понимал контекст задач.\n\n\
         Что нужно выяснить (по приоритету):\n\
         1. Чем занимается проект, для кого, какие услуги/товары и цены.\n\
         2. Технологии, структура страниц, ключевые разделы и формы.\n\
         3. Правила контента и тона, ограничения и то, чего на сайте нет.\n\
         4. Контакты, география, сроки и всё, что влияет на постановку задач.\n\n\
         Дополнительные указания владельца:\n{}\n\n\
         Бюджет: не больше 6 HTTP-запросов. Не скачивай изображения, CSS, JS и шрифты.\n\
         Не обходи весь сайт. Не используй браузер, скриншоты и Playwright.\n",
        instructions
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("—")
    );

    seed.insert("BRIEF.md".to_string(), Value::String(brief));

    if let Some(agents) = agents_md {
        if !agents.trim().is_empty() {
            seed.insert("AGENTS.md".to_string(), Value::String(agents.to_string()));
        }
    }

    Value::Object(seed)
}

/// One task, strict budget, explicit deliverables. The agent is told to use the
/// two writing skills rather than reinvent their structure.
pub fn task_prompt(url: &str, focus: Option<&str>) -> String {
    let focus = focus
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("общий контекст проекта и правила для будущих задач");

    format!(
        "Изучи сайт {url} и обнови проектную инструкцию для ассистента CRM.\n\n\
         Фокус: {focus}.\n\n\
         Порядок работы:\n\
         1. Прочитай BRIEF.md. Если в рабочем каталоге уже есть AGENTS.md — это текущая версия, обнови её и не теряй подтверждённые факты.\n\
         2. Загрузи /sitemap.xml и главную страницу. Выбери не больше 3 дополнительных страниц: услуги/цены, о проекте, одна ключевая услуга. Итого не больше 6 запросов.\n\
         3. HTML сжимай до текста (`sed`/`tr`/`grep`), читай только релевантные фрагменты. Не выводи в контекст всю страницу.\n\
         4. Используй скиллы `create-agentsmd` и `writing-for-agents`, чтобы собрать AGENTS.md по их правилам.\n\
         5. Запиши в рабочем каталоге два файла:\n\
            - AGENTS.md — не больше 120 строк: о проекте, стек и команды, структура и конвенции, контент и цены, ограничения, чего не делать. Только проверенные факты; неизвестное помечай явно.\n\
            - PRICE-NOTES.md — не больше 40 строк: найденные цены, единицы, источники (URL), дата.\n\n\
         Экономия: не повторяй уже прочитанное, не пересказывай BRIEF.md, не вставляй длинные цитаты. Если сайт недоступен — зафиксируй это в AGENTS.md и заверши.\n\n\
         В финальном ответе: 3–6 пунктов — что изучено, что осталось неизвестным, какие файлы записаны."
    )
}

/// Keep only the last `limit` events and cap each one, so a study poll does not
/// ship a megabyte of tool output to the browser.
pub fn compact_events(events: &Value, limit: usize) -> Value {
    let Some(items) = events.as_array() else {
        return Value::Array(Vec::new());
    };

    let start = items.len().saturating_sub(limit);
    let compacted: Vec<Value> = items[start..]
        .iter()
        .map(|event| {
            let mut event = event.clone();
            if let Some(object) = event.as_object_mut() {
                for key in ["text", "result", "input"] {
                    if let Some(value) = object.get(key) {
                        let text = match value {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        };
                        let capped: String = text.chars().take(600).collect();
                        object.insert(key.to_string(), Value::String(capped));
                    }
                }
            }
            event
        })
        .collect();

    Value::Array(compacted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_hosts_are_rejected() {
        assert!(validate_site_url("http://127.0.0.1:8080").is_err());
        assert!(validate_site_url("http://localhost/").is_err());
        assert!(validate_site_url("http://192.168.1.10/").is_err());
        assert!(validate_site_url("ftp://example.com").is_err());
        assert!(validate_site_url("https://inteli-dev.ru").is_ok());
        assert!(validate_site_url("https://koreatur.ru/tury/").is_ok());
    }

    #[test]
    fn seed_keeps_existing_instructions() {
        let seed = workspace_seed("https://example.com", Some("# old"), None);
        assert!(seed.get("AGENTS.md").is_some());
        assert!(seed.get("BRIEF.md").is_some());
    }
}
