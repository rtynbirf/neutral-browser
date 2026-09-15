# Neutral Browser — НЕЙТРАЛЬНЫЙ БРАУЗЕР ПОЛЬЗОВАТЕЛЯ

Инструмент пользователя: `USER → BROWSER → WEB`. Без обязательного корпоративного
посредника, аккаунта, облака, телеметрии и рекламы. Спецификация — МАСТЕР-ПРОМПТ
(`docs/MASTER_PROMPT.md`). Правило проекта: **каждый статус подтверждается машинным
выводом, а не обещанием**.

Текущая версия: **0.5.0 / STAGE4_STYLE**

Цепочка этапа 4: `URL → Broker → TCP → HTTP/1.1 → тело → HTML → DOM → CSS → STYLE → машинный отчёт`

## Что уже РАБОТАЕТ (VERIFIED — машинное доказательство)

| Компонент | Статус | Доказательство |
|---|---|---|
| Capability-модель `Policy → Broker → Decision` (этап 1) | VERIFIED | `SELF_TEST=PASS`, `WEB_TO_OS=DENY`, `WEB_NETWORK=ALLOW`, `NON_WEB_NETWORK=DENY` |
| Реальная сборка Rust, ноль внешних зависимостей | VERIFIED | `cargo build` SUCCESS; `Cargo.lock` зафиксирован; `[dependencies]` пуст (проверяет `tests/architecture.py`) |
| URL-парсер (подмножество: http, IPv4/IPv6, порт, путь, query) | VERIFIED | `URL_PARSE=PASS` |
| Разбор HTTP/1.1-ответа (статус, заголовки) | VERIFIED | `HTTP_PARSE=PASS` |
| Chunked-декодер (RFC 9112) | VERIFIED | `CHUNKED_DECODE=PASS` + `E2E_T2_SHA` |
| SHA-256 (FIPS 180-4, целостность тела) | VERIFIED | `SHA256_VECTORS=PASS` (3 опубликованных вектора NIST) |
| Fetch-цепочка целиком через брокер | VERIFIED | `E2E_TEST=PASS` — случаи T1–T12 на живом локальном сервере |
| Путь отказа брокера (сокет не открывается) | VERIFIED | `E2E_T4_DENIED`, `NETWORK=CLOSED` |
| Структурный инвариант: единственный `TcpStream::connect` в `src/network.rs`, за брокером | VERIFIED | `STAGE2_STRUCTURE=PASS` |
| HTML-токенизатор (подмножество WHATWG §13.5): данные, теги, семейство атрибутов, комментарии (включая `--!>`), фиктивный комментарий, DOCTYPE (имя), RAWTEXT/RCDATA/PLAINTEXT, символьные ссылки | VERIFIED | `HTML_TOKENIZER=PASS` (самотест) + 61 юнит-тест `cargo test` |
| Числовые символьные ссылки с нормативной таблицей windows-1252 (0x80–0x9F) и правилами U+FFFD | VERIFIED | юнит-тесты: `&#128;`→€, `&#xD800;`→FFFD, `&#0;`→FFFD |
| Конструктор дерева DOM (подмножество WHATWG §13.2.6): подразумеваемые html/head/body, void-элементы, автозакрытие p/li/dd/dt, `</br>`→`<br>`, `image`→`img`, пропуск LF после pre/textarea/listing | VERIFIED | `DOM_TREE=PASS` (самотест) + юнит-тесты с точным дампом дерева |
| Канонический дамп дерева DOM (замороженный формат, итеративный) | VERIFIED | `E2E_T9_DUMP_EXACT` — побайтовое равенство на хитром документе |
| Вложенность 10 000 элементов без переполнения стека | VERIFIED | юнит-тесты; дамп/подсчёты/каскад — итеративные алгоритмы |
| Восстановление после ошибок разбора HTML без паники | VERIFIED | `PARSE_ERRORS=N` + `PARSE_ERROR_KINDS` (до 8 kinds) |
| CSS-токенизатор (подмножество CSS Syntax L3): ident/function/at-keyword/hash/string/bad-string/url/bad-url/number/percentage/dimension/delim/пунктуация, `\`-escape (hex до 6 цифр + нормативные U+FFFD-правила) | VERIFIED | `CSS_TOKENIZER=PASS` (самотест) + юнит-тесты токенизатора |
| CSS-парсер: qualified-правила, декларации с `!important` (включая `! important`), восстановление после ошибок без паники (пустое значение, мусор, вложенное правило, EOF в блоке, лишняя `}`), at-правила поглощаются и считаются | VERIFIED | `CSS_PARSE=PASS` + юнит-тесты восстановления |
| Селекторы (подмножество Selectors L3): тип, `*`, `.class` (чувствителен к регистру), `#id`, `[attr]`, `[attr="value"]` (точное), комбинаторы потомок/`>`, группировка через запятую; специфичность (a,b,c) | VERIFIED | `CSS_CASCADE=PASS` + юнит-тесты (специфичность `div p.x#a [b]` = (1,2,2)) |
| Каскад (author origin): победитель = max(important, inline, специфичность, порядок источника); инлайн-стиль бьёт обычные правила листа, `!important` листа бьёт инлайн | VERIFIED | `CSS_CASCADE=PASS` + `E2E_T11_DUMP_EXACT` (страница с 6 правилами, инлайном и `!important`) |
| Matching по DOM: class-список по пробелам, точное `[attr=value]`, право-в-лево по комбинаторам | VERIFIED | юнит-тесты `matching_descendant_child_class_split` |
| Канонический STYLE-дамп (замороженный формат `STYLE v1`, свойства по алфавиту) | VERIFIED | `E2E_T11_DUMP_EXACT`, `E2E_T13_DUMP_EXACT` — побайтовое равенство |
| Источники CSS: `<style>`-элементы DOM (`STYLE_SRC_ELEMENTS`) + файл пользователя `--css-file` (юзер-принципипал) | VERIFIED | `E2E_T11` (fetch-страница), `E2E_T13` (--css-file) |
| Чистота модулей движка: `html.rs`/`dom.rs`/`css.rs` без сетевого и файлового I/O | VERIFIED | `STAGE3_STRUCTURE=PASS`, `STAGE4_STRUCTURE=PASS` (`tests/architecture.py`) |

