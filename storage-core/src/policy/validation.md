Канонический граф обязан уметь представить найденные варианты:

```text
mdraid -> LVM PV
mdraid -> Btrfs
LVM RAID
Btrfs native RAID
```

Но стандартная policy создания PDisks должна:

- запрещать создание LVM поверх mdraid;
- предлагать native LVM RAID для LVM-сценариев;
- запрещать создание Btrfs поверх mdraid;
- предлагать native Btrfs profiles для multi-device Btrfs;
- разрешать standalone mdraid, если его верхним слоем является обычная ФС или swap и policy это допускает.

Это правило живёт в policy/validation, а не в `DeviceKind`, `GraphBuilder` или probe.
