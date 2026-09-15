# Neutral Browser — НЕЙТРАЛЬНЫЙ БРАУЗЕР ПОЛЬЗОВАТЕЛЯ

Инструмент пользователя: `USER → BROWSER → WEB`. Без обязательного корпоративного
посредника, аккаунта, облака, телеметрии и рекламы. Спецификация — МАСТЕР-ПРОМПТ
(`docs/MASTER_PROMPT.md`). Правило проекта: **каждый статус подтверждается машинным
выводом, а не обещанием**.

Текущая версия: **0.4.0 / STAGE3_DOM**

Цепочка этапа 3: `URL → Broker → TCP → HTTP/1.1 → тело → HTML-токенизатор → DOM → машинный отчёт`

## Что уже РАБОТАЕТ (VERIFIED — машинное доказательство)

| Компонент | Статус | Доказательство |
|---|---|---|
| Capability-модель `Policy → Broker → Decision` (этап 1) | VERIFIED | `SELF_TEST=PASS`, `WEB_TO_OS=DENY`, `WEB_NETWORK=ALLOW`, `NON_WEB_NETWORK=DENY` |
| Реальная сборка Rust, ноль внешних зависимостей | VERIFIED | `cargo build` SUCCESS; `Cargo.lock` зафиксирован; `[dependencies]` пуст (проверяет `tests/architecture.py`) |
| URL-парсер (подмножество: http, IPv4/IPv6, порт, путь, query) | VERIFIED | `URL_PARSE=PASS` |
| Разбор HTTP/1.1-ответа (статус, заголовки) | VERIFIED | `HTTP_PARSE=PASS` |
| Chunked-декодер (RFC 9112) | VERIFIED | `CHUNKED_DECODE=PASS` + `E2E_T2_SHA` |
| SHA-256 (FIPS 180-4, целостность тела) | VERIFIED | `SHA256_VECTORS=PASS` (3 опубликованных вектора NIST) |
| Fetch-цепочка целиком через брокер | VERIFIED | `E2E_TEST=PASS` — случаи T1–T8 на живом локальном сервере |
| Путь отказа брокера (сокет не открывается) | VERIFIED | `E2E_T4_DENIED`, `NETWORK=CLOSED` |
| Структурный инвариант: единственный `TcpStream::connect` в `src/network.rs`, за брокером | VERIFIED | `STAGE2_STRUCTURE=PASS` |
| HTML-токенизатор (подмножество WHATWG §13.5): данные, теги, семейство атрибутов, комментарии (включая `--!>`), фиктивный комментарий, DOCTYPE (имя), RAWTEXT/RCDATA/PLAINTEXT, символьные ссылки | VERIFIED | `HTML_TOKENIZER=PASS` (самотест) + 48 юнит-тестов `cargo test` |
| Числовые символьные ссылки с нормативной таблицей windows-1252 (0x80–0x9F) и правилами U+FFFD | VERIFIED | юнит-тесты: `&#128;`→€, `&#xD800;`→FFFD, `&#0;`→FFFD |
| Конструктор дерева DOM (подмножество WHATWG §13.2.6): режимы initial/before html/before head/in head/after head/in body/text/after body/after after body, подразумеваемые html/head/body, void-элементы, автозакрытие p/li/dd/dt, `</br>`→`<br>`, `image`→`img`, пропуск LF после pre/textarea/listing | VERIFIED | `DOM_TREE=PASS` (самотест) + юнит-тесты с точным дампом дерева |
| Канонический дамп дерева (замороженный формат, итеративный — без рекурсии) | VERIFIED | `E2E_T9_DUMP_EXACT` — побайтовое равенство на хитром документе |
| Вложенность 10 000 элементов без переполнения стека | VERIFIED | юнит-тест `deep_nesting_no_overflow`; дамп и подсчёты итеративные |
| Восстановление после ошибок разбора без паники (HTML-ошибки нормативно-восстановимы) | VERIFIED | `PARSE_ERRORS=N` + `PARSE_ERROR_KINDS` (до 8 различных kinds) в машинном отчёте |
| Чистота модулей движка: `html.rs`/`dom.rs` без сетевого и файлового I/O | VERIFIED | `STAGE3_STRUCTURE=PASS` (машинная проверка `tests/architecture.py`) |

## Что НЕ реализовано (честно, анти-фасад из МАСТЕР-ПРОМПТА §23)

| Компонент | Статус |
|---|---|
| HTTPS/TLS | NOT_IMPLEMENTED (план: вендоренный rustls по `docs/DEPENDENCY_POLICY.md`) |
| HTTP/2, HTTP/3 (QUIC) | NOT_IMPLEMENTED |
| Редиректы (следование 3xx) | NOT_IMPLEMENTED — статус только отображается |
| Настраиваемый DNS / DoH | NOT_IMPLEMENTED — используется системный резолвер ОС |
| Проверка scope в Policy | NOT_IMPLEMENTED — scope записывается в capability, политикой пока не проверяется |
| CSS-парсер / стили / layout / рендеринг / JS | NOT_IMPLEMENTED — этапы 4+ по МАСТЕР-ПРОМПТУ |
| Foster parenting и режимы вставки таблиц | NOT_IMPLEMENTED — `<table><tr>` парсится без подразумеваемого `<tbody>` (документированное отклонение) |
| Алгоритм усыновления (adoption agency) и активные форматирующие элементы | NOT_IMPLEMENTED — неправильно вложенные `b/i/em` остаются буквально вложенными |
| Иностранный контент (SVG/MathML) | NOT_IMPLEMENTED — `<svg>`/`<math>` парсятся как обычные HTML-элементы |
| Специальные режимы `<template>` | NOT_IMPLEMENTED |
| Полная таблица именованных символьных ссылок (2231 запись) и legacy-набор без `;` | NOT_IMPLEMENTED — реализовано 80 общих имён с обязательным `;`; неизвестные `&name;` остаются литералом (это и есть поведение спецификации для неизвестных имён) |
| public/system-идентификаторы DOCTYPE, вычисление quirks mode | NOT_IMPLEMENTED — захватывается только имя |
| Песочница ОС (seccomp/pledge/AppContainer) | NOT_IMPLEMENTED — этап 1 остаётся прототипом policy/broker в одном процессе |
| obs-fold в заголовках, gzip/br-сжатие | NOT_IMPLEMENTED — такие ответы отклоняются как BAD_HEADER |

