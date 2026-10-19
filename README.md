# disksgraph

`disksgraph` — будущий инструмент для работы с дисковой подсистемой Linux. Проект предназначен для повторного использования в инсталляторах и обычных системных утилитах.

Сейчас реализован безопасный read-only контур: обнаружение доступных провайдеров, чтение данных из Linux, преобразование результатов в каноническую
модель и сборка единого снимка состояния. Библиотечный `storage-engine` композирует
типизированные фрагменты в план, проверяет переходы и строит условный прогноз.
Обычная конфигурация остаётся read-only. Узкая native LVM/ext4/Host mount цепочка
доступна через явно настроенный runtime и проверена на одноразовых VM-дисках.

## Текущая реализованная архитектура

Workspace разделён на шесть крейтов; прикладной вход — `disksgraph`, исполнение
принадлежит `storage-runtime`. Поток данных во время probe выглядит так:

```text
native-библиотеки / результаты системных утилит
                       │
                       ▼
              storage-provider
 логический provider → technology backend
                       │
                       ▼
                storage-probe
       парсинг, адаптеры и сборка наблюдений
                       │
                       ▼
                 storage-core
канонический граф внутри CurrentState и DesiredState
```

`storage-core` не зависит от способов получения данных. `storage-provider`
разделяет семантику logical providers, конкретные system backends и способ
подключения provider-а. `storage-probe` вызывает logical providers и собирает
их результаты в `CurrentState`.

### `storage-core`

Крейт содержит сериализуемую доменную модель хранилища:

- `NodeGraph` хранит узлы, зависимости владения и отдельные не владеющие отношения;
- `NodeKind` описывает диски, разделы, файловые системы, LUKS, LVM, MD RAID, Btrfs, ZFS, NVMe, device-mapper и другие сущности;
- `CurrentState` объединяет граф, наблюдаемые монтирования, сведения о среде и структурированные диагностические сообщения;
- `DesiredState` задаёт пользовательскую цель и явные операции для проверки покрытия плана; `DesiredState::from_current` создаёт независимое начальное состояние из наблюдаемого графа и runtime mounts;
- типы размеров, геометрии, RAID и внешних идентификаторов не позволяют терять важную семантику при переходе между провайдерами.

Граф различает два класса связей. `Dependency` выражает состав и владение — например, раздел внутри диска или PV внутри VG. `Relation` описывает связь без владения — например, snapshot или подключение NVMe-контроллера. Точки монтирования хранятся отдельно от графа, потому что один источник может быть смонтирован в нескольких контекстах.

`NodeGraph` сохраняет прежний JSON-формат (`nodes` — объект UUID → node),
но импорт отклоняет повторные идентификаторы и рёбра, отсутствующие endpoints,
циклы зависимостей, конфликтующие роли и несогласованные offsets разделов.
`insert_new` создаёт только новые узлы, `replace_existing` заменяет только существующие.
При сборке пакета допустимы временно незавершённые связи; `validate_structure`
проверяет завершённый граф. Engine вызывает эту проверку для входных графов
и перед публикацией атомарного эффекта. Missing placeholders, неизвестные типы,
перекрывающиеся разделы и циклы не владеющих отношений остаются представимыми.

### `storage-provider`

Крейт содержит три отдельные границы:

- logical providers владеют typed actions, validation и переводом результатов в модель;
- technology backends обращаются к `libcryptsetup`, `libfdisk`, `libbd_lvm` и другим системным API;
- provider connections доставляют запросы встроенному provider-у, будущему Stabby plugin или отдельному процессу.

`BackendRegistry` читает проектный JSON manifest, проверяет абсолютные пути и обязательные символы и регистрирует только доступные системные backend-ы.

Все вызовы C API находятся в узком модуле `dylib`; остальной workspace запрещает `unsafe`. Загруженные native-библиотеки сохраняются до завершения процесса: GLib/GObject и некоторые другие библиотеки регистрируют глобальное состояние, которое нельзя безопасно отменить через `dlclose`.

Реализованы адаптеры для libudev, libfdisk, libblkid, libmount, device-mapper, libcryptsetup, libblockdev, LVM, MD RAID, Btrfs, ZFS и NVMe. Формат manifest и
правила пакетной интеграции описаны в [`storage-provider/README.md`](storage-provider/README.md).

### `storage-probe`

Крейт реализует read-only сбор данных двумя взаимодополняющими слоями:

- `parse` преобразует машинные ответы `lsblk`, LVM, `mdstat`, `findmnt`, Btrfs и sysfs в промежуточные типы;
- Linux-адаптеры вызывают logical providers и строят части канонического графа;
- каждый `StateProvider` объявляет стабильный идентификатор и единственную зону ответственности;
- `assemble::probe` объединяет независимые наблюдения в `CurrentState`, не подменяет один logical provider другим и превращает конфликты или ошибки провайдеров в диагностику;
- `ProbeSnapshot` публикует неизменяемое состояние вместе с номером поколения.

Составные адаптеры образуют последовательные уровни: базовые блочные устройства, device-mapper, локальные технологии хранения и полный системный снимок с
монтированиями, Btrfs, ZFS и NVMe.

### `storage-engine`

Крейт композирует `PlanFragment` в неизменяемый DAG `ExecutionPlan` и проигрывает
доверенные контракты от фактического `CurrentState`. Он проверяет промежуточные
resize-переходы, общий расход ёмкости, доступность writable endpoint, порядок
монтирований, покрытие явных намерений и заданную защищённую область.
`Unknown` остаётся непроверенным обязательством, а устаревшие наблюдения
отмечаются отдельно от новых измерений.

Симулятор последовательно применяет эффекты к локальному `DesiredState` и возвращает
его в `SimulationResult.desired`. Composer и replay используют общий интерпретатор
из `effects.rs`; проверки предусловий находятся в `prerequisites.rs`. Внутренний
`SimulationEvidence` хранит только временные гарантии, происхождение значений и
учёт ёмкости. У него нет собственного графа или списка будущих монтирований.
Неудачное действие сохраняет и конфигурацию, и evidence без частичных изменений.

`MountSpec.runtime` и `MountSpec.persistent` задают независимые аспекты цели.
Начальное состояние копирует наблюдаемые runtime mounts; постоянная конфигурация
остаётся неизвестной до соответствующих эффектов. Принадлежность записи списку
определяет аспект цели. `MountEntry` описывает источник, target, options и context;
у записи нет флага persistence. В контрактах постоянную настройку выбирает
`MountAction::Persist`, активное монтирование — `MountAction::Mount`.
`SimulationResult.evidence` содержит invalidations исторических `NodeFacts` и
перечень persistent targets, определённых планом. Для отдельного сравнения служит
`compare_goal(&result.desired, &result.evidence, &goal)`.

Обычная симуляция возвращает пустые intents. Goal-aware симуляция сохраняет
исходные `goal.intents`, независимо проверяя их покрытие действиями. Boot остаётся
`Unmanaged`, пока нет поддерживаемых эффектов настройки загрузки; запрос boot
остаётся обязательством. Условный `DesiredState` сам по себе не разрешает запись:
сохраняются `SimulationReport::is_complete`, obligations и отдельные live-проверки.

`PlanDocument` сохраняет цель, политику защиты и исторический baseline в
версионированном каноническом JSON. Импорт повторно проверяет DAG, ресурсы,
контракты и цель в прогнозных состояниях перед каждым действием.

