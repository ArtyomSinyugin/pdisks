# Stabby API, native host policy и C JSON bridge

`cargo build -p disksgraph-ffi --offline` собирает `target/debug/libdisksgraph_ffi.so`.
`scripts/stage-package.sh /path/to/new-bundle` укладывает release ABI как
`lib/libdisksgraph.so`, CLI и standalone consumer SDK в новый локальный bundle.
Cargo-имя сохраняется, чтобы не конфликтовать с Rust facade crate `disksgraph`.
Это завершение функциональной/compatibility приёмки C7 для текущего API;
production deployment и write/safety policies относятся к C8.
Все методы делегируются `disksgraph`; runtime остаётся единственным владельцем
execution/admission/checkpoint. Конфигурация native reads общая с `dg-cli`.
В обычном `disksgraph_open` native writes отключены: capability/authorization возвращают
Unknown, dispatcher не выполняет mutations. Явный `disksgraph_open_with_policy`
подключает внешний host callback. Для клиентов не на Rust есть C JSON bridge и
Python SDK; [контракт embedding](../docs/embedding.md) описывает signatures,
policy schema, ownership и ограничения.

Экспорты используют Stabby **72.1.16**, закреплённый в workspace. Consumer обязан
получать функции через `StabbyLibrary::get_stabbied`, который проверяет signature
report. Пример границы загрузки: [standalone checked loader](../packaging/consumer/src/api.rs).
Обычный поиск символа по имени пропускает эту проверку. Crate использует edition
2021 из-за export attributes этой версии Stabby. Workspace MSRV — Rust **1.88.0**;
полный locked workspace и actual checked cdylib consumer проверены на официальном
Rust/Cargo 1.88.0 и текущем Rust 1.98.1. Воспроизведение:
`scripts/check-msrv.sh /path/to/rust-1.88.0/bin`. Скрипт требует exact compiler/cargo,
использует отдельный target directory и также собирает standalone consumer.
Обновления зависимостей проверяются с сохранёнными lockfiles.

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
| `session_prepare_rebase`, `session_accept_rebase`, `review_close` | Retained reviewed translation и явное освобождение |
| `plan_import`, `plan_export`, `plan_close` | Immutable historical plans |
| `apply_blocking`, `run_snapshot`, `run_events`, `run_close` | Синхронная попытка и retained evidence |
| `apply_start`, `job_poll`, `job_close`, `job_cancel` | Фоновая попытка, passive polling, явное освобождение и отказ cancellation |
| `incident_inspect`, `incident_review`, `incident_acknowledge` | Inspection и confirmed review через штатные runtime gates |

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

Pending reviews хранятся только в памяти context; их общий лимит — **64** для
rebase и incident вместе. Проверка capacity и reserve выполняются до fresh probe.
`review` — новый opaque UUID handle; он отличается от imported document UUID,
session handle и runtime confirmation token. Другой context, неверный вид review
или закрытый handle дают `unknown_review`. Переданные в reply объекты — только
данные для просмотра: запросы не принимают обратно candidate/inspection/policy.

Для rebase передаются следующие `method` / `params`:

```json
{"method":"session_prepare_rebase","params":{"session":"<session-handle>","revision":0}}
```

Result содержит `review`, `source_revision`, полный proposed `baseline`, translated
`draft`, `correlations`, planning `status` и `diagnostics`. Prepare требует native
manifest, сохраняет исходный draft/revision и использует `DisksGraph::prepare_rebase`.
Клиент показывает этот review перед явным accept:

```json
{"method":"session_accept_rebase","params":{"session":"<same-session-handle>","revision":0,"review":"<review-handle>"}}
```

Session и revision должны совпасть с retained candidate: иначе `review_mismatch`
без потребления review. Для совпавшего запроса candidate потребляется и передаётся
в `DisksGraph::accept_rebase`; engine повторно проверяет source context и текущую
revision. Изменение session после prepare даёт application `revision_conflict`;
для нового accept требуется новый review. Success возвращает новую `revision`.
Это изменение редактируемых inputs, без native writes или сохранения файла.
Закрытие session освобождает все её rebase candidates, сохраняя incident reviews.

`incident_review` с пустыми params возвращает `review`, полную `incident` inspection
и runtime challenge `token`. Если marker отсутствует, result равен
`{"review":null,"incident":null}`: token/slot не создаётся, provider не вызывается.
Malformed marker остаётся на месте и даёт application error. Подтверждение:

```json
{"method":"incident_acknowledge","params":{"review":"<review-handle>","run_id":"<shown-run-id>","token":"<shown-challenge>"}}
```

Exact run ID и challenge проверяются до fresh probe; mismatch даёт
`review_mismatch`, сохраняя review. Совпавшая попытка потребляет review даже при
последующем отказе runtime. Прежний runtime заново проверяет root inode, exact marker,
неизменность observations, независимую safety policy и marker после policy callback.
Токен подтверждает намерение; authorization, complete coverage и quiescence он
не устанавливает. **Default ABI policy возвращает Unknown**, так что даже точный
challenge не разрешает clear checkpoint. Успешный ответ имеет
`{"incident":{...},"acknowledged":true}`; в этом конструкторе он не включён.
Trusted Rust embedding может передать отдельную policy в
`Protocol::call_with_policy`; JSON entrypoint и `.so` такой настройки не принимают.

Отмена review — `review_close` с `{"review":"<review-handle>"}`. Она освобождает
только память, не принимает rebase и не удаляет marker. Context close аналогично
отбрасывает reviews после завершения worker. Неверная confirmation/schema и чужие
handles не потребляют retained review; после реальной accept/ack попытки нет
автоматического retry. Во время active job reviewed methods получают обычный `busy`.

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
  cargo test -p disksgraph-ffi --test checked_consumer --offline native_ -- --ignored --test-threads=1
