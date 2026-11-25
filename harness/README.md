# Harness runner

HTTP-обёртка над `dsh --profile headless`. Отдельный сервис: ядро не запускает
процессы и не знает, как устроен DeepSeek Harness.

## Контракт

- `POST /v1/runs` `{task, seed, timeout_seconds}` → `202 {run_id, status}`
- `GET /v1/runs/{id}` → статус, события, финальный текст, файлы воркспейса
- `POST /v1/runs/{id}/cancel`
- `GET /health/live`, `/health/ready`

Авторизация — `Authorization: Bearer $HARNESS_TOKEN`. Каждый запуск получает
`/data/workspaces/<run_id>`; агент видит только этот каталог. События
(`tool_call`, `tool_result`, `thinking`, `text`) сохраняются, чтобы UI показывал
процесс, а не просто ждал.

## Экономия токенов

- `lean.yml` отключает subagent/workflow/MCP/plan/goal/web-search;
- бриф лежит в `BRIEF.md`, а не в промпте;
- промпт ограничивает изучение 6 HTTP-запросами и требует сжимать HTML;
- результат — `AGENTS.md` (≤120 строк) и `PRICE-NOTES.md` (≤40 строк).

## Переменные

`HARNESS_TOKEN`, `DEEPSEEK_API_KEY` (или `KERNEL_CLOUD_API_KEY`), `MAX_CONCURRENT`,
`DEFAULT_TIMEOUT`, `DATA_DIR`, `DSH_HOME`.