`ExecutionRun` хранит отдельные статусы попытки и события прогресса. Допуск требует
полного live-отчёта, зависимые действия ждут `Verified`, а конечная цель проверяется
отдельно по новому наблюдению. `storage-runtime` реализует live admission, checkpoint
и typed dispatch для поддержанного native recipe. История выполнения на диск не записывается;
универсальный resolver всех desired-diff также не реализован.

## Rust API C5

Крейт [`disksgraph`](disksgraph/src/lib.rs) владеет сессиями, импортированными
планами и результатами запусков. Он принимает явно настроенные host/dispatcher и
существующий private execution root; `apply_blocking`, `apply_frozen` и `apply_plan`
вызывают единственный `ExecutionCoordinator`. Сохранённый plan не содержит approval.
Trusted principal policy настраивается владельцем host отдельно от JSON и проверяет
полный `ApprovalRequest` при каждом admission. Положительной production policy нет.

`create_session` берёт baseline и capacity из одного свежего provider-pass. NodeId
для редактирования следует выбирать из `export_draft(session).baseline`, поскольку
другой probe может назначить новые transient IDs. Фасад сохраняет revision checks,
invalid/conditional preview, undo/redo и отдельно проверяемый rebase. Import draft
пересчитывает preview и не восстанавливает сохранённую историю. Freeze сохраняет
source map; импортированный `PlanDocument` его не получает. Handles принадлежат
одному экземпляру; session/plan/run collections ограничены 64 объектами каждая,
освобождаются явно через `close_session/close_plan/close_run`. Закрытие run не
удаляет аварийный checkpoint. Plan JSON ограничен 16 MiB до разбора.

`ExecutionRun` хранит максимум 1024 события, progress message — максимум 4096 bytes.
Sequence монотонен независимо от вытеснения; переполнение отклоняется без wrap и
частичной публикации admission. `events(run, since)` принимает последний прочитанный
sequence **исключительно**. Возвращённый `next_sequence` передаётся следующим `since`
без прибавления единицы. При `gap=true` клиент получает `run_snapshot`, заменяет
своё состояние и продолжает от `snapshot.execution.event_sequence`. Будущий cursor
отклоняется. Прежний `events_since()` остаётся iterator по сохранившимся событиям,
поэтому не подходит для обнаружения потерь.

Snapshot содержит все action states, actual outcomes/checks, ссылки на admission и
completion publications, последнюю actual observation и отдельный final report.
`finished` означает завершение blocking-попытки, `verified` — успешную независимую
проверку конечной цели. Даже `Ok(run_id)` требует проверки `verified`: Unknown
admission или OutcomeUnknown не являются успехом. Ошибка coordinator сохраняет
попытку и возвращает её ID в `DisksGraphError.run_id`; typed `ErrorCode` сериализуется
отдельно от текста. Фасад не возобновляет попытки автоматически и не создаёт jobs
после завершения вызывающего процесса.

`incident/review_incident/acknowledge_incident` используют тех же providers, lock,
challenge и независимую safety policy runtime. Никакого нового пути clear/retry нет.
Пример [`c4_vm.rs`](disksgraph/examples/c4_vm.rs) импортирует только `disksgraph` для
публичного workflow и явной host-конфигурации. Существующий [VM runner](storage-runtime/tests/vm/README.md)
проверяет через фасад реальную четырёхшаговую цепочку, fresh final probe, events,
source map и отказ после arm. Начальный CLI C6 описан ниже; асинхронные ABI jobs и
Stabby (C7) пока не реализованы. Disks preview остаётся отключённым.

## CLI C6 — сохранённые черновики и foreground apply

`cargo build -p dg-cli --offline` собирает `target/debug/dg-cli`. Исполняемый файл
использует только `disksgraph`, включая явную host-конфигурацию из его exports.
Нативные reads требуют полного набора registrations из
[`providers.live.example.json`](storage-probe/tests/providers.live.example.json),
доступных библиотек и соответствующих прав. Root — заранее созданный owner-only
каталог, общий с другими cooperating callers; программа не создаёт его молча.

```sh
mkdir -m 700 /tmp/dg-session
dg-cli probe --manifest providers.json --root /tmp/dg-session
dg-cli plan new --manifest providers.json --root /tmp/dg-session --output draft.json
dg-cli plan add --root /tmp/dg-session --draft draft.json --request request.json --output draft.json
dg-cli plan preview --root /tmp/dg-session --draft draft.json
dg-cli plan freeze --root /tmp/dg-session --draft draft.json --output plan.json
dg-cli plan apply --manifest providers.json --root /tmp/dg-session --file plan.json --events jsonl
dg-cli incident show --manifest providers.json --root /tmp/dg-session
```

`request.json` — существующий serde-формат `PlanRequest`, например:

```json
{"create_lv":{"vg":"00000000-0000-7000-8000-000000000002","name":"data","size":16777216,"layout":"Linear","activate":true}}
```

VG ID выбирается из **baseline конкретного draft**, пример UUID не относится к
вашей системе. `plan add` возвращает revision, operation, created_node и status.
Семантически invalid draft сохраняется для исправления, но не может быть frozen.
`add/preview/freeze` не загружают native libraries и не обновляют historical baseline.
Файлы сохраняются заменой через соседний private tempfile, file sync, rename и
directory sync. Ошибка directory sync после rename означает, что файл уже виден,
но его crash durability не подтверждена. Вход ограничен 16 MiB; validator draft
дополнительно ограничивает его 1 MiB. Повторный freeze создаёт новый `plan_id`.

Все результаты, кроме `--help`, — один JSON на stdout. Apply возвращает `run` со
snapshot даже при ошибке coordinator после создания попытки. Exit codes: `0` —
команда завершена (для apply обязательно `verified=true`), `1` — IO/config/application
error или ошибка event writer, `2` — аргументы/JSON, `3` — завершённая, но не
верифицированная попытка. `event_output_error` не стирает фактический outcome.
Preview со status invalid — успешный запрос прогноза, а не успешное исполнение.

`--events jsonl` пишет **retained replay после blocking apply** в stderr:
первая строка содержит `type=replay`, `gap` и exclusive `next_sequence`, затем
идут отдельные `RunEvent`. Это не live progress; ring ограничен 1024 событиями,
snapshot на stdout содержит полное итоговое состояние даже после eviction.
Истории run/events между процессами и daemon нет; checkpoint инцидента сохраняется.

**C6 пока не завершён.** Production scope/authorization остаются Unknown, native
dispatcher в executable отключён. Даже manifest с Execute не включает запись.
Тесты проверяют parity preview/plan (кроме нового plan_id), одинаковый отказ
readiness, retained failures, independent empty-plan verification в synthetic host
и отдельные процессы редактирования. Реального write через CLI в VM ещё не было;
live events, REPL undo/redo, explicit rebase и incident acknowledgment впереди.
При smoke-probe в текущем host namespace получен typed `probe_failed`: mount source
отсутствует в observation graph; CLI не превращает неполную публикацию в baseline.

## Редактирование плана без доступа к дискам

`storage_engine::editing::PlanSession` хранит неизменяемый `HistoricalBaseline`
и редактируемый `PlanDraft`. Каждая команда `edit(expected_revision, ...)`
пересобирает DAG и рассчитывает прогноз общим engine. Стабильные `OperationId`
и planned `NodeId` сохраняются при изменении параметров и undo/redo. Удаление
создателя требует явного списка зависимых запросов из `removal_impact()`;
независимые операции, использующие общую VG, сохраняются.

