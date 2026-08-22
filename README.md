# clipsync

> **Clipboard universal entre Android, Linux, macOS e Windows via LAN ou relay opcional.**
> O relay é self-hosted; não há serviço cloud obrigatório.

`clipsync` sincroniza o clipboard entre o seu celular Android e o seu PC Linux,
macOS ou Windows pela rede local. O daemon Linux suporta Wayland e X11; os
clientes macOS/Windows usam as APIs nativas do sistema. Copiou no celular →
cola no PC.
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
   Texto / imagem                wl-copy / xclip (-t MIME)
```

## Status

| Componente       | Estado      |
|------------------|-------------|
| `clipsyncd` (Rust daemon) | v0.1 — texto + imagem via `wl-copy`/`wl-paste`/`xclip` |
| Descoberta mDNS  | ✅ `_clipsync._tcp.local` |
| Pareamento por PIN | ✅ 6 dígitos, mostrado no stdout/tray |
| Múltiplos devices | ✅ |
| App Android      | ✅ v0.1 — Kotlin + Compose, texto + imagem |
| Cliente macOS/Windows | 🧪 cliente Rust mínimo oficial, texto + clipboard nativo |
| Criptografia E2E | ✅ AES-256-GCM para payloads relay |
| Sincronia de arquivos | ⏳ planejado para v0.3 |

## Arquitetura

O projeto é um Cargo workspace:

```
clipsync/
├── crates/
│   ├── clipsync-core/   # biblioteca: protocolo, WS, mDNS, clipboard, pairing
│   ├── clipsyncd/       # binário do daemon (CLI + tray)
│   └── clipsync-harness/# client de referência e testes manuais
├── android/             # app Android (Kotlin + Compose)
├── crates/clipsync-client/ # cliente desktop macOS/Windows (texto)
├── docs/                # arquitetura, protocolo
└── .github/workflows/   # CI
```

Docs detalhados: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md),
[`docs/PROTOCOL.md`](docs/PROTOCOL.md) e
[`docs/ANDROID.md`](docs/ANDROID.md) (guia do app).

Cliente desktop: [`docs/DESKTOP-CLIENTS.md`](docs/DESKTOP-CLIENTS.md).

Deploy operacional do `clipsyncd` headless: [`docs/DEPLOY.md`](docs/DEPLOY.md).

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

Ao se conectar pela primeira vez, o daemon exibe um **PIN de 6 dígitos**
no tray ou log. Você o digita no app, e o device é adicionado à lista
de pares confiáveis (persistido em `~/.config/clipsync/trusted.toml`).
Conexões subsequentes do mesmo device são aceitas direto. O daemon mantém um
único desafio ativo, de acordo com o único PIN apresentado no tray.

## Linux suportado

O daemon é headless-first e não finge oferecer uma GUI: a bandeja é opcional e
depende de D-Bus/SNI. A sincronização funciona com Wayland, X11 ou sem display
(modo headless, útil para relay e CI).

| Distribuição | Pacotes de clipboard | Backend |
|---|---|---|
| Debian 12 / Ubuntu 22.04+ | `wl-clipboard`, `xclip` | Wayland / X11 |
| Fedora 40+ | `wl-clipboard`, `xclip` | Wayland / X11 |
| Arch Linux | `wl-clipboard`, `xclip` | Wayland / X11 |

Instale `cargo`/Rust pelo método oficial da distribuição ou `rustup`. O CI
testa as quatro famílias sem depender de desktop ou Android.

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

Em Debian/Ubuntu use `sudo apt install wl-clipboard xclip`; em Fedora use
`sudo dnf install wl-clipboard xclip`.

O backend X11 consulta os `TARGETS` do clipboard e lê/escreve `image/png` ou
`image/jpeg` com `xclip -target <mime>`. O backend não converte imagens em
texto. `DISPLAY` e uma sessão X11 acessível pelo usuário do daemon são
requisitos em runtime; sem eles o daemon permanece em modo headless e o
relay de rede continua disponível. O limite padrão de imagem é 25 MiB.

## Uso

### macOS e Windows (cliente mínimo)

O cliente oficial usa o clipboard nativo via `arboard` e sincroniza texto.
Ele não oferece tray, descoberta mDNS, imagens ou persistência de identidade
nesta primeira entrega. O PIN é informado explicitamente, e cada execução sem
uma identidade persistida exige novo pareamento.

```bash
cargo run -p clipsync-client -- --pin 834921 ws://192.168.1.50:8765/ws
```

O binário é compilado e testado em runners macOS e Windows no workflow
`desktop-clients`. O ambiente Linux deste repositório não afirma ter validado
esses binários nativamente; a validação real depende do CI desses runners.

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

# Remover um peer (operação offline; pare o daemon antes)
clipsyncd untrust <device-id>

# Mostrar endereço de descoberta
clipsyncd show-address

# Gerenciar conexões outbound Linux
clipsyncd endpoints add relay wss://relay.example.invalid/ws --scope relay \
  --tls-fingerprint <sha256-der> --credential-ref CLIPSYNC_RELAY_TOKEN
clipsyncd endpoints list
clipsyncd endpoints test relay
clipsyncd endpoints remove relay
```

As gravações de `config.toml` e `trusted.toml` são atômicas. Enquanto está
ativo, o daemon possui exclusivamente o trusted store; uma tentativa offline
de `untrust` falha em vez de competir com o estado em memória.

## Configuração

`~/.config/clipsync/config.toml`:

```toml
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
max_text_bytes = 16_777_216

[limits]
max_connections = 256
messages_per_minute = 120
bytes_per_minute = 67_108_864

[security]
# Aceita apenas endereços locais (privados, loopback ou link-local).
# Não verifica SSID nem prova a mesma sub-rede.
local_only = true
# Expira o desafio e a conexão de pareamento após N segundos
pairing_timeout_secs = 120
# TLS é obrigatório por padrão; plaintext_legacy só para rede privada/proxy.
transport = "tls"
# Seleção outbound: "lan", "relay" ou "auto" (LAN primeiro)
outbound_route = "auto"
```

### Relay outbound

Um endpoint `relay` usa `wss://.../ws`, bearer no header `Authorization` e
pin SHA-256 do certificado DER. `credential_ref` é apenas uma referência: por
exemplo `CLIPSYNC_RELAY_TOKEN` lê a variável de ambiente, e
`file:/etc/clipsync/relay.token` lê um arquivo que deve ser `0600`. O token não
é salvo no TOML nem na URL. Em `auto`, endpoints LAN são tentados antes dos
relays; uma queda reconecta usando o próximo endpoint configurado.

O token relay precisa ser provisionado para o mesmo `device_id` persistido no
TOML. O relay fornece transporte TLS hop-to-hop, não E2E contra o operador do
relay. A rotação de certificado exige atualizar o pin antes de reconectar.

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
- [x] v0.4 — app Android (Kotlin + Compose), foreground service
- [x] v0.5 — IME para insercao explicita do ultimo clipboard remoto

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