## Что НЕ реализовано (честно, анти-фасад из МАСТЕР-ПРОМПТА §23)

| Компонент | Статус |
|---|---|
| HTTPS/TLS | NOT_IMPLEMENTED (план: вендоренный rustls по `docs/DEPENDENCY_POLICY.md`) |
| HTTP/2, HTTP/3 (QUIC) | NOT_IMPLEMENTED |
| Редиректы (следование 3xx) | NOT_IMPLEMENTED — статус только отображается |
| Настраиваемый DNS / DoH | NOT_IMPLEMENTED — используется системный резолвер ОС |
| Проверка scope в Policy | NOT_IMPLEMENTED — scope записывается в capability, политикой пока не проверяется |
| at-правила CSS (at-rules: `@media`, `@import`, `@font-face`…) | NOT_IMPLEMENTED — поглощаются без применения, счётчик `STYLE_AT_RULES_SKIPPED`; @import не скачивается |
| Наследование и initial/inherit/unset в CSS | NOT_IMPLEMENTED — каждое свойство применяется только к совпавшему элементу |
| `<link rel=stylesheet>` / внешние таблицы стилей | NOT_IMPLEMENTED — внешний CSS не скачивается (только `<style>` и `--css-file`) |
| Продвинутые селекторы: псевдоклассы/псевдоэлементы, `+`/`~`, атрибутные операторы `~= ^= $= *= \|=`, пространства имён | NOT_IMPLEMENTED — такой селектор = ошибка разбора, правило отбрасывается (счётчик `STYLE_ERRORS`) |
| Shorthand-раскрытие, `var()`, CSS-переменные, @keyframes | NOT_IMPLEMENTED — значения хранятся как текст |
| Foster parenting и режимы вставки таблиц | NOT_IMPLEMENTED — `<table><tr>` парсится без подразумеваемого `<tbody>` (документированное отклонение) |
| Алгоритм усыновления (adoption agency) и активные форматирующие элементы | NOT_IMPLEMENTED — неправильно вложенные `b/i/em` остаются буквально вложенными |
| Иностранный контент (SVG/MathML) | NOT_IMPLEMENTED — `<svg>`/`<math>` парсятся как обычные HTML-элементы |
| Специальные режимы `<template>` | NOT_IMPLEMENTED |
| Полная таблица именованных символьных ссылок (2231 запись) и legacy-набор без `;` | NOT_IMPLEMENTED — реализовано 80 общих имён с обязательным `;` |
| public/system-идентификаторы DOCTYPE, вычисление quirks mode | NOT_IMPLEMENTED — захватывается только имя |
| Layout / рендеринг / JS | NOT_IMPLEMENTED — этапы 5+ по МАСТЕР-ПРОМПТУ |
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
CSS_TOKENIZER=PASS
CSS_PARSE=PASS
CSS_CASCADE=PASS
```

Структурные тесты и сквозной прогон (поднимает локальный тест-сервер и гоняет
реальный бинарник через 15 случаев):

```text
python3 tests/architecture.py
python3 tools/e2e.py
```

Юнит-тесты токенизаторов, дерева и каскада:

```text
cargo test
```

Загрузка страницы, разбор в DOM и каскад стилей:

```text
./target/debug/neutral-browser --fetch http://127.0.0.1:18080/index.html --dom
./target/debug/neutral-browser --fetch <url> --dom --style --style-output <style.txt>
./target/debug/neutral-browser --dom-file <page.html> --style --css-file <user.css> --style-output <style.txt>
```

`--dom-file` и `--css-file` читают файлы, явно указанные пользователем в его
собственной командной строке: это действие самого пользователя над своими
файлами, а не возможность веб-контента (политика брокера этапа 1 не тронута —
веб-контент по-прежнему не имеет доступа к файловой системе).

Машинный отчёт DOM-блока: `DOM_NODES`, `DOM_ELEMENTS`, `DOM_TEXTS`,
`DOM_COMMENTS`, `DOM_DOCTYPE`, `DOM_DEPTH`, `PARSE_ERRORS`,
`PARSE_ERROR_KINDS`, `DOM_SAVED`, `DOM=OK`.

Машинный отчёт STYLE-блока: `STYLE_SRC_ELEMENTS` (число `<style>`-элементов),
`STYLE_RULES`, `STYLE_DECLS` (выжившие декларации), `STYLE_AT_RULES_SKIPPED`,
`STYLE_ERRORS` (CSS-ошибки не прерывают работу — восстановление нормативно для
CSS; прерывают только ошибки I/O файла), `STYLE_ELEMENTS_STYLED`,
`STYLE_PROPS`, `STYLE_SAVED`, `STYLE=OK`.

Коды выхода: `0` OK; `2` ошибка использования; `3` схема не поддерживается
(например `https` или `file://`); `4` отказ брокера; `5` сетевая/протокольная
ошибка либо ошибка I/O.