Рецепты поддерживают точное создание раздела, LUKS format/open, linear LV, ext4,
resize LV/ext, удаление раздела/LV, закрытие mapping, runtime/persistent mounts,
метку ФС и read-only Check. `add_lv()` по умолчанию выбирает linear и активацию;
`set_created_lv_size()` меняет исходный CreateLv, не добавляя ResizeLv.
Неподдержанные запросы остаются в черновике с диагностикой. Ошибочная команда
и устаревшая revision оставляют сессию без изменений.

Три документа имеют разное назначение:

- `preview_document()` выдаёт JSON текущего результата: `Invalid`, `Blocked`,
  `Conditional` или `Complete`, проверки, ограничения и связи actions с операциями.
  При `Invalid` текущего `desired` нет; вывод не является форматом импорта плана.
- `draft_document()` сохраняет запросы и baseline. `DraftDocument::from_json()`
  проверяет версии, идентификаторы и ссылки; `PlanSession::from_document()`
  пересчитывает preview. Текущая схема запросов — 5 (с удалением раздела/LV и close mapping);
  схемы 1–4 читаются с прежними каталогами, после открытия сессия экспортирует схему 5.
  Структурно корректный черновик с ошибкой ёмкости можно
  сохранить и продолжить редактировать. Пределы: 1 МиБ JSON, 256 операций и
  стандартный предел глубины serde_json. История не сохраняется; в сессии
  по умолчанию до 64 снимков на каждом стеке, предел задаёт `set_history_limit()`.
- `freeze(expected_revision)` возвращает `FrozenPlan`: `PlanDocument` вместе
  с source map выбранной revision. Повторно экспортируйте `document()` этого
  экземпляра, чтобы сохранить plan_id и канонический JSON. Baseline сохраняет
  исходный ID. Условный прогноз допускает офлайн-фиксацию; `Invalid` и `Blocked`
  отклоняются. Ни сохранение, ни freeze не выполняют live-допуск или запись.

Полный сценарий на искусственной VG с 64 ГиБ свободного места:

```sh
cargo run -p storage-engine --example edit_plan
```

[Пример](storage-engine/examples/edit_plan.rs) проверяет LV 20 → 30 ГиБ,
создание ФС и два вида mount, сохранение/открытие, freeze, ошибочный запрос
100 ГиБ, undo/redo и явную отмену зависимых запросов. Root, probe и native backend
ему не нужны. Незавершённый disks preview остаётся выключенным.

Создание раздела на существующей таблице и дальнейшие ext4/mount:

```sh
cargo run -p storage-engine --example partition_plan
```

[Пример](storage-engine/examples/partition_plan.rs) использует искусственный диск
64 МиБ с занятым первым разделом. `PlanRequest::CreatePartition` задаёт номер,
смещение и размер в байтах, роль и native attributes; автоматического размещения нет.
`set_created_partition_extent()` меняет исходный Create-запрос с сохранением IDs.
Проверяются нулевой размер, переполнение, известная ёмкость, занятый номер и
пересечения, включая ранее запланированные разделы через общий replay.
Свободное место, границы метаданных таблицы и выравнивание всё ещё требуют
backend evidence (`ExtentAvailable`); прогноз остаётся условным. Пример также
проверяет сохранение/открытие, freeze, ошибочное пересечение, undo и отмену
зависимых запросов. Rebase таблиц разделов пока явно отклоняется без поддержанной
стабильной identity. Создание новой таблицы и записи на диск этот рецепт не добавляет.

Подготовка зашифрованного стека LV → LUKS → mapping → ext4 → mount:

```sh
cargo run -p storage-engine --example luks_plan
```

[Пример](storage-engine/examples/luks_plan.rs) задаёт два отдельных запроса:
`FormatLuks { backing, version, credential }` создаёт будущий контейнер,
`OpenLuks { container, mapping_name, credential, read_only }` — будущий mapping.
Оба получают устойчивые planned NodeId. Поддержаны LUKS1/LUKS2, а параметры
алгоритмов и ключей этот рецепт не выбирает. `CredentialRef` — UUID внешнего
хранилища секретов; пароли, ключи и keyfile paths не принимаются в запросах.
Пустая identity credential, недопустимые и занятые известным dm-crypt mapping
имена отклоняются. Проверки повторяются по общему prefix replay.

Format сохраняет требования writable, отсутствия верхних потребителей и
разрешения на уничтожение данных. Открытие на запись требует writable контейнер;
read-only mapping запрещает создание ФС. `CredentialAvailable` для Format/Open
всегда остаётся live-обязательством в симуляции и не может быть удалён из
импортированного контракта. Полезный размер контейнера и mapping после Format
остаётся неизвестным: размер LV не подменяет наблюдения о LUKS overhead.
Сохранение и freeze фиксируют только план. Пример проверяет ошибочный read-only
edit, undo и явную отмену зависимой crypto-цепочки с сохранением исходного LV.

Уже поддержанный rebase переносит запланированный crypto-стек, если его
существующий вход — однозначно сопоставленная VG/LV. Корреляция существующих
LUKS containers и mappings пока не добавлена. Разрешение credentials, проверка
реального пространства имён device-mapper и запись требуют будущего runtime;
этот пример их не выполняет.

Resize существующих LV и ext2/ext3/ext4:

```sh
cargo run -p storage-engine --example resize_plan
```

[Пример](storage-engine/examples/resize_plan.rs) берёт смонтированный ext4 на
linear LV размером 128 МиБ и 64 МиБ свободного места в VG. `ResizeLv { lv,
new_size }` и `ResizeFilesystem { filesystem, new_size }` — отдельные явные запросы.
Они сохраняют NodeId существующих объектов и не создают новых узлов.
`set_resize_size()` меняет параметр Resize; `set_created_lv_size()` по-прежнему
меняет только CreateLv. Порядок задаёт вызывающий код, общий replay проверяет
направление и условия по состоянию перед каждым действием:

- grow: сначала LV, затем ФС; рост LV расходует известные свободные extents VG;
- shrink: unmount → forced read-only Check → shrink ФС → shrink LV → mount;
  безопасный минимум ФС остаётся отдельным live-обязательством.

Check должен быть свежим: изменение содержимого/геометрии или mount отзывает
предыдущую гарантию. Нулевые размеры, невыравненный linear LV, нехватка места,
read-only backing, неправильный порядок и отсутствие подготовки дают `Invalid`.
Известный слишком большой потребитель не скрывается неизвестным размером соседа.
Ext resize с несколькими прямыми backing отклоняется; общий capacity check
не выбирает произвольного владельца по порядку обхода графа. Точный расчёт LVM
allocation реализован для linear; неизвестные layout/геометрия остаются
обязательствами, а не выдуманной ёмкостью.

Пример проверяет grow 128 → 160 МиБ, shrink до 96 МиБ, save/resume, freeze,
ошибочный edit, undo и повторное использование 96 МиБ свободного места без
двойного освобождения. Существующие LV/ФС переносятся через уже поддержанные UUID;
rebase повторно проверяет доступную ёмкость. Check/unmount/remount не добавляются
автоматически. Resize разделов, mapping/LUKS и автоматический resize всего стека
этот каталог запросов не включает. Реальные размеры и минимумы проверит будущий runtime.

