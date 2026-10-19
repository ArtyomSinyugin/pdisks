# Начальный синхронный Stabby API (C7)

`cargo build -p disksgraph-ffi --offline` собирает `target/debug/libdisksgraph_ffi.so`.
Это первая часть C7; финальное packaging-имя `libdisksgraph.so` ещё не настроено.
Все методы делегируются `disksgraph`; runtime остаётся единственным владельцем
execution/admission/checkpoint. Конфигурация native reads общая с `dg-cli`.
Native writes в этом конструкторе отключены: capability/authorization возвращают
Unknown, dispatcher не выполняет mutations.

Экспорты используют Stabby **72.1.16**, закреплённый в workspace. Consumer обязан
получать функции через `StabbyLibrary::get_stabbied`, который проверяет signature
report. Пример границы загрузки: [tests/support/dylib.rs](tests/support/dylib.rs).
Обычный поиск символа по имени пропускает эту проверку. Crate использует edition
2021 из-за export attributes этой версии Stabby. Workspace MSRV — Rust 1.88;
фактическая сборка на 1.88 пока не проверена, текущие проверки выполнены на 1.98.1.

```rust,ignore
// StableString = stabby::string::String; все строки передаются во владение.
extern "C" fn disksgraph_api_version() -> u32;
extern "C" fn disksgraph_open(config: StableString) -> StableString;
extern "C" fn disksgraph_call(context: u64, request: StableString) -> StableString;
extern "C" fn disksgraph_close(context: u64) -> StableString;
```

ABI version и application protocol version сейчас равны 1 и проверяются отдельно.
Open принимает строгий JSON, например:

```json
{"abi_version":1,"protocol_version":1,"root":"/existing/private/directory","manifest":null}
```

Root должен уже существовать и соответствовать private-root checks runtime.
`manifest: null` (или отсутствие поля) означает работу только с сохранёнными
документами. Для fresh reads указывается путь к полному native registration
[manifest](../storage-probe/tests/providers.live.example.json). Этот файл выбирает
загружаемые библиотеки и является доверенной конфигурацией host.
Отсутствие manifest не превращается в пустое успешное наблюдение:
live-методы возвращают `live_unavailable`.

Успешный open возвращает
`{"protocol_version":1,"result":{"context":1,"abi_version":1,"mode":"synchronous","live_host":false}}`.
Context — process-local integer; session/plan/run handles принадлежат одному facade.
Закрытые context IDs не переиспользуются. Для вызова метода:

```json
{"protocol_version":1,"request":{"method":"session_preview","params":{"session":"<session-id>"}}}
```

Методы и типы параметров определены в [Request](../disksgraph/src/wire.rs).
Для методов без параметров передаётся `"params": {}`. Неизвестные поля
отклоняются; future protocol version отклоняется до интерпретации метода.

| Методы | Назначение |
| --- | --- |
| `probe`, `session_create`, `capabilities` | Fresh evidence и advisory capabilities |
| `session_import`, `session_export`, `session_close` | Исторические draft inputs и lifetime |
| `session_edit`, `session_undo`, `session_redo` | Редактирование с expected revision |
| `session_preview`, `session_freeze` | Forecast и frozen historical document |
| `plan_import`, `plan_export`, `plan_close` | Immutable historical plans |
| `apply_blocking`, `run_snapshot`, `run_events`, `run_close` | Синхронная попытка и retained evidence |
| `incident_inspect` | Проверка checkpoint через штатный runtime lock |

Reply содержит `protocol_version` и ровно одно поле `result` или `error`.
Ошибка имеет `code` и при необходимости `detail`; application error вложена
как `{"code":"application","detail":{...}}`. Apply возвращает snapshot также
при сохранённой ошибке coordinator: проверяйте `verified`, а не только наличие
`result`. Events доступны после blocking call, с exclusive `since` cursor и
обычным gap handling через snapshot. Freeze возвращает документ; его импорт
не восстанавливает source map живого `FrozenPlan` или execution permission.

Одновременно допускаются 32 context owners, включая opening/closing. Open config
ограничен 64 KiB, application request — 16 MiB до JSON parsing. Каждый context
имеет один owner thread; параллельный call того же context получает `busy`,
без неограниченной очереди. Разные context не держат общий registry lock во время
probe/runtime. Rust panics перехватываются на границе; после panic worker
не продолжает обработку. Panic abort, OOM и native process faults не становятся JSON.

Consumer обязан закрыть все contexts и освободить все возвращённые StableString
**до выгрузки библиотеки**: их allocator callbacks находятся в `.so`. Close ждёт
текущий вызов и join owner thread, освобождает facade/native registrations и
не удаляет аварийный checkpoint. Автоматического retry или восстановления нет.

Проверки consumer загружают реальную `.so` через checked imports, отвергают
неверную signature/version, сравнивают edit/preview/freeze с прямым Rust API,
проверяют чужие handles и сохранение checkpoint при close:

```sh
cargo test -p disksgraph-ffi --offline
DISKSGRAPH_NATIVE_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p disksgraph-ffi --test checked_consumer --offline native_provider_calls -- --ignored
```

Вторая проверка требует установленных native libraries; она проверяет повторные
reads и retained unverified run без native writes. Недоступные host subsystems
могут давать diagnostics: тест не доказывает completeness наблюдения.

C7 остаётся открыт: async apply jobs и polling во время исполнения, wire rebase
review/accept, incident review/acknowledgment, cancellation contract и отдельный
credential channel ещё не реализованы. Положительный native write workflow через
динамический consumer и сборка на MSRV требуют отдельной приёмки. Production
write policies относятся к C8. Disks preview остаётся отключённым.