`--subject <s>` — диагностический флаг для проверки пути отказа
(пример: `--subject untrusted-process` → `BROKER_DECISION=DENY`). Это средство
тестирования уровня CLI: пользователь — хозяин; веб-контент таких прав не имеет.

## Инварианты этапов 2–4

1. Единственный `TcpStream::connect` в кодовой базе — в `src/network.rs`
   (машинно проверяется `tests/architecture.py`).
2. `html.rs`, `dom.rs`, `css.rs` — чистые модули: ноль сетевого и файлового I/O
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
8. Каскадный порядок (author origin): inline important > sheet important >
   inline normal > sheet normal, внутри уровня — специфичность, затем порядок
   источника; свойства в дампе отсортированы по имени (детерминизм).
9. Имена CSS-свойств и типов селекторов ASCII-приводятся к нижнему регистру;
   имена классов, id и значения атрибутов регистрозависимы.
10. Движок ни при каком входе не паникует: токенизатор, парсер, matching и
    каскад — итеративные алгоритмы с восстановлением после ошибок.

## Изменения относительно stage3 (0.4.0)

- Новый модуль `src/css.rs`: CSS-токенизатор (подмножество CSS Syntax L3),
  парсер правил/деклараций/селекторов (подмножество Selectors L3), каскад
  (author origin) и канонический STYLE-дамп.
- `main.rs`: `VERSION 0.5.0`, `STATUS=STAGE4_STYLE`,
  `CHAIN=...DOM->CSS->STYLE`; флаги `--style` (каскад по DOM),
  `--css-file <path>` (CSS пользователя, юзер-принципипал),
  `--style-output <file>` (дамп `STYLE v1`); блок самотеста этапа 4
  (`CSS_TOKENIZER=PASS`, `CSS_PARSE=PASS`, `CSS_CASCADE=PASS`).
- Семантика этапов 1–3 не менялась: policy/broker/url/http/network/sha256/
  html/dom перенесены дословно, старые проверки самотеста и e2e зелёные.
- `tests/architecture.py` и `tools/e2e.py` расширены проверками этапа 4
  (`STAGE4_STRUCTURE=PASS`, случаи T11–T13b; всего 15 e2e-случаев).

## Структурные алгоритмы (перенесены из K-Sidebar)

- Этапный цикл: `ЦЕЛЬ → АРХИТЕКТУРА → КОД → СБОРКА → ЗАПУСК → ТЕСТ → ВЫВОД → ВЕРДИКТ`.
- Честные статусы: `VERIFIED / PARTIAL / NOT_IMPLEMENTED`.
- Замкнутый контур качества: код → сборка → тесты → машинный вывод → README →
  архив → SHA-256 → worklog.
