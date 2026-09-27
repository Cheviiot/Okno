# Разработка

## Контейнер

Сборка идёт в Distrobox `okno-dev` на Fedora 44:

```sh
distrobox create --name okno-dev --image registry.fedoraproject.org/fedora:44
distrobox enter okno-dev -- sudo dnf -y install \
  rust cargo clippy rustfmt clang cmake nasm pkgconf-pkg-config protobuf-compiler \
  pipewire-devel dbus-devel fontconfig-devel freetype-devel libxkbcommon-devel \
  wayland-devel libX11-devel opus-devel \
  mingw64-gcc mingw64-gcc-c++ mingw64-winpthreads-static \
  rust-std-static-x86_64-pc-windows-gnu
# Для проверки интерфейса на виртуальном дисплее:
distrobox enter okno-dev -- sudo dnf -y install \
  xorg-x11-server-Xvfb ImageMagick xdotool mesa-dri-drivers \
  libXcursor libXrandr libXi libxkbcommon-x11 libX11-xcb mesa-libEGL mesa-libGL
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

`okno-cli host --test-pattern` отдаёт сгенерированную картинку вместо экрана, а
`connect --frames 90 --snapshot shot.png` принимает видео и сохраняет последний кадр.

Настоящий захват на Linux идёт через портал RemoteDesktop: при первом запуске
GNOME показывает диалог, токен восстановления сохраняется в
`host.portal_restore_token`, и дальше диалога нет. Автотесты портал не трогают.

`OKNO_LOG=debug` включает подробный журнал.

## Устройство

| Крейт | Назначение |
|---|---|
| `okno-proto` | Сообщения протокола (`prost`, без `.proto` и `protoc`) |
| `okno-net` | Noise_XX, фрагментация, каналы с приоритетами |
| `okno-auth` | Argon2id, задержки входа, allowlist, TOFU |
| `okno-discovery` | mDNS, UDP-broadcast, Wake-on-LAN |
| `okno-codec` | H.264 (OpenH264 из исходников): кодер и декодер |
| `okno-desktop` | Захват экрана и ввод: порталы + PipeWire (Linux), WGC + SendInput (Windows), тестовый рабочий стол |
| `okno-core` | Конфиг, ключ устройства, хост, клиентская сессия, служба рабочего стола |
| `okno-app` | Приложение на Slint (`okno`) |
| `okno-cli` | Командная строка |

## Интерфейс

Экраны описаны один раз в `crates/okno-app/ui/app.slint` и используют только
компоненты набора `@kit`: `KButton`, `KEntry`, `KSwitch`, `KGroup`, `KRow`,
`KPage`, `KSidebar`, `KDialog` и другие. Наборов два, с одинаковым API:
`ui/kit/adwaita` (токены libadwaita 1.6) и `ui/kit/fluent` (WinUI 3). `build.rs`
выбирает набор по целевой ОС; `OKNO_UI_KIT=adwaita|fluent` переопределяет выбор.
Новый компонент нужно добавлять в оба набора сразу.

Строки интерфейса пишутся по-английски в `@tr(...)`; строки, которые собирает
Rust-код, живут в глобальном объекте `Messages` в `app.slint`. Русский перевод —
`crates/okno-app/po/ru/LC_MESSAGES/okno-app.po`, он вшивается при сборке.
Язык берётся из системы, `OKNO_LANG=en|ru` переопределяет.

Скриншот на собственном виртуальном дисплее (основной экран не затрагивается):

```sh
distrobox enter okno-dev -- scripts/ui-screenshot.sh /tmp/shot.png 1 fluent ru dark
```

Аргументы: страница (0 — устройства, 1 — этот компьютер, 2 — настройки),
набор, язык, схема (`OKNO_COLOR_SCHEME=light|dark`).

`zbus` должен работать без tokio (`ashpd` с `async-io`): поток AccessKit
интерфейса вызывает zbus вне tokio-runtime и падает, если у zbus включён tokio.