Удаление явно выбранных объектов:

```sh
cargo run -p storage-engine --example removal_plan
```

[Пример](storage-engine/examples/removal_plan.rs) задаёт отдельные
`DeletePartition { partition }`, `RemoveLv { lv }`, `CloseLuks { mapping }`.
Каждый запрос выбирает ровно один NodeId; рекурсивного режима нет. Раздел/LV
удаляются только без оставшихся верхних потребителей. Close убирает runtime
mapping, сохраняя контейнер и backing. Таблицы разделов, VG, соседние узлы,
существующие ФС и конфигурация mounts не выбираются автоматически. Явное
удаление ФС/сигнатур и связанных snapshot/replication объектов этот рецепт
не реализует; такой потребитель или связь блокирует удаление нижнего узла.

Контракты удаления раздела/LV сохраняют `DestructiveApproval` для выбранного
узла; для раздела также проверяется writable таблицы. `NoUnexpectedConsumers`
проверяет граф и известные runtime mount references. Bind/other mounts требуют
живого разрешения источников, прежде чем считать узел неиспользуемым; контракт
удерживает read resource для runtime mount state. `NoUnexpectedRelations`
блокирует не-владеющие связи в обоих направлениях, а общий эффект удаления
не забывает эти связи автоматически. Scope проверяется независимым coverage:
лишний action или подмена target не проходят. Frozen-план сохраняет все условия.

`PlanEdit::Remove` по-прежнему отменяет запрос в черновике; это не запрос
удаления устройства. Отмена/undo восстанавливают только прогноз, baseline
остаётся прежним. Пример проверяет save/resume, freeze, отказ удалить занятый
LV, undo, сохранность соседей и повторное использование свободного места:
64 МиБ исходных + 16 МиБ выбранного LV без двойного освобождения.

Rebase удаления LV сопоставляет UUID и повторно проверяет новых потребителей;
поддержки корреляции существующих разделов/mappings пока нет, их перенос
отклоняется явно. Сохранение и freeze не разрешают реальную запись или закрытие
mapping. Native dispatch и проверка фактических потребителей остаются работой runtime.

## Проверки наблюдений: C3

`storage_engine::live::check_observed_prerequisites` проверяет часть frozen
prerequisites по отдельно полученному `LiveObservation`. Результат привязан к
probe instance/epoch, условия сохраняют исходные NodeId. Сопоставление использует
существующую строгую корреляцию UUID для VG/LV и обычных ФС; совпадение пути или
NodeId само по себе ничего не доказывает. Неподдержанные объекты и будущие outputs
дают `Unknown`.

Проверяются наблюдаемые наличие, тип и read-only флаг прямого block endpoint.
Обнаруженный потребитель, связь или mount запрещает соответствующее offline
действие. Их отсутствие в частичном снимке остаётся `Unknown`: модель пока не
доказывает полноту provider/namespace coverage. Capacity, credentials, approval,
результат fsck и прочие внешние доказательства также остаются `Unknown`.

```sh
cargo run -p storage-engine --example observed_checks
```

[Пример](storage-engine/examples/observed_checks.rs) использует только fixtures:
фиксирует смену метки ФС, меняет NodeId LV и его read-only флаг во втором снимке,
показывает отказ без изменения frozen-документа. Реальный probe не вызывается.
Полученный частичный отчёт не передаётся как полный admission.

`LiveObservation::validate()` проверяет граф, non-nil IDs, явно заданные epochs
узлов и ссылки typed mount sources. Проверка применяется перед admission,
completion/final verification; ошибка
сохраняет прежние runtime evidence/events. Missing placeholders и unstamped
узлы допускаются как неполные данные. Свежесть и происхождение снимка обязан
подтверждать доверенный host: JSON и счётчик сами этого не доказывают.

Полного `LiveExecutionChecks` и native dispatch пока нет.

`LinuxProbe::instance_id()` отличает независимые экземпляры probe при одинаковых
generations; старые fixture JSON не меняются. `storage_runtime::probe::ProbedObservation::collect(&probe)`
вызывает выбранные adapters ровно один раз, связывает snapshot с этим instance и
generation, использует raw LVM records того же прохода. Импорта такого объекта
из JSON нет. Выбор настоящих native adapters остаётся ответственностью host;
recorded adapters продолжают быть fixtures, независимо от нового UUID.

`ProbedObservation::check_prerequisites()` дополнительно проверяет
`CapacityAvailable` по измеренной свободной ёмкости. VG сопоставляется по UUID;
при смене NodeId условия сохраняют frozen IDs. Изменившиеся/неизвестные размеры
extent не позволяют переосмыслить frozen allocation count. Отсутствующие,
двусмысленные, exported и неточные measurements оставляют `Unknown`; известная
нехватка места даёт `Violated`. Старые baseline/free counts не подмешиваются.
Ошибки, missing information и requires-confirmation diagnostics не дают
положительных observed checks; известные противоречия сохраняются. Warnings
сами по себе evidence не отзывают.

## Execution leases: C3, Linux

`storage_runtime::lease::ExecutionLease` удерживает настоящий неблокирующий
`flock` на `execution.lock`. Одна эксклюзивная блокировка последовательно
пропускает все действия сотрудничающих hosts. Она покрывает весь logical resource
set frozen action и не зависит от transient NodeId, plan UUID или путей устройств.
Раздельная конкурентная работа с независимыми ресурсами пока не реализуется.

Все hosts одного effective user должны использовать **один и тот же постоянный
локальный каталог** с правами 0700 под доверенным родителем. API требует готовый
каталог, проверяет ownership/permissions и открывает directory/lock без следования
symlink; lock должен быть regular, private и иметь ровно одну hard link.
`execution.lock` не удаляется и не обнуляется. Каталог и lock нельзя заменять или
удалять, пока hosts могут работать: новый inode создаст другую область блокировки.
Разные каталоги и произвольные внешние disk tools этой блокировкой не исключаются.

`ExecutionRoot::open()` фиксирует проверенный directory fd.
`try_lock_host()` получает тот же lock без `ExecutionRun` для будущей инспекции
checkpoint; `ExecutionLease::host_lock()` даёт заимствованный guard этого lock.
`storage-runtime` зависит от engine; engine не зависит от runtime, `rustix` или `storage-probe` в production dependencies.
Сбор provider-pass находится в runtime; engine выполняет чистую проверку
`live::check_observed_prerequisites_with_capacity()` над переданными фактами.

Lease отвечает только за OS-блокировку и список ресурсов одного run/action.
Сбор наблюдений, admission и verification выполняются отдельно через `ExecutionRun`.
Drop и завершение процесса освобождают lock, но не доказывают завершения операции.
Конфигурация по умолчанию остаётся read-only; native writes требуют явного
adapter и полного admission под checkpoint.

```sh
cargo run -p storage-runtime --example lease_checkpoint
```

[Пример](storage-runtime/examples/lease_checkpoint.rs) удерживает настоящий lock,
проверяет конкуренцию и отклоняет создание LV по свежему recorded probe с нулём
свободных extents. В новом изолированном каталоге остаётся только `execution.lock`:
история и checkpoint при отказе admission не создаются. Native probe и backend
не вызываются. Межпроцессная конкуренция и освобождение lock при exit без Rust
Drop проверяются отдельным subprocess-тестом.

## Фактический результат: C3

