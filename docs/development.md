# Разработка

## Контейнер

Сборка идёт в Distrobox `okno-dev` на Fedora 44:

```sh
distrobox create --name okno-dev --image registry.fedoraproject.org/fedora:44
distrobox enter okno-dev -- sudo dnf -y install \
  rust cargo clippy rustfmt clang cmake nasm pkgconf-pkg-config protobuf-compiler \
  pipewire-devel dbus-devel fontconfig-devel freetype-devel libxkbcommon-devel \
  wayland-devel libX11-devel opus-devel \
  mingw64-gcc mingw64-winpthreads-static rust-std-static-x86_64-pc-windows-gnu
```

## Команды

```sh
distrobox enter okno-dev -- cargo build --workspace
distrobox enter okno-dev -- cargo test --workspace
distrobox enter okno-dev -- cargo clippy --workspace --all-targets
distrobox enter okno-dev -- cargo build --workspace --target x86_64-pc-windows-gnu
```

Линковщик для Windows задан в `.cargo/config.toml`. Зависимости в dev-профиле
собираются с `opt-level = 2`: без этого Argon2 и шифрование в тестах работают
в десятки раз медленнее.

## Ручная проверка на одной машине

```sh
export OKNO_CONFIG_DIR=/tmp/okno-host
echo 'secret-pass' | okno-cli set-password --user me
okno-cli host --port 21290 &
OKNO_CONFIG_DIR=/tmp/okno-client okno-cli discover
echo 'secret-pass' | OKNO_CONFIG_DIR=/tmp/okno-client okno-cli connect 127.0.0.1:21290 --user me --trust
```

`OKNO_LOG=debug` включает подробный журнал.

## Устройство

| Крейт | Назначение |
|---|---|
| `okno-proto` | Сообщения протокола (`prost`, без `.proto` и `protoc`) |
| `okno-net` | Noise_XX, фрагментация, каналы с приоритетами |
| `okno-auth` | Argon2id, задержки входа, allowlist, TOFU |
| `okno-discovery` | mDNS, UDP-broadcast, Wake-on-LAN |
| `okno-core` | Конфиг, ключ устройства, хост и клиентская сессия |
| `okno-cli` | Командная строка |