## Запуск

Сборка (после установки Rust; нулевые внешние зависимости):

```text
cargo build --locked
```

Самотест (без сети, без серверов):

```text
./target/debug/neutral-browser --self-test
```

Ожидаемый вывод:

```text
SELF_TEST=PASS
WEB_TO_OS=DENY
WEB_NETWORK=ALLOW
NON_WEB_NETWORK=DENY
URL_PARSE=PASS
HTTP_PARSE=PASS
CHUNKED_DECODE=PASS
SHA256_VECTORS=PASS
HTML_TOKENIZER=PASS
DOM_TREE=PASS
```

Структурные тесты и сквозной прогон (поднимает локальный тест-сервер и гоняет
реальный бинарник через 10 случаев):

```text
python3 tests/architecture.py
python3 tools/e2e.py
```

Юнит-тесты токенизатора и дерева:

```text
cargo test
```

Загрузка страницы и разбор в DOM:

```text
./target/debug/neutral-browser --fetch http://127.0.0.1:18080/index.html --dom
./target/debug/neutral-browser --fetch <url> --dom --dom-output <tree.txt> --output <body.bin>
./target/debug/neutral-browser --dom-file <page.html> --dom-output <tree.txt>
```

`--dom-file` читает файл, явно указанный пользователем в его собственной
командной строке: это действие самого пользователя над своим файлом, а не
возможность веб-контента (политика брокера этапа 1 не тронута — веб-контент
по-прежнему не имеет доступа к файловой системе).

Машинный отчёт DOM-блока: `DOM_NODES`, `DOM_ELEMENTS`, `DOM_TEXTS`,
`DOM_COMMENTS`, `DOM_DOCTYPE`, `DOM_DEPTH`, `PARSE_ERRORS`,
`PARSE_ERROR_KINDS`, `DOM_SAVED`, `DOM=OK`. Разбор ошибок не прерывает
никогда — восстановление нормативно для HTML.

Коды выхода: `0` OK; `2` ошибка использования; `3` схема не поддерживается
(например `https` или `file://`); `4` отказ брокера; `5` сетевая/протокольная
ошибка либо ошибка I/O.

`--subject <s>` — диагностический флаг для проверки пути отказа
(пример: `--subject untrusted-process` → `BROKER_DECISION=DENY`). Это средство
тестирования уровня CLI: пользователь — хозяин; веб-контент таких прав не имеет.

## Инварианты этапов 2–3

1. Единственный `TcpStream::connect` в кодовой базе — в `src/network.rs`
   (машинно проверяется `tests/architecture.py`).
2. `html.rs` и `dom.rs` — чистые модули: ноль сетевого и файлового I/O
   (машинно проверяется тем же скриптом).
3. Каждое соединение проходит `Policy → Broker`; запрашивающий не подсовывает
   своё решение (наследование этапа 1, не нарушено).
4. Capability (subject/resource/operation/scope/lifetime) выводится движком
   из URL, а не вводится извне.
5. Сеть трогается только командой пользователя `--fetch`: никаких фоновых
   соединений; телеметрии нет по построению — ей просто негде быть.
6. Host и path валидируются по белому списку печатного ASCII — CRLF-инъекция
   в запросную строку и заголовок Host невозможна (`BAD_HOST` / `BAD_PATH`).
7. Переключение RAWTEXT/RCDATA/PLAINTEXT выполняет конструктор дерева через
   `TokControl` — как в спецификации; самозакрывающийся `<script/>` не
   переключает токенизатор.

## Изменения относительно stage2 (0.3.0)

- Новые модули: `src/html.rs` (токенизатор) и `src/dom.rs` (арена DOM,
  конструктор дерева, канонический дамп, счётчики).
- `main.rs`: флаги `--dom` (разбор тела ответа), `--dom-file` (разбор
  пользовательского файла), `--dom-output` (канонический дамп дерева в файл);
  блок самотеста этапа 3 (`HTML_TOKENIZER=PASS`, `DOM_TREE=PASS`).
- Семантика этапов 1–2 не менялась: policy/broker/url/http/network/sha256
  перенесены дословно, старые проверки самотеста и e2e остались зелёными.
- `tests/architecture.py` и `tools/e2e.py` расширены проверками этапа 3
  (`STAGE3_STRUCTURE=PASS`, случаи T7–T10).

## Структурные алгоритмы (перенесены из K-Sidebar)

- Этапный цикл: `ЦЕЛЬ → АРХИТЕКТУРА → КОД → СБОРКА → ЗАПУСК → ТЕСТ → ВЫВОД → ВЕРДИКТ`.
- Честные статусы: `VERIFIED / PARTIAL / NOT_IMPLEMENTED`.
- Замкнутый контур качества: код → сборка → тесты → машинный вывод → README →
  архив → SHA-256 → worklog.