`live::check_observed_postconditions(baseline, observation, conditions)` проверяет
результат по отдельному фактическому снимку, сохраняя frozen IDs и его точный
instance/epoch. UUID correlation поддерживает существующие VG/LV и обычные ФС.
Проверяются size, node spec, label, reprobe, наблюдаемая dependency и точный
filesystem mount в Host. Существующий узел/edge/mount опровергает требуемое
отсутствие; исчезновение из неполного снимка даёт Unknown. Planned outputs,
результаты fsck, activation, persistent config и остальные неподдержанные факты
не подменяются успехом backend или prediction. Blocking diagnostics отзывают
положительные checks.

Полный `record_admission()` переводит действие в `Admitted`, сохраняя разделение
допуска и вызова backend. После надёжной публикации checkpoint host вызывает
`mark_dispatch_started()` (`Running`); при ошибке подготовки —
`mark_pre_dispatch_failed()` (`Failed`) с сохранённой причиной. В `Admitted`
progress/completion и запуск другого действия запрещены. Engine сам не выполняет
файловые операции и не подтверждает наличие checkpoint.

`ExecutionRun::record_completion()` сохраняет outcome и новый actual snapshot в
памяти без обязательного файла. Для `Succeeded` host проверяет postconditions
по сохранённому completion и передаёт `VerificationReport`. Поздний probe не
заменяет этот снимок. Известное расхождение даёт Failed, недостаток evidence —
OutcomeUnknown. Итоговая цель проверяется отдельно; автоматический retry запрещён.

Коррелятор поддерживает PV UUID, WWN диска, GPT UUID раздела и ненулевые NVMe
UUID/NGUID/EUI. Повторяющиеся, конфликтующие и слабые identity отклоняются.
Таблица разделов без собственной UUID сопоставляется только по единственному
Backs от однозначно найденного физического диска/namespace и неизменному типу
таблицы; несколько таблиц или несколько backing parents дают отказ.
Это проверка identity, а не разрешение на запись или проверка полноты discovery.

## Разрешение runtime targets

`storage_runtime::targets::resolve_action()` поддерживает первый `CreateLv` в
существующей VG: сопоставляет UUID в полном графе, проверяет уникальность,
сохранение свойств и связей всего затронутого компонента и таблицы монтирований.
Переиспользованный NodeId/путь не заменяет UUID; занятое имя LV, новый consumer,
изменение геометрии, отношений или mount state отклоняются. Неизвестные для
коррелятора identity также дают отказ, а не уменьшение области проверки.

`ResolvedAction` не десериализуется и не позволяет заменить typed operation,
backend или привязку к probe после resolution. Сохранённое имя VG — endpoint
рядом с проверенным UUID; оно само по себе не разрешает dispatch. Проверка
полноты provider/namespace coverage и авторизация остаются обязанностью host.
Resolver поддерживает linear `CreateLv` и `SetActivation(active=true)` только
для LV, созданного и подтверждённого в этом run. Deactivation, существующие LV
без output binding и остальные write-рецепты пока отклоняются.

Runtime сверяет число PV из same-pass VG measurement с MemberOf edges, требует
present PV с уникальными UUID и физическим диском/namespace в backing ancestry.
Частичная или противоречивая топология не становится положительным admission.
Для физических disks/partitions/namespaces обязательны известный размер,
неизменная BlockGeometry и свежий `read_only = false`; пути и devno могут меняться
при сохранённой identity. Полнота прочих namespace/provider данных всё равно
требует отдельного trusted coverage check.

`LinuxLiveHost` собирает новый provider-pass под `ExecutionLease`, проверяет
принадлежность guard/publication, отсутствие активного checkpoint и все frozen
prerequisites. `BackendCapabilityCheck` отдельно устанавливает поддержку точной
операции и полноту scope/namespace coverage; default scope остаётся `Unknown`.
`AuthorizationContext` вызывается только после остальных положительных проверок
и получает run/action, digest плана, backend и live UUID targets. JSON-флага
разрешения, автоматического допуска по root и production allow-all policy нет.
Положительный admission пока проверяется только с синтетическими test policies;
production scope/authorization policies остаются частью следующего native этапа.

`ExecutionCoordinator::execute_action()` выполняет одно действие: lease →
fresh admission → durable checkpoint → Running → typed synchronous dispatcher →
новый probe → completion → проверка сохранённых postconditions → clear только
после Verified. Backend ID, run/action, operation и publication сверяются перед
допуском; при ошибке подготовки действие остаётся до invocation boundary.
Ошибка adapter не считается доказательством отсутствия побочных эффектов.
Неудача, неизвестный результат или ошибка clear блокируют дальнейшие вызовы
через coordinator. Финальная проверка всей цели остаётся отдельным этапом;
`execute_action()` не объявляет run успешным.

`ExecutionReceipt` может содержать UUID созданного LV. Runtime проверяет, что UUID
не существовал до вызова, уникально наблюдается после него и принадлежит именно
допущенной VG. Остальной затронутый компонент должен совпадать с reviewed scope.
Provisional correlation передаётся в чистую engine-проверку: она независимо
сверяет размер, свойства и Provides edge; UUID не подменяет эти postconditions.
Только после `Verified` появляется запись в `OutputBindings`, затем очищается
checkpoint. Последующие обращения к binding повторно проверяют UUID, VG owner,
свойства и публикацию; другой run использовать его не может.

Coordinator привязан к одному `ExecutionRun`. Успешный синтетический `CreateLv`
проверен до `Verified` и удаления checkpoint, но `run.is_verified()` остаётся false:
Последовательное исполнение DAG и отдельная final goal/preservation verification
подключены и проверены на mock native LVM. Корреляция PV/физических устройств расширена, но production admission и native
write всё ещё требуют проверки полной физической области; неизвестный scope
отклоняется.
Автоматической регистрации write-adapter нет. В provider реализован узкий
`RegisteredBackend::create_linear_lv()` для linear LV с явным `execute` в manifest.
Он повторно проверяет native UUID VG, состав PV, свободное место и кратность
extent, вызывает `bd_lvm_lvcreate` без дополнительных options и читает новый
UUID LV. Все ошибки требуют свежего probe, без автоматического retry.
API libblockdev адресует VG по имени, поэтому внешние LVM writers должны быть
исключены host-политикой: UUID check и native create не атомарны.
Тесты вызывают подставную C-библиотеку; реальные VG пока не проверены.

`storage_runtime::native_lvm::NativeLvmAdapter` связывает этот зарегистрированный
backend с `BackendCapabilityCheck` и `BackendDispatcher`. Он поддерживает только
linear CreateLv и SetActivation(active=true) для его подтверждённого output;
resolver проверяет точные native-параметры до arm. Deactivation и другие операции
получают Unavailable. Полноту scope adapter по умолчанию
оценивает как Unknown: host обязан отдельно доказать coverage, отсутствие
внешних writers и авторизацию. Само создание adapter не разрешает запись.
Disk-free интеграционный тест проходит через настоящий FFI adapter и отдельные
native queries в fresh probe: проверяет наличие checkpoint и удержание lock до
вызова, Verified/binding/clear при успехе и сохранение checkpoint с запретом
retry при ошибке вызова, чтения или результата. Это проверка ABI и runtime
порядка на mock-библиотеке, а не готовности host-политик или реального LVM.

