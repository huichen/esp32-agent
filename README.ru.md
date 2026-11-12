# esp32-agent

**Русский** · [English](README.md) · [简体中文](README.zh.md) · [Français](README.fr.md) · [Español](README.es.md) · [العربية](README.ar.md)

Прошивка для ESP32, которая выполняет собственный цикл агента и отдаёт эндпоинт MCP, так что
одна модель может управлять платой напрямую, а любой внешний агент — пользоваться теми же
инструментами по HTTP.

Плату прошивают, она подключается к WiFi и опрашивает шины, для которых есть драйверы. Каждое
ответившее устройство становится инструментами MCP. Агент на плате видит те же инструменты и
отвечает ими; новый датчик можно объявить в JSON, пока плата работает.

> Цикл агента следует [FutureOS](https://github.com/futuregene/future-os): транскрипт, журнал
> вызовов инструментов, события потока, адаптеры протоколов, индекс памяти. Там, где прошивка
> отличается (килобайты вместо гигабайт RAM, никаких сводок, написанных моделью), код
> объясняет почему.

| Что | В подробностях |
|---|---|
| Модель работает на плате | Цикл, инструменты, память и транскрипт — это прошивка; модель — любой эндпоинт, совместимый с OpenAI, или Anthropic. [Как задать модель](docs/model.md) |
| Набор инструментов — это документ | `manifest_apply` проверяет манифест JSON и компилирует его в инструменты, действующие уже в том же ходу. [Манифест](docs/manifest.md) |
| Сама находит своё железо | I2C, 1-Wire и объявленные выводы опрашиваются при загрузке; инструменты получает только то, что отвечает. [Возможности](docs/capabilities.md) |
| Один набор инструментов, два вызывающих | Эндпоинт MCP и агент на плате видят одни и те же инструменты из одного описания. [HTTP и MCP API](docs/http-api.md) |
| Консоль по HTTPS | С телефона — чат с моделью и настройка устройств, инструментов, расписания, журналов, WiFi и модели; сертификат плата выпускает сама. [Веб-консоль](docs/web-console.md) |
| Работает без присмотра | Задания по времени и правила по датчикам, которые запускают задание, когда показание пересекает порог. [Работа по расписанию](docs/scheduling.md) |
| Shell и язык скриптов | `shell` с 26 командами с именами из NuttX и интерпретатор для циклов по вызовам инструментов. [Shell](docs/shell.md) · [Script](docs/scripting.md) |
| Помнит, что ей сказали | Постоянные указания, `/AGENTS.md`, индекс `/memory.md` и навыки, собранные в промпт на каждом ходу. [Что модель видит всегда](docs/context.md) |
| Никакого облака на пути | HTTPS с платы, сертификат, который она генерирует, один PIN на три вида вызывающих. [Аутентификация](docs/security.md) |
| Проверяемо без железа | Симулятор запускает тот же `agent-core` на виртуальной плате. [Симулятор](docs/simulator.md) |

## Прошивка и подключение

```bash
cargo install espup espflash ldproxy
espup install
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

Без сохранённых учётных данных плата поднимает собственную точку доступа. Подключитесь к ней с
телефона и откройте `http://192.168.4.1/`, либо отправьте `provision <ssid> <password>` по USB.
Учётные данные уходят в NVS и никогда в образ.

```bash
curl -sS http://<board>/agent.json | jq '.endpoints | length'
curl -sS -X POST http://<board>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools | length'
curl -k -sS https://<board>/api/tls | jq -r .fingerprint
```

Порт 443 отдаёт те же маршруты по TLS с сертификатом, сгенерированным при первой загрузке;
браузеры предупреждают, поэтому сравните отпечаток. Порт 80 остаётся открытым, так что плохой
сертификат стоит одного URL, а не платы. `GET /agent.json` — весь интерфейс как данные: 40
маршрутов с методом, телом, возвратом и аутентификацией, плюс каждая записываемая настройка с её
типом и значением по умолчанию.

## Структура репозитория

`crates/` собирается и тестируется на хосте; `firmware/` — бэкенд ESP-IDF. `agent-core` — это
плата за одним трейтом `Hardware`, без кода под конкретный чип, поэтому `cargo test` гоняет его
на ноутбуке; `agent-sim` реализует этот трейт поверх файла регистров. Их слияние вложило бы
симулятор в каждый образ, а зависимости прошивки — в каждый тест. `firmware/` исключён из
workspace, потому что ему нужны `cargo +esp`, `-Zbuild-std` и выкачанный ESP-IDF.

```
crates/agent-core/    hardware.rs (the trait) · drivers/ (one module per chip) · boards.rs ·
                      manifest.rs · agent.rs + llm.rs · sched.rs · tools/ ·
                      http.rs · app.rs · sitemap.rs + settings.rs · virtual_board.rs
crates/agent-sim/     the virtual board behind an HTTP port
firmware/             main.rs (boot order) · runner.rs (agent thread, TLS) ·
                      esp_hw.rs (GPIO/I2C/PWM/ADC/RMT/I2S) · console.rs · wifi.rs
```

## Состояние

| Часть | Состояние |
|---|---|
| `agent-core` | 427 модульных тестов и 14 интеграционных проходят; `scripts/smoke.ps1` проходит от начала до конца |
| Симулятор | консоль, MCP и цикл агента работают от начала до конца |
| Прошивка | прошита на ESP32-S3-DevKitC-1 N16R8 (IDF v5.5.2): 42 инструмента, OLED 128×32, микрофон и усилитель I²S, WS2812, консоль по TLS, ходы модели по HTTPS |
| Не реализовано | OTA, SD-карта, DHT11/22, камера. См. `ROADMAP.md` |

Цифры на стенде: ход с четырьмя инструментами за 13.7 s; консоль по TLS за 0.55 s; сканирование
I²C стабильно на 8 прогонах; внутренняя SRAM 5–6 kB свободно после страницы по TLS — самое узкое
место здесь.

## Документация

[Архитектура](docs/architecture.md) · [Манифест](docs/manifest.md) · [Возможности](docs/capabilities.md) ·
[HTTP и MCP](docs/http-api.md) · [Добавление устройства](docs/adding-a-device.md) · [Прошивка](docs/flashing.md) ·
[Диагностика](docs/troubleshooting.md) · [Цикл агента](docs/agent-loop.md) · [Поток](docs/agent-stream.md) ·
[Skills](docs/agent-skills.md) · [Память](docs/agent-memory.md) · [Scripting](docs/scripting.md) ·
[Model](docs/model.md) · [Web console](docs/web-console.md) · [Scheduling](docs/scheduling.md) · [Shell](docs/shell.md) · [Context layers](docs/context.md) · [Security](docs/security.md) · [Simulator](docs/simulator.md) · [Performance](docs/performance.md)

## Участие в проекте

Новая периферия начинается как фрагмент манифеста в `examples/manifests/`; драйвер на Rust
нужен только для таймингов, калибровки или состояния. `CATALOG` — место, где драйвер
регистрирует себя. Соглашения — в [AGENTS.md](AGENTS.md) и [CONTRIBUTING.md](CONTRIBUTING.md).

## Лицензия

MIT. См. [LICENSE](LICENSE).
