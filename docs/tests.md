# Ручное тестирование модели

Обычные проверки, не требующие root:

```sh
cargo test -p storage-core
cargo test -p storage-provider
cargo test -p storage-probe
cargo clippy -p storage-provider -- -D warnings
cargo check -p storage-probe --tests
```

Проверка загрузки всех заявленных динамических библиотек и разрешения их ABI:

```sh
env PDISKS_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  installed_native_provider_abi_matrix_matches_manifest -- \
  --ignored --nocapture --test-threads=1 --show-output
```

Проверка наличия адаптеров `CurrentState` для всех библиотек обнаружения:

```sh
env PDISKS_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  native_current_state_adapter_matrix_is_complete -- \
  --ignored --nocapture --test-threads=1 --show-output
```

Реальные проверки отдельных слоёв:

```sh
# libmount и таблица текущих монтирований
sudo env PDISKS_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  native_libmount_populates_current_state -- \
  --ignored --nocapture --test-threads=1 --show-output

# udev + fdisk + blkid + libmount
sudo env PDISKS_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  native_block_stack_populates_connected_current_state -- \
  --ignored --nocapture --test-threads=1 --show-output

# libdevmapper + libcryptsetup
sudo env PDISKS_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  native_mapper_stack_populates_current_state -- \
  --ignored --nocapture --test-threads=1 --show-output

# LVM + MD RAID + loop + swap + multipath и нижележащие слои
sudo env PDISKS_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  native_local_storage_stack_populates_current_state -- \
  --ignored --nocapture --test-threads=1 --show-output
```

Главная end-to-end проверка всех адаптеров `CurrentState`, включая Btrfs, ZFS
и NVMe:

```sh
sudo env PDISKS_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers \
  native_system_storage_stack_populates_current_state -- \
  --ignored --nocapture --test-threads=1 --show-output
```

Запуск всего набора реальных проверок одной командой:

```sh
sudo env PDISKS_PROVIDER_MANIFEST="$PWD/storage-probe/tests/providers.live.example.json" \
  cargo test -p storage-probe --test live_providers -- \
  --ignored --nocapture --test-threads=1 --show-output
```
