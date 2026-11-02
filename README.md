# Kiki Assistant Kernel API

Одно ядро ассистента Кики для сайта inteli-dev.ru, CRM и будущих каналов.

## Что здесь есть

- `POST /v1/chat`: диалог, выбор локальной или облачной модели, tool loop.
- `GET /v1/conversations/{id}`: история диалога.
- `DELETE /v1/conversations/{id}`: удаление истории.
- `POST /v1/conversations/{id}/notes`: заметка после подтверждённого действия CRM.
- `/health/live` и `/health/ready`.
- `openapi.yaml`: контракт API 1.0.

## Границы

Ядро не читает базы сайта и CRM. Оно вызывает внутренние типизированные
инструменты каналов:

```
GET  {provider}/profile?actor_id=...&actor_role=...
POST {provider}/tools/{tool}
```

Провайдер возвращает системные правила канала, JSON-схемы инструментов и
подсказки. Ядро хранит только диалоги, сообщения и usage.

## Запуск

```bash
cp .env.example .env
# заполнить токены и адрес локальной модели
docker compose up -d --build
curl http://127.0.0.1:8090/health/ready
```

Сеть `inteli-dev-assistant` создаётся этим compose. Сайт и CRM подключаются к
ней как к external network.

## Маршрутизация данных

- `public`: облачная модель, если настроена; иначе локальная.
- `internal` и `secret`: только локальная модель. Если локальной нет,
  запрос завершается `503`, а не уходит в облако.
- `KERNEL_ALLOW_CLOUD_FOR_INTERNAL=true` существует только для локальной
  разработки. В продакшене оставляйте `false`.

## Токены

У каждого канала свой входящий токен (`KERNEL_SITE_TOKEN`,
`KERNEL_CRM_TOKEN`) и свой токен провайдера
(`KERNEL_SITE_PROVIDER_TOKEN`, `KERNEL_CRM_PROVIDER_TOKEN`). Токены не
передаются в браузер.

## Локальная проверка без реальной модели

Для проверки tool loop можно поднять OpenAI-совместимый мок в сети ядра:

```bash
docker run --rm --name kiki-mock-llm --network inteli-dev-assistant \
  -v "$PWD/scripts/mock-llm.js:/app/mock.js:ro" \
  node:24-alpine node /app/mock.js
```

Укажите в `.env`:

```dotenv
KERNEL_LOCAL_BASE_URL=http://kiki-mock-llm:18091/v1
KERNEL_LOCAL_MODEL=mock
```

Мок не является частью продакшена. Для CRM в продакшене нужен реальный
локальный OpenAI-совместимый endpoint.

## Порядок запуска

1. `docker compose up -d --build` в этом репозитории.
2. `docker compose up -d` в `inteli-dev.ru` и `inteli-dev-crm`.
3. Проверить `/health/ready` ядра и `/api/health` сайта.

Сайт и CRM подключаются к внешней сети `inteli-dev-assistant`. Если ядро
остановлено, их контейнеры не смогут создать сеть; сначала поднимите ядро.
