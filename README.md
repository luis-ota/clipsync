# clipsync

> **Clipboard universal entre Android e Linux via LAN.**
> Zero cloud. Zero conta. Zero servidor externo.

`clipsync` sincroniza o clipboard entre o seu celular Android e o seu PC Linux
(em Wayland ou X11) pela rede local. Copiou no celular → cola no PC.
Copiou no PC → cola no celular. Texto e imagens. Em tempo real.

Inspirado em [KDE Connect](https://kdeconnect.kde.org/), mas com escopo
cirúrgico: **só clipboard, e bem feito**.

```
                 Wi-Fi / LAN
┌──────────────┐             ┌──────────────────┐
│   CELULAR    │             │   ARCH LINUX     │
│              │             │                  │
│ Android App  │◄───────────►│ clipsyncd        │
│              │   WebSocket │                  │
│ Clipboard    │             │ Clipboard        │
│ Listener     │             │ Manager          │
└──────────────┘             └──────────────────┘
       │                              │
       ▼                              ▼
   Texto / imagem                wl-copy / xclip
```

## Status

| Componente       | Estado      |
|------------------|-------------|
| `clipsyncd` (Rust daemon) | v0.1 — texto + imagem via `wl-copy`/`wl-paste` |
| Descoberta mDNS  | ✅ `_clipsync._tcp.local` |
| Pareamento por PIN | ✅ 6 dígitos, mostrado no stdout/tray |
| Múltiplos devices | ✅ |
| App Android      | ⏳ planejado (Kotlin + Compose) |
| Criptografia E2E | ⏳ planejado para v0.2 |
| Sincronia de arquivos | ⏳ planejado para v0.3 |

## Arquitetura

O projeto é um Cargo workspace:

```
clipsync/
├── crates/
│   ├── clipsync-core/   # biblioteca: protocolo, WS, mDNS, clipboard, pairing
│   └── clipsyncd/       # binário do daemon (CLI + tray)
├── android/             # futuro app Android
├── docs/                # arquitetura, protocolo
└── .github/workflows/   # CI
```

Docs detalhados: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md),
[`docs/PROTOCOL.md`](docs/PROTOCOL.md) e
[`docs/ANDROID.md`](docs/ANDROID.md) (guia do app).

### Como funciona (clipboard bidirecional)

1. **PC → Android**
   ```
   wl-paste --watch detecta mudança local
        ↓
   clipsyncd envia via WebSocket para peers conectados
        ↓
   App Android recebe e escreve no ClipboardManager
   ```

2. **Android → PC**
   ```
   App Android detecta mudança no clipboard (foreground/IME)
        ↓
   WebSocket → clipsyncd
        ↓
   wl-copy escreve no clipboard do PC
   ```

3. **Anti-eco**
   Quando o daemon escreve via `wl-copy`, ele marca o evento como
   "remoto" por alguns ms, evitando reenviar de volta para o Android.

### Descoberta

O daemon anuncia via mDNS em `_clipsync._tcp.local`. O app Android
faz browse nesse serviço e mostra o PC na lista — sem precisar
digitar IP.

### Pareamento

Ao se conectar pela primeira vez, o celular vê um **PIN de 6 dígitos**
gerado pelo daemon. Você digita no app, e o device é adicionado à lista
de pares confiáveis (persistido em `~/.config/clipsync/trusted.toml`).
Conexões subsequentes do mesmo device são aceitas direto.

## Instalação (daemon)

### Arch Linux

```bash
git clone https://github.com/luis-ota/clipsync.git
cd clipsync
cargo install --path crates/clipsyncd
systemctl --user enable --now clipsyncd.service   # unit abaixo
```

### Dependências de runtime (Wayland)

```bash
sudo pacman -S wl-clipboard
```

Em X11, o daemon usa `xclip` automaticamente:

```bash
sudo pacman -S xclip
```

## Uso

```bash
# Rodar em foreground (stdout com logs e PIN)
clipsyncd run

# Rodar em background com config customizada
clipsyncd run --config ~/.config/clipsync/config.toml

# Gerar unit do systemd --user
clipsyncd service-install

# Mostrar PIN atual
clipsyncd show-pin

# Listar devices pareados
clipsyncd list-peers

# Remover um peer
clipsyncd untrust <device-id>

# Mostrar endereço de descoberta
clipsyncd show-address
```

## Configuração

`~/.config/clipsync/config.toml`:

```toml
[server]
bind = "0.0.0.0:8765"
name = "luis-arch"

[discovery]
enable_mdns = true
service_type = "_clipsync._tcp.local"

[clipboard]
# Tipos sincronizados
sync_text = true
sync_images = true
sync_html = false          # v0.2
sync_files = false         # v0.3
# Backend preferido: "wayland" | "x11" | "auto"
backend = "auto"
# Limite de tamanho (bytes) para imagens
max_image_bytes = 25_000_000

[security]
# Aceita apenas endereços locais (privados, loopback ou link-local).
# Não verifica SSID nem prova a mesma sub-rede.
local_only = true
# Expira o desafio e a conexão de pareamento após N segundos
pairing_timeout_secs = 120
```

## Protocolo

Veja [`docs/PROTOCOL.md`](docs/PROTOCOL.md) para a especificação completa
do protocolo WebSocket/JSON usado entre daemon e app.

Mensagens v0.1:

| Tipo               | Direção         | Descrição                          |
|--------------------|-----------------|------------------------------------|
| `hello`            | cliente → server | identifica o device                |
| `pair_challenge`   | server → cliente | envia PIN de 6 dígitos             |
| `pair_submit`      | cliente → server | submete PIN                        |
| `pair_ok`          | server → cliente | pareamento confirmado              |
| `pair_fail`        | server → cliente | PIN incorreto / expirado           |
| `clipboard_text`   | ↔                | texto/plain                        |
| `clipboard_image`  | ↔                | image/png ou image/jpeg            |
| `ping` / `pong`    | ↔                | keepalive                          |

## Roadmap

- [ ] v0.1 — daemon Rust funcional (texto + imagem, mDNS, pairing)
- [ ] v0.2 — criptografia E2E (Noise / AES-GCM), HTML rich text
- [ ] v0.3 — transferência de arquivos, múltiplos PCs
- [ ] v0.4 — app Android (Kotlin + Compose), foreground service
- [ ] v0.5 — IME customizado para captura em background

## Contribuindo

PRs são bem-vindos. Veja [`CONTRIBUTING.md`](CONTRIBUTING.md) (em breve).

## Licença

MIT OR Apache-2.0 — escolha a que preferir.

```
Copyright 2026 Luis Ota

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0
```
