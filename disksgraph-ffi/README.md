# Stabby API с ограниченными apply jobs (C7)

`cargo build -p disksgraph-ffi --offline` собирает `target/debug/libdisksgraph_ffi.so`.
Это незавершённый этап C7; финальное packaging-имя `libdisksgraph.so` ещё не настроено.
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
`{"protocol_version":1,"result":{"context":1,"abi_version":1,"mode":"background_jobs","live_host":false}}`.
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
| `apply_start`, `job_poll`, `job_close`, `job_cancel` | Фоновая попытка, passive polling, явное освобождение и отказ cancellation |
| `incident_inspect` | Проверка checkpoint через штатный runtime lock |

Reply содержит `protocol_version` и ровно одно поле `result` или `error`.
Ошибка имеет `code` и при необходимости `detail`; application error вложена
как `{"code":"application","detail":{...}}`. Apply возвращает snapshot также
при сохранённой ошибке coordinator: проверяйте `verified`, а не только наличие
`result`. Events доступны после blocking call, с exclusive `since` cursor и
обычным gap handling через snapshot. Freeze возвращает документ; его импорт
не восстанавливает source map живого `FrozenPlan` или execution permission.

`apply_start` принимает `{"plan":"<plan-handle>"}` и возвращает `{"job":1}`.
Это подтверждение очереди, до validation/readiness/admission; выполнение идёт на
том же owner thread через `DisksGraph::apply_plan_with_updates` и единственный
runtime coordinator. Прямой синхронный `wire::Protocol::call` не владеет worker и
возвращает `unsupported_operation` для job lifecycle; Rust embedding может
использовать тот же observed apply. ABI signatures и JSON version остаются v1,
новые методы добавлены без изменения прежних.

`job_poll` принимает `{"job":1,"since":0}`. Result содержит `job`, `finished`,
`snapshot`, `events` (обычный EventsPage) и `error`. Пока job в очереди, snapshot
равен null. Далее это целостный последний snapshot на границе фазы runtime;
новых heartbeat/probe во время блокирующего native вызова нет. Live snapshots
имеют `finished=false`; последняя публикация содержит finished snapshot.
Job `finished=true` устанавливается только после возврата owner. Snapshot и cursor
публикуются под одним коротким lock, polling не ждёт provider/native execution.
При `events.gap=true` клиент заменяет своё состояние полным snapshot и продолжает
с его `event_sequence`, без прибавления единицы. Future cursor отклоняется.

Завершение job само по себе не означает verified success. Ошибка до создания run
публикуется в job `error`, snapshot остаётся null. Coordinator failure сохраняется
в `snapshot.error`, snapshot/run/events доступны как у blocking apply. Panic даёт
terminal job error `panicked`, сохраняет последнее passive evidence и останавливает
owner; это не завершённая verification. Клиент проверяет job error, snapshot error
и `snapshot.verified`. Никакого автоматического повторного start нет.

На context допускается **один job**, включая завершённый. Во время исполнения
обычные команды, повторный start и `job_close` возвращают `busy`; `job_poll` и
`job_cancel` не ставятся в очередь worker. По завершении можно читать обычные
run methods. Перед следующим start требуется `job_close` с `{"job":1}`; до него
повторный start возвращает `resource_limit`. Job IDs не переиспользуются и чужой
context возвращает `unknown_job`. Освобождение mailbox не закрывает retained run:
для этого отдельно вызывается `run_close`. Сохранённого job/event journal нет.

`job_cancel` явно возвращает `unsupported_operation` и не меняет попытку.
`disksgraph_close` удаляет context handle и ждёт завершения owner, включая активный
job, затем освобождает registrations. Timeout/прерывание native вызова не реализованы:
close может ждать столько же, сколько этот вызов. Checkpoint не удаляется ни при
close context, ни при job close.

Одновременно допускаются 32 context owners, включая opening/closing. Open config
ограничен 64 KiB, application request — 16 MiB до JSON parsing. Каждый context
имеет один owner thread; параллельный call того же context получает `busy`,
без неограниченной очереди. Разные context не держат общий registry lock во время
probe/runtime. Rust panics перехватываются на границе; после panic worker
не продолжает обработку. Panic abort, OOM и native process faults не становятся JSON.

Consumer обязан закрыть все contexts и освободить все возвращённые StableString
**до выгрузки библиотеки**, завершив все конкурентные exported calls: allocator callbacks находятся в `.so`. Close ждёт
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
reads, background start/poll, exact final snapshot/events parity, foreign/stale job
handles, release и pre-run failure без native writes. Детерминированный unit test
с channel-blocked synthetic owner отдельно проверяет polling при занятом worker,
Busy, отказ cancellation и join-before-close-return; искусственная задержка не
включена в production config. Недоступные host subsystems
могут давать diagnostics: тест не доказывает completeness наблюдения.

C7 остаётся открыт: wire rebase review/accept, incident review/acknowledgment и
отдельный credential channel ещё не реализованы. Cancellation явно отклоняется. Положительный native write workflow через
динамический consumer и сборка на MSRV требуют отдельной приёмки. Production
write policies относятся к C8. Disks preview остаётся отключённым.
