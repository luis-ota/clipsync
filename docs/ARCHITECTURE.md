# Arquitetura do clipsync

## Visão geral

Sincronização bidirecional de clipboard entre **Android** e **Linux**
(Arch) via LAN. Zero nuvem, zero servidor externo: o daemon roda no PC
do usuário e os apps Android conectam via WebSocket.

```
┌───────────────────────┐        LAN        ┌───────────────────────┐
│  Android (app)        │ ◄──── WebSocket ──► │  Arch Linux (daemon) │
│  · clipboard service  │      wss://:8765   │  · clipsyncd          │
│  · NsdManager browse  │                    │  · wl-copy/wl-paste  │
│  · WebSocket client   │   mDNS discovery   │  · mDNS announce      │
└───────────────────────┘   _clipsync._tcp   └───────────────────────┘
```

## Crate `clipsync-core`

Biblioteca pura (sem binário) com toda a lógica de protocolo e
servidor, separada do binário para permitir testes unitários e
reuso pelo futuro client desktop.

```
crates/clipsync-core/src/
├── lib.rs        Re-exports, constantes (PROTOCOL_VERSION, SERVICE_TYPE)
├── error.rs      Erros tipados (Error enum)
├── protocol.rs   Message, DeviceId, DeviceInfo, Capabilities, serde
├── clipboard.rs  ClipboardManager e contratos comuns
├── clipboard/
│   ├── watch.rs  polling/eventos e debounce
│   └── x11.rs    backend X11 via xclip com MIME explícito
├── config.rs     Config em TOML (server/discovery/clipboard/security)
├── discovery.rs  mDNS announce/browse (mdns-sd crate)
├── pairing.rs    PairingManager (PIN, trusted devices, tokens)
├── peer.rs       PeerSession / PeerHandle (conexão de um device)
├── state.rs      ServerState (peers ativos, broadcast, ping/idle)
├── transport.rs  Connection (handshake hello/pair, message loop)
└── server.rs     Server (TcpListener, ws_handler, healthz, shutdown)
```

### Fluxo de dados

```
[Clipboard local]                              [Peers remotos]
      │                                               │
      ▼ watch()                                     /        \
[ClipboardEvent]                     [Peer 1]   [Peer 2]   [Peer N]
      │                                    \         |         /
      ▼                                      \        |        /
[ServerState::broadcast]                      ▶ PeerHandle ◀
      │  (exclui origin da mensagem)              │
      ▼                                           ▼
[peers do state]                          [send(Message)]
```

### Anti-eco

O daemon não reenvia o clipboard para quem originou a mensagem
(checagem por `origin`). O `ClipboardManager` grava por subprocesso
(`wl-copy`), e o watcher compara o sha256 do conteúdo lido com o
último escrito por ele mesmo (`last_self_write`) para não re-emitir
o que acabou de ser sincronizado de um peer.

Wayland usa `wl-copy`/`wl-paste`. X11 consulta `TARGETS` com `xclip` e
seleciona explicitamente o MIME (`image/png`, `image/jpeg` ou
`UTF8_STRING` para texto), evitando que uma imagem seja lida como texto.
O backend X11 requer `xclip` e uma sessão acessível via `DISPLAY`; os
testes do backend são puros e não precisam de display.

### Segurança

- Pareamento: PIN de 6 dígitos (`PairingManager`). O device envia
  `hello`; se desconhecido, o servidor gera um `PairChallenge` ligado ao
  `session_id` desta conexão e espera `PairSubmit`. PIN correto → `pair_ok`
  + `device_id` persistido pelo client. Há no máximo um desafio global, como
  há um único PIN no tray; um novo pedido invalida o anterior. O nome do
  device é apenas metadata.
- `trusted_devices_path()` → `~/.config/clipsync/trusted.toml`.
- `config.toml` e `trusted.toml` são publicados por escrita temporária,
  `fsync`, rename atômico e `fsync` do diretório.
- O daemon mantém ownership exclusivo de `trusted.toml` por lock
  interprocesso. `untrust` opera offline e recusa a mutação enquanto o daemon
  estiver ativo, evitando divergência entre disco e memória.
- A criptografia por mensagem (AES-GCM) é planejada para v0.2.

## Crate `clipsyncd`

Binário do daemon desktop. CLI com subcomandos:

```
clipsyncd                 # roda o daemon (default)
clipsyncd run
clipsyncd show-pin        # orienta como consultar o PIN no daemon/tray
clipsyncd list-peers      # lista devices confiados
clipsyncd untrust <id>    # remove um device (com o daemon parado)
clipsyncd show-address    # mostra IP:porta para o app
clipsyncd service-install # instala unit systemd de usuário
clipsyncd service-uninstall
```

Config em `~/.config/clipsync/config.toml`:

```toml
[server]
bind = "0.0.0.0:8765"
name = "luis-arch"

[discovery]
enabled = true

[clipboard]
max_image_bytes = 26214400  # 25 MB

[security]
local_only = true
pairing_timeout_secs = 120
```

## Testes

- `cargo test --workspace` — 22 testes (protocolo, pairing, state,
  healthz, broadcast).
- CI (`.github/workflows/ci.yml`): `fmt` + `clippy -D warnings` +
  `build` + `test` no Ubuntu.

## Roadmap

| Versão | Escopo |
|--------|--------|
| v0.1   | Texto + imagens inline (base64), pairing PIN, mDNS, daemon CLI |
| v1     | TLS autoassinado com pinning mDNS, rich text (HTML) |
| v0.3   | Frames binários para arquivos grandes, sincronização de arquivos |
| v1.0   | Android no F-Droid, notification actions, multi-display |