Runtime admission передаёт подтверждённые `OutputBindings` в pure prerequisite
checks и заново сопоставляет UUID каждого output при каждом probe. Ожидаемая
область расширяется только проверенными LV и их Provides edge, не копируется
из текущей топологии. Синтетическая цепочка CreateLv → SetActivation проходит
до двух Verified; успех активации дополнительно требует наблюдаемого writable
block interface с известной геометрией. Unknown activation или read-only endpoint
оставляют checkpoint, а UUID replacement отклоняется до dispatch.
Эта же цепочка проверяется через native C adapter на mock-библиотеке с fresh
queries между действиями. Native activation отдельно проверяет MODIFY/QUERY,
UUID VG/LV, размер и фактическую активацию после вызова; force-like options
отключены. Ошибки native call/post-query сохраняют инцидент без retry; отсутствие
поддержки или activation symbol отклоняется до arm. Эти тесты не заменяют VM/VG
испытаний и не объявляют весь run успешным без отдельной final verification.

## Аварийный checkpoint вместо истории выполнения

Старый JSONL-журнал удалён. Импорт исторических `.jsonl`, восстановление run и
автоматический повтор действий больше не поддерживаются. Существующие файлы
пользователя не удаляются и не преобразуются автоматически.

`storage_runtime::checkpoint::ActiveAttempt` описывает одну попытку: run/plan/action,
необязательный OperationId, выбранный backend, внешние identity целей и SHA-256
канонического плана. Это диагностические данные, не разрешение на запись.
`capture()` требует `Admitted`; strict parsing отклоняет неизвестные поля, версии,
пустые LVM UUID и превышение лимитов (64 KiB, 64 targets, 16 identity на target,
1024 байта на строку). Пока разрешены только `CreateLv` и `SetActivation`: другие
действия требуют отдельной проверки допустимых для записи параметров.

На Linux `CheckpointStore::new(&host_lock)` заимствует реально удерживаемый
`HostExecutionLock`; store нельзя использовать после освобождения guard.
`inspect()` читает файл через закреплённый directory fd без следования symlink,
проверяет regular/private/owner/single-link и лимит размера. Только отсутствие
имени означает отсутствие попытки; повреждённый файл блокирует новые записи.

`arm()` создаёт уникальный временный файл 0600, синхронизирует содержимое,
публикует `active-attempt.json` через `renameat2(RENAME_NOREPLACE)` и синхронизирует
каталог. Второй checkpoint не заменяет первый. При ошибке после публикации
активный файл сохраняется. `clear(run, action)` проверяет владельца попытки и
inode файла, удаляет только совпадающий checkpoint и синхронизирует каталог.
После ошибки мутации handle запрещает дальнейшие изменения: host обязан
остановить run, даже если имя уже было удалено. Более слабых fallback для
неподдерживаемой ФС нет.

После нормального `clear()` остаётся только постоянный `execution.lock`.
`SIGKILL` до публикации может оставить неактивный `.active-attempt-*.tmp`:
он не разрешает dispatch и не читается как инцидент; автоматическое удаление
посторонних или оставшихся временных файлов не выполняется. После публикации
checkpoint переживает завершение процесса и требует отдельного разбора.

Тесты проверяют I/O-отказы на границах sync/publication/removal, конкуренцию
hosts и реальные дочерние процессы с `SIGKILL`. Это не проверка отключения питания
или аппаратной надёжности накопителя. Полный запуск первого LVM recipe проверен
через mock native adapter, без изменения реальных устройств;
настройки по умолчанию не разрешают native writes, незавершённый disks preview
остаётся выключенным.

## Осмотр прерванной попытки

`storage_runtime::incident::inspect_incident(root, probe)` использует тот же
`execution.lock`, читает checkpoint и выполняет новый provider-pass. Run не
восстанавливается; backend не вызывается, checkpoint не удаляется. Повреждённый
файл, недоступный lock или ошибка probe возвращаются как блокирующие ошибки.

`EffectObserved` означает лишь уникальное присутствие LV нужного имени, размера,
layout и UUID под сохранённой VG. Это не доказательство, кто его создал, завершена
ли операция и остановлен ли backend. Отсутствие результата, неоднозначность,
blocking diagnostics и SetActivation без фактического activation evidence дают
`Indeterminate`; отсутствие в частичном probe не объявляется отсутствием эффекта.
Подтверждение инцидента требует отдельной проверки безопасности перед clear.

`review_incident()` выдаёт неизменяемый `IncidentReview` и одноразовый challenge.
`acknowledge_incident()` принимает явное подтверждение, удерживает общий lock,
проверяет тот же root inode и полный checkpoint, повторяет probe и отклоняет
изменившееся состояние (publication epochs не считаются изменением содержимого).
Перед clear обязательно вызывается trusted `IncidentAcknowledgmentPolicy`:
он должен подтвердить права вызывающего, полноту relevant coverage и фактическую
остановку native-операции. Положительной production policy по умолчанию нет.
Unknown/Violated, подмена checkpoint даже с прежними run/action IDs или ошибка
probe/clear запрещают продолжение. Acknowledge не меняет старый ExecutionRun и
не запускает повтор. Если identity/NodeId или данные изменились, нужен новый review.

Native LVM probe сохраняет ошибки `GError` из запросов PV/VG/LV: ошибка чтения
больше не выглядит как пустая топология. Обязательные identity с null/пустой
строкой или неверным UTF-8 отклоняются целиком; ранняя ошибка освобождает и
непрочитанные native records. Init/query/close плагина сериализованы внутри
процесса. Этот probe используется также для повторных проверок narrow write
adapter; обычный manifest по-прежнему разрешает только чтение.

## Наблюдения для нового baseline

Pure postconditions принимают provisional filesystem UUID bindings для ФС с
совпадающим типом, единственным UUID и Present-наблюдением. Размер, label и
backing dependency проверяются отдельно. Эти bindings не включают native mkfs:
runtime по-прежнему обязан доказать происхождение нового UUID и owning backing.
Runtime mount postconditions требуют ровно одну запись на Host target и
UUID-correlated filesystem source (либо typed tmpfs). Дубликаты/overmount,
unresolved source, отсутствие записи и неподдержанная namespace остаются Unknown;
подтверждённый чужой source даёт Violated. Условия native Mount описаны ниже.
Runtime scope заново коррелирует typed mount sources, включая ФС вне изменяемого
компонента. Смена transient ID и порядка options допустима; UUID replacement,
клонированный UUID, новый target или неоднозначный mount target блокируют admission.

Сквозной native mock DAG содержит существующую ext4 на другом диске. Её transient
ID меняется на каждом probe; UUID и mount semantics остаются основанием сравнения.
Финальные option drift, overmount и filesystem UUID replacement не дают Verified
run после успешных actions. Эти pure checks сами по себе не разрешают native writes.

