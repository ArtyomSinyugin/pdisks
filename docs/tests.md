# Ручное тестирование модели

Обычные проверки, не требующие root:

```sh
cargo test -p storage-core
cargo test -p storage-provider
cargo test -p storage-probe
cargo test -p storage-runtime
cargo clippy -p storage-provider -- -D warnings
cargo check -p storage-probe --tests
```

Проверка загрузки всех заявленных динамических библиотек и разрешения их ABI:

```sh
env DISKSGRAPH_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  installed_native_provider_abi_matrix_matches_manifest -- \
  --ignored --nocapture --test-threads=1 --show-output
```

Проверка наличия адаптеров `CurrentState` для всех библиотек обнаружения:

```sh
env DISKSGRAPH_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  native_current_state_adapter_matrix_is_complete -- \
  --ignored --nocapture --test-threads=1 --show-output
```

Реальные проверки отдельных слоёв:

```sh
# libmount и таблица текущих монтирований
sudo env DISKSGRAPH_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  native_libmount_populates_current_state -- \
  --ignored --nocapture --test-threads=1 --show-output

# udev + fdisk + blkid + libmount
sudo env DISKSGRAPH_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  native_block_stack_populates_connected_current_state -- \
  --ignored --nocapture --test-threads=1 --show-output

# libdevmapper + libcryptsetup
sudo env DISKSGRAPH_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  native_mapper_stack_populates_current_state -- \
  --ignored --nocapture --test-threads=1 --show-output

# LVM + MD RAID + loop + swap + multipath и нижележащие слои
sudo env DISKSGRAPH_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  native_local_storage_stack_populates_current_state -- \
  --ignored --nocapture --test-threads=1 --show-output
```

Главная end-to-end проверка всех адаптеров `CurrentState`, включая Btrfs, ZFS
и NVMe:

```sh
sudo env DISKSGRAPH_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  native_system_storage_stack_populates_current_state -- \
  --ignored --nocapture --test-threads=1 --show-output
```

Запуск всего набора реальных проверок одной командой:

```sh
sudo env DISKSGRAPH_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers -- \
  --ignored --nocapture --test-threads=1 --show-output
```


## C8: конфигурация host и границы привилегий

Проверки не требуют root и не записывают на устройства:

```sh
cargo test -p storage-provider --lib registry_ --locked --offline
cargo test -p disksgraph --lib native_configuration --locked --offline
cargo test -p disksgraph --test wire native_configuration --locked --offline
cargo test -p storage-runtime --lib checkpoint::store::tests --locked --offline
cargo test -p storage-runtime --test execution_leases --test runtime --locked --offline
cargo test -p dg-cli --lib --test exit_codes --locked --offline
cargo test -p disksgraph-ffi --test checked_consumer --locked --offline
```

Частная Rust `.so` и `/proc/self/maps` проверяют отсутствие загрузки native-кода
до полной проверки manifest и root. Это проверка фактического loader через Rust
и Stabby checked import, без подмены production constructor. Матрица credentials
проверяет отказ несовпадающих UID/GID на synthetic значениях; setuid/setgid install
и production privilege separation не заявлены.

Runtime проверяет полный principal request на каждом свежем admission, права
root/lock/checkpoint и сохранение marker при подмене lock или изменении прав из
callback перед arm/clear. CLI subprocess проверяет явную native-конфигурацию и
выход 1 без panic при закрытом stdout `--help`. FIFO inputs отклоняются без writer.

После SIGKILL/crash suites не запускайте ignored helpers отдельно: их вызывают
обычные parent tests. Native writes, внешние writers, privileged configuration
races и power loss эти synthetic проверки не моделируют. Полный контракт и
незавершённые production gates: [trusted host](host-trust.md).

При нехватке места сборку можно вынести в отдельный каталог без debug symbols:

```sh
CARGO_TARGET_DIR=/tmp/disksgraph-check CARGO_INCREMENTAL=0 \
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 \
cargo test --workspace --all-targets --locked --offline
```


Проход 20 октября 2026 проверен на Rust/Cargo 1.98.1: полный locked/offline
workspace/all-targets — 452 passed, 19 ignored; Clippy с `-D warnings`, rustfmt и
diff-check прошли. Новый локальный release bundle прошёл `scripts/check-package.py`
с отдельно собранным consumer: checksums, production exports, checked ABI/CLI
preview parity и сохранение incident marker. В bundle включён HOST-TRUST.md.
MSRV 1.88, native VM/crash и power loss в этом проходе не повторялись.
