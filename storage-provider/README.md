# Provider manifests

Provider manifests belong to pdisks. Third-party packages such as `libzfs`,
LVM, and libfdisk do not contain or maintain them.

Each manifest describes one pdisks integration and the native library symbols
or command it needs. `manifest_version` versions this JSON schema only. Native
API requirements differ between integrations and are expressed by
`library.required_symbols` plus provider-specific adapter tests.

At runtime pdisks reads one manifest file and registers capabilities only when
the declared library loads with every required symbol, or when the declared
command exists. A missing optional package is therefore normal and does not
prevent pdisks from starting.

The file is `/usr/share/pdisks/providers.json` by default. Set
`PDISKS_PROVIDER_MANIFEST` to replace it.

The pdisks RPM build should:

1. generate one architecture-specific manifest from a project-owned template,
   expanding `%_libdir` into an absolute library path;
2. install it as `%_datadir/pdisks/providers.json`;
3. add runtime `Requires` selected by the package maintainer;
4. add matching `BuildRequires` when native integration tests are enabled.

Third-party RPM specs need no changes.

Example generated manifest:

```json
{
  "manifest_version": 1,
  "providers": [
    {
      "id": "zfs",
      "library": {
        "path": "/usr/lib64/libzfs.so",
        "required_symbols": ["libzfs_init", "libzfs_fini"]
      },
      "capabilities": ["probe"]
    }
  ]
}
```

Set `PDISKS_PROVIDER_MANIFEST` when running tests to make
`installed_provider_manifests_match_host` verify every selected manifest
against the libraries and commands installed by `BuildRequires`.

RPM `%check` example:

```sh
PDISKS_PROVIDER_MANIFEST="$PWD/providers.json" \
  cargo test -p storage-provider --test installed_tools \
  installed_provider_manifests_match_host -- --ignored --exact
```

This strict test opens every declared library, resolves every required symbol,
and checks that every declared command is a regular executable file. Runtime
discovery remains permissive and skips unavailable optional providers.