`live::check_observed_goal()` независимо проверяет финальный фактический граф
после UUID correlation, включая подтверждённые outputs, точные связи/размеры
и последнее запрошенное состояние активации. Host filesystem/tmpfs mounts
сверяются по уникальному target, source и explicit options без учёта порядка.
Defaults, дубликаты keys, противоречивые flags и неизвестные namespaces остаются
Unknown; persistent mounts, managed boot и неподдержанные intent evidence тоже
не объявляются успешными. Эта pure проверка не доказывает полноту probe или
сохранность содержимого защищённых объектов: это отдельная обязанность host.
`LinuxLiveHost::verify_goal()` требует новый probe после всех Verified actions
под host lock без checkpoint. `check_final_scope()` по умолчанию Unknown;
непустая protection policy требует независимого `check_preservation()`.
Успех action admission не подменяет эти финальные доказательства.
`ExecutionCoordinator::execute_run()` идёт по DAG последовательно и удерживает
один host lock от первого действия до отдельного финального probe. Каждый action
использует тот же admission/checkpoint/dispatch путь; любой не-Verified результат
останавливает проход без retry. `ExecutionSummary.verified` следует только за
принятой final goal/preservation проверкой. Уже успешный run повторно не запускается.
До первого write runtime заново анализирует frozen document и проверяет наличие
adapter для всех операций DAG, включая ещё не созданные outputs. Unknown,
Unsupported или несовпадение backend у позднего действия останавливают весь
запуск до первого arm. Это не отменяет fresh admission каждого отдельного action.
Сквозной native DAG тест требует независимый final coverage даже после двух
Verified actions. При final Unknown повторная проверка получает новый probe,
но не повторяет writes. Непустая protection policy без доказательства сохранности
остаётся Unknown даже при точном совпадении графа. Host mount source/options
проверяются по actual observations; persistent/managed boot/namespace adapters
и production policies пока не реализованы. Default configuration остаётся read-only.

C4 runtime поддерживает узкую вертикаль на явно настроенном dispatcher:
`CreateLv → SetActivation → CreateFilesystem(ext4) → Mount(Host)`. Resolver принимает только
подтверждённый LV этой попытки с active/writable endpoint evidence. Отдельные
`check_consumer_exclusion()` и `check_destructive()` по умолчанию Unknown;
обычный scope/authorization не подтверждает отсутствие потребителей и согласие
на уничтожение содержимого. Native dispatch дополнительно требует удерживаемый endpoint;
одних BlockFacts для вызова mkfs недостаточно.

Receipt различает LV и filesystem UUID. Ext4 completion требует нового
канонического UUID, единственной Present ФС с точным label, единственной Backs
связи с подтверждённым LV и неизменности остального scope. Binding публикуется
только после всех postconditions; последующие probe заново проверяют владельца,
UUID и первоначально наблюдённый размер ФС. Исторический UUID нельзя выдать за
созданный output. Checkpoint сохраняет ext4 intent и предшествующий LV UUID.
Сквозные mock-тесты проверяют Verified run, смену transient IDs и сохранение
checkpoint/запрет retry при ошибочных receipts, duplicate UUID, неверном владельце,
label drift, дополнительных consumers и blocking diagnostics.
`NativeExt4Adapter` использует execute-enabled `libbd_fs`: полная MKFS/QUERY ABI,
UUID/label/no-discard options и зависимости проверяются до первого write.
`LvBlockEndpoint` требует один `/dev` path без final symlink, проверяет через O_PATH
block type и major/minor, затем удерживает открытый RW descriptor. До/после вызова
сверяются capacity, logical/physical geometry и sysfs DM UUID/ro/size с VG/LV UUID.
Guard не использует O_EXCL: host обязан исключать mounts, external writes и DM
table changes на всё время dispatch и verification; default policy остаётся Unknown.

Native recipe получает `/proc/<parent-pid>/fd/<fd>`, проверяет отсутствие сигнатуры,
вызывает `bd_fs_mkfs(ext4)` без force, dry-run и extras, выполняет sync_all и проверяет
свежие type/UUID/label и допустимую ext4 geometry. Произвольного path-параметра в
provider write API нет; обычные файлы отвергаются. `NativeStorageAdapter` фиксирует
отдельные LVM/FS/Mount регистрации (`new(lvm, filesystems, mounts)`); `backend_for(operation)` сверяется в whole-plan readiness
и каждом dispatch. Он не добавляет положительных scope/approval/final policies.
`NativeMountAdapter` принимает только ext4 output этого же run и профиль
`rw,nosuid,nodev,noexec,relatime` в Host namespace. Source удерживается через
`LvBlockEndpoint`; существующий пустой target открывается через openat2 без symlinks.
Libmount получает descriptor paths; у directory path обязателен завершающий `/`,
иначе fd-based move_mount может отвергнуть конечный magic-link. Helpers, ambient
fstab, mtab writes и read-only fallback отключены. Native status не заменяет fresh
probe: runtime проверяет единственный target, filesystem UUID, точные options и
неизменность остального scope. Checkpoint Mount включает VG/LV/FS fingerprints.
Отдельная host policy должна исключать внешние source writers и изменения target
и namespace на весь интервал; `check_mount_exclusion()` по умолчанию Unknown.
Unmount, произвольные options, другие sources/namespaces и persist не реализованы.

Native ABI success/failure проверены на частной C-библиотеке, которая меняет только
свои записи в памяти. Только private unit ABI tests обходят block-type gate на
новых sparse temporary files; публичный API такого обхода не предоставляет.
Runtime тесты проверяют разные backend routes и сохранение checkpoint при отказе
native descriptor guard. Контролируемая QEMU VM проверяет реальную четырёхшаговую
цепочку через coordinator, отдельный final probe и запись/чтение файла на ext4.
Отказ после arm на последнем шаге сохраняет checkpoint и запрещает retry.
Исключение внешних writers в production и power loss не проверены. Критерий C4 для этого узкого
сценария выполнен; эксплуатационные ограничения остаются отдельной работой. Стенд и воспроизводимые команды: [C4 VM](storage-runtime/tests/vm/README.md).

`NodeFacts.lvm_active` хранит необязательное наблюдение native LVM activation.
Оно читается из state column `lv_attr`; suspended/неизвестное состояние остаётся
Unknown. Отсутствие endpoint не доказывает inactive. Pure postconditions
проверяют `Activation` по этому факту и UUID correlation, включая provisional
bindings созданных LV; это ещё не подтверждение writable block interface.
`check_observed_prerequisites_with_bindings()` использует ту же строгую UUID
корреляцию для последующих действий над подтверждёнными outputs. Без binding,
при неоднозначной identity или blocking diagnostics prerequisites остаются
Unknown; существующие baseline-узлы нельзя переназначить через output binding.

`ProviderState.lvm_vgs` и `ProbeSnapshot.lvm_vgs` сохраняют исходные LVM VG
records с соответствующими `NodeId` из того же прохода. `LinuxProbe` вызывает
каждый адаптер один раз и публикует граф вместе с этими записями; конфликтующие
узлы не предоставляют ёмкость. Старый JSON snapshot без `lvm_vgs` читается
с неизвестной ёмкостью. Native-адаптеры могут заимствовать регистрации backend-ов
на время жизни `LinuxProbe`.

Прикладной код передаёт `snapshot.current` и `snapshot.lvm_vgs` в
`HistoricalBaseline::from_lvm_observations()`. Engine проверяет присутствие и тип
VG, уникальность UUID и привязки, имя, полный размер и ненулевой extent size.
Свободные байты должны точно делиться на размер extent и не превышать размер VG.
Экспортированная VG, дубликаты, неточные или несогласованные данные оставляют
ёмкость неизвестной с диагностикой; известный ноль остаётся нулём.
`storage-provider` и `storage-probe` не зависят от engine.