```

Вторая проверка требует установленных native libraries; она проверяет повторные
reads, background start/poll, exact final snapshot/events parity, foreign/stale job
handles, release и pre-run failure без native writes. Второй native test проверяет
prepare без изменения draft, exact accept, foreign/stale reviews, отказ подделки
candidate/policy, incident refusal и неизменность checkpoint после discard/close.
Положительный incident acknowledgment проверен только на recorded synthetic host
через trusted Rust policy; native safety не подразумевается. Детерминированный unit test
с channel-blocked synthetic owner отдельно проверяет polling при занятом worker,
Busy, отказ cancellation и join-before-close-return; искусственная задержка не
включена в production config. Недоступные host subsystems
могут давать diagnostics: тест не доказывает completeness наблюдения.

Rust embedding может вызвать `transport::open_with(config, factory)`: factory
создаёт host на его worker thread, сохраняет native registrations в своей области
жизни и передаёт facade в `Worker::serve`. Это тот же protocol/job loop и coordinator.
Close join-ит всю factory, включая drop borrowed registrations после serve; factory
failure/panic и выход без serve освобождают slot. Config size/version проверяются
до вызова factory. Эта Rust API не передаётся через ABI, JSON не выбирает factory.
Обычный `disksgraph_open` по-прежнему создаёт только ReadOnlyPolicy/DisabledDispatcher.

Для положительной native write приёмки используется **отдельная тестовая библиотека**
`target/debug/examples/libc7_vm_library.so` и checked-import consumer `c7_vm`.
Она экспортирует `disksgraph_vm_*` с теми же Stabby signatures, используя общий
transport и [guarded guest host](../dg-cli/examples/support/guest.rs) Rust/CLI routes.
Эти symbols отсутствуют в production `.so`. VM factory дополнительно проверяет
специальный kernel argument, единственный private 512 MiB SCSI disk с exact WWN,
отсутствие других физических devices/network и fixed guest root/manifest.
Никакого флага для включения записи в production config нет.

```sh
python3 storage-runtime/tests/vm/run_c4.py --rescue-iso /path/to/rescue.iso --interface ffi
python3 storage-runtime/tests/vm/run_c4.py --rescue-iso /path/to/rescue.iso --interface ffi --case mount-target-change
cargo build -p disksgraph-ffi --example c7_vm_library --offline
cargo test -p disksgraph-ffi --test checked_consumer --offline vm_library_refuses -- --ignored
```

Guest consumer создаёт/edit/preview/freeze session через actual checked `.so`
imports, сравнивает forecast/frozen inputs с прямым offline Rust API, затем отдельно
пишет reviewed digest/LV scope в фиксированный private guest-owned host config
`/run/c7-review.json`. Приложение не получает approval из plan JSON. Первый admission
привязывает review к одному run; тот же host проверяет exact routes и destructive LV.
Этот механизм относится только к disposable guest, не production policy.

Success требует live snapshots во время job, полного exclusive event replay,
exact final snapshot/events parity с retained facade run, четырёх Verified,
независимого final probe и записи/чтения proof-файла на ext4. После close root
содержит только `execution.lock`. Imported historical plan не присваивает source map.
Failure изменяет Mount target после arm: три Verified, Mount OutcomeUnknown и
checkpoint с тремя fingerprints. Повторный apply отклоняется до пятого native call,
exact incident confirmation не очищает marker, original run остаётся inspectable.
После close/reopen старый run не восстанавливается; marker блокирует новый apply,
в новом context ни одного native dispatch. Bytes checkpoint остаются прежними.
Подробности и ограничения — в [VM runner docs](../storage-runtime/tests/vm/README.md).

В bundle есть самостоятельный Cargo workspace `consumer/` с закреплённым Stabby
72.1.16, libloading 0.9.0 и собственным lockfile. Он не зависит от facade/runtime
и не требует исходного monorepo. Программа принимает explicit library path, existing
private root и regular draft JSON до 16 MiB, возвращает preview и закрывает context
до unload. Это пример stable owned-string checked imports, не plain C header:
`stabby::string::String` нельзя заменять C string или обычным Rust String.

```sh
CARGO_TARGET_DIR=/tmp/disksgraph-client cargo build --manifest-path /path/to/bundle/consumer/Cargo.toml --locked --offline
/tmp/disksgraph-client/debug/disksgraph-consumer-example /path/to/bundle/lib/libdisksgraph.so /private/root /path/to/draft.json
python3 scripts/check-package.py /path/to/bundle /tmp/disksgraph-client/debug/disksgraph-consumer-example
```

Последняя команда выполняется из исходного checkout: проверяет checksums и exact
file set, production exports без VM symbols, ABI/CLI preview parity на записанном
fixture и сохранение synthetic incident marker при preview/close. Проверены все
четыре сочетания compiler library/client **1.88.0 × 1.98.1**, включая client,
собранный из перемещённого bundle. Это evidence для этих pinned signatures/toolchains,
не обещание совместимости произвольных будущих compiler/dependency combinations.
Staging не устанавливает систему и не публикует release; детали в
[packaging README](../packaging/README.md).

Критерий C7 для текущего API выполнен: checked consumer, native success/failure
с guarded test host, actual MSRV и public bundle filename проверены. C8 production
write/incident policies, system packaging и fault injection ещё не завершены.
Отдельный credential channel не реализован, LUKS writes не включены; cancellation
явно отклоняется. Disks preview остаётся отключённым.