`baseline.has_same_observations(&candidate)` позволяет показать изменение
наблюдений независимо от UUID публикации и локального generation probe.
Сравнение не подтверждает актуальность системы и не заменяет корреляцию
устройств. Исторический `PlanDocument::analyse_against()` по-прежнему требует
исходный ID baseline. Открытая сессия сама не меняет свой baseline: сохраните
черновик и явно создайте `PlanSession::new(candidate)` для нового контекста.
Для переноса запросов используйте отдельный просмотр и принятие rebase:

```rust
let review = session.prepare_rebase(session.revision(), candidate)?;
// Покажите review.correlations(), review.draft() и review.preview().
session.accept_rebase(session.revision(), review)?;
```

`prepare_rebase()` не меняет активную сессию. Существующие VG/LV сопоставляются
по единственному LVM UUID, обычные ФС — по единственному filesystem UUID с
согласованным типом. Идентификатор должен быть уникален в обоих полных графах;
объекты должны быть `Present`. Имена, пути, serial и совпавший `NodeId` не
доказывают identity. Пропавшие объекты, клоны UUID, противоречивые identity records,
смена типа/раскладки и коллизии planned IDs отклоняются. Начальный rebase не
поддерживает другие виды узлов, path-only Unmount/RemovePersistentMount,
bind/network/Btrfs-subvolume/ZFS-dataset mounts и не-host контексты.

Кандидат сохраняет `OperationId`, planned `NodeId`, порядок запросов и политику
защиты; меняются только сопоставленные ссылки на существующие узлы. Прогноз
пересчитывается общим engine с ёмкостью нового baseline. `Invalid` остаётся
доступен для просмотра, без старого desired.

`accept_rebase()` сверяет revision и полный исходный контекст, атомарно
публикует кандидата и увеличивает revision. История undo/redo очищается на границе
baseline; последующие undo относятся уже к новому контексту. Старые сохранённые
черновики и `FrozenPlan` не меняются. Можно принять невалидный кандидат для
исправления запросов, но freeze и исполнение от этого не разрешаются.
Устаревший кандидат нужно подготовить заново.

Проверка rebase на искусственных наблюдениях:

```sh
cargo run -p storage-engine --example rebase_plan
```

Проверяемый пример интеграции:

```sh
cargo run -p storage-engine --example probe_plan
```

По умолчанию используется записанная искусственная публикация. Пример проверяет
путь через `StateProvider`, assembler, `LinuxProbe`, capacity evidence и сессию,
включая сохранение старого черновика/документа при открытии нового baseline.
Для чтения реальной системы укажите manifest со всеми native backend-ами
`NativeSystemProvider`; отсутствующие зависимости приводят к явной ошибке:

```sh
cargo run -p storage-engine --example probe_plan -- /path/to/providers.json
```

Native-режим выполняет read-only probe и выводит preview пустого плана с
диагностикой. Он не выбирает устройства для изменения и не исполняет actions.
Доступ к probe нужен только examples/tests; runtime-зависимость engine не менялась.

## Готовность этапов 3 и 4

Этапы завершены в объёме поддерживаемых типизированных действий: проверка
переходов и цели, сохранение плана и подготовка границы с будущим исполнителем.
Это готовность библиотечного планирования и контрактов исполнения.

| Требование | Реализация и проверяемые сценарии |
|---|---|
| Безопасные промежуточные состояния | [Эффекты над DesiredState](storage-engine/src/effects.rs): атомарные эффекты, неизменность исходного снимка, отклонение read-only backing перед mkfs, подготовка shrink и отзыв устаревших гарантий |
| Общие ресурсы и расход ёмкости | [Композиция](storage-engine/src/compose.rs): конфликтующие действия требуют порядка; два выделения по 60 при доступных 100 отвергаются; resize и removal учитывают освобождение без повторного зачёта |
| Доступность endpoint | [Композиция](storage-engine/src/compose.rs): `CreateLv → Activate → CreateFilesystem → Mount`, запрет mount после деактивации backing, условия новых узлов проверяются в состояниях предшественников |
| Runtime и persistent mounts | [Симуляция](storage-engine/src/simulate.rs): runtime mount не удовлетворяет запрос persistence; неизвестная конфигурация остаётся обязательством для проверки |
| Цель, intents и защищённая область | [Композиция](storage-engine/src/compose.rs): `Check` сохранён даже без graph effect, неподдерживаемый intent отвергается, защищены также временные изменения; неизвестная запрошенная ёмкость не считается доказанной |
| JSON, версии и исторический baseline | [PlanDocument](storage-engine/src/wire.rs): канонический round-trip, структурная проверка до replay, пересчёт контрактов на новых узлах, отказ при подмене условий, порядка, версии или исторического контекста |
| Отдельное состояние запуска | [ExecutionRun](storage-engine/src/execution.rs): зависимые действия ждут `Verified`; чужие, неполные и устаревшие отчёты отвергаются; ошибка или неизвестный исход останавливают обычное продолжение |
| Проверка фактического результата | [ExecutionRun](storage-engine/src/execution.rs): фактический снимок сохраняется и при ошибке, прогноз на него не накладывается; успешный backend и проверенные действия ещё требуют отдельной проверки конечной цели |

В указанных модулях находятся регрессионные тесты этих сценариев. Формат плана
имеет `PLAN_SCHEMA_VERSION = 3`, каталог контрактов — `ACTION_SEMANTICS_VERSION = 11`.
Frozen-планы с другой версией семантики отклоняются; открытие сохранённого
черновика пересчитывает контракты текущей версией engine.
`requested_goal.mounts` содержит отдельные массивы `runtime` и `persistent`;
`MountEntry` не содержит прежнего флага `persistent`. `PlanDocument::from_json`
явно отвергает версии 1 и 2 до разбора старой формы монтирований. Устаревшие поля
в `MountSpec` и `MountEntry` отклоняются; автоматической миграции нет.

Следующая работа — реализация доверенных live-checkers, корреляции устройств,
блокировок ОС и write-dispatch. Сейчас `LiveExecutionChecks` задаёт их контракт,
а `ExecutionRun` проверяет полноту отчётов доверенного host; сам отчёт не является
измерением или авторизацией. Универсальный решатель desired diff, reformat,
планирование boot и автоматическое восстановление после сбоя пока отсутствуют.
Сложная доступность Btrfs/ZFS и неизвестные ограничения остаются `Unknown`.
Реальная запись и незавершённый disks preview выключены.

## Поток данных

1. `BackendRegistry` читает manifest и выбирает доступные системные API.
2. Logical provider вызывает свой backend, проверяет семантику и переводит backend-наблюдения в типизированный результат.
3. `storage-probe` собирает идентификаторы, размеры и связи в `NodeGraph`.
4. Ассемблер объединяет граф, монтирования, сведения о среде и диагностику в `CurrentState`.
5. Состояние можно сериализовать, сравнить с fixture-эталоном или передать в `storage-engine` для композиции и симуляции типизированных фрагментов.
6. `DesiredState::from_current` создаёт начальную конфигурацию; симуляция плана изменяет её локальную копию и возвращает `SimulationResult { desired, evidence, steps }`. После исполнения фактический `CurrentState` получают новым probe.

## Проверки

Основной набор запускается без доступа к реальным дискам:

```sh
cargo test --workspace
```

Подробная целевая модель проекта находится в [`docs/storage-backend-spec.md`](docs/storage-backend-spec.md), а сценарии проверок — в [`docs/tests.md`](docs/tests.md).
